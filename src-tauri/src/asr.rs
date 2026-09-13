// 百度 ASR 运行时状态 + 一次性识别命令。
use std::sync::Arc;
use tauri::{AppHandle, State};
use tokio::sync::Mutex;

use crate::audio::new_cuid;
use crate::baidu::asr::recognize_short;
use crate::baidu::auth::{HttpTokenFetcher, TokenProvider};
use crate::config::Config;

/// 全局状态：HTTP 客户端 + 按「凭证签名」缓存的 token provider。
/// Clone 廉价（reqwest::Client 与 cache 均为 Arc），可同时交给
/// managed 状态与后台「全局推说话」任务共用。
#[derive(Clone)]
pub struct AsrState {
    client: reqwest::Client,
    cache: Arc<Mutex<Option<(String, Arc<TokenProvider>)>>>,
}

impl Default for AsrState {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
            cache: Arc::new(Mutex::new(None)),
        }
    }
}

impl AsrState {
    /// 取 access_token；凭证变更时自动重建 provider（保留有效 token 的缓存与自动刷新）。
    async fn token(&self, cfg: &Config) -> Result<String, String> {
        if cfg.baidu_api_key.trim().is_empty() || cfg.baidu_secret_key.trim().is_empty() {
            return Err("未配置百度语音凭证（请在「设置」填写 AppID / API Key / Secret Key）".into());
        }
        let sig = format!(
            "{}|{}|{}",
            cfg.baidu_app_id, cfg.baidu_api_key, cfg.baidu_secret_key
        );

        {
            let g = self.cache.lock().await;
            if let Some((s, p)) = &*g {
                if *s == sig {
                    return p.get_token().await.map_err(|e| e.to_string());
                }
            }
        }

        let fetcher = Arc::new(HttpTokenFetcher::new(
            cfg.baidu_api_key.clone(),
            cfg.baidu_secret_key.clone(),
        ));
        let provider = Arc::new(TokenProvider::new(fetcher));
        let token = provider.get_token().await.map_err(|e| e.to_string())?;
        *self.cache.lock().await = Some((sig, provider));
        Ok(token)
    }

    /// 把一段 16k/16bit/单声道 PCM 送去百度识别，返回文本。
    /// 麦克风命令与全局推说话两条路径都走这里，复用同一套 token 缓存。
    pub async fn recognize(&self, app: &AppHandle, bytes: Vec<u8>) -> Result<String, String> {
        if bytes.is_empty() {
            return Err("录音为空".into());
        }
        let cfg = crate::config::load(app);
        let token = self.token(&cfg).await?;
        let cuid = new_cuid();
        recognize_short(&self.client, &token, &bytes, &cuid, cfg.baidu_dev_pid)
            .await
            .map_err(|e| e.to_string())
    }
}

/// 麦克风一次性识别：前端采好 16k/16bit/单声道 PCM 字节后整体传入。
#[tauri::command]
pub async fn asr_one_shot(
    app: AppHandle,
    state: State<'_, AsrState>,
    bytes: Vec<u8>,
) -> Result<String, String> {
    state.recognize(&app, bytes).await
}
