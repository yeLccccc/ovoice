// MiniMax 同步语音合成 —— WebSocket (t2a_v2)。
// 流程: connect -> connected_success -> task_start -> task_started
//       -> task_continue(text) -> 多段 hex 音频直到 is_final -> task_finish
use crate::config::Config;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        client::IntoClientRequest,
        http::{header::AUTHORIZATION, HeaderValue},
        Message,
    },
};

const WS_URL: &str = "wss://api.minimaxi.com/ws/v1/t2a_v2";

pub struct AudioOut {
    pub bytes: Vec<u8>,
    pub format: String,
}

pub async fn synthesize(text: String, cfg: &Config) -> Result<AudioOut, String> {
    let key = crate::llm::api_key(cfg)?;
    if text.trim().is_empty() {
        return Err("待合成的文本为空".into());
    }

    // 让 tungstenite 自动补全 WebSocket 握手头，再插入 Authorization。
    let mut request = WS_URL
        .into_client_request()
        .map_err(|e| format!("构造握手请求失败: {e}"))?;
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|e| format!("非法 API Key: {e}"))?,
    );

    let (ws_stream, _resp) = connect_async(request)
        .await
        .map_err(|e| format!("WebSocket 连接失败: {e}"))?;

    let (mut write, mut read) = ws_stream.split();

    // 1) 等待 connected_success
    expect_event(&mut read, "connected_success").await?;

    // 2) 发送 task_start（参数全部来自配置）
    let start_msg = json!({
        "event": "task_start",
        "model": cfg.tts_model,
        "voice_setting": {
            "voice_id": cfg.voice_id,
            "speed": cfg.speed,
            "vol": cfg.vol,
            "pitch": cfg.pitch,
            "english_normalization": false
        },
        "audio_setting": {
            "sample_rate": 32000,
            "bitrate": 128000,
            "format": cfg.audio_format,
            "channel": 1
        }
    });
    send_json(&mut write, start_msg).await?;
    expect_event(&mut read, "task_started").await?;

    // 3) 发送 task_continue + 文本
    send_json(&mut write, json!({ "event": "task_continue", "text": text })).await?;

    // 4) 收集 hex 音频直到 is_final
    let mut audio: Vec<u8> = Vec::new();
    loop {
        let msg = read
            .next()
            .await
            .ok_or_else(|| "WebSocket 流提前关闭".to_string())?
            .map_err(|e| format!("WebSocket 接收失败: {e}"))?;

        let payload = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => {
                audio.extend_from_slice(&b);
                continue;
            }
            Message::Close(_) => return Err("WebSocket 被关闭".into()),
            _ => continue,
        };

        let v: Value = match serde_json::from_str(&payload) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(hex_audio) = v["data"]["audio"].as_str() {
            if !hex_audio.is_empty() {
                let bytes = hex::decode(hex_audio)
                    .map_err(|e| format!("hex 解码失败: {e}"))?;
                audio.extend_from_slice(&bytes);
            }
        }

        if v.get("is_final").and_then(|f| f.as_bool()).unwrap_or(false) {
            break;
        }
    }

    // 5) 收尾
    let _ = send_json(&mut write, json!({ "event": "task_finish" })).await;
    let _ = write.close().await;

    if audio.is_empty() {
        return Err("未收到任何音频数据".into());
    }

    Ok(AudioOut {
        bytes: audio,
        format: cfg.audio_format.clone(),
    })
}

async fn send_json(
    write: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        Message,
    >,
    value: Value,
) -> Result<(), String> {
    write
        .send(Message::Text(value.to_string().into()))
        .await
        .map_err(|e| format!("WebSocket 发送失败: {e}"))
}

async fn expect_event<S>(read: &mut S, expected: &str) -> Result<(), String>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    for _ in 0..5 {
        let msg = read
            .next()
            .await
            .ok_or_else(|| format!("等待 {expected} 时连接关闭"))?
            .map_err(|e| format!("读取 {expected} 失败: {e}"))?;

        if let Message::Text(t) = msg {
            if let Ok(v) = serde_json::from_str::<Value>(&t.to_string()) {
                if v.get("event").and_then(|e| e.as_str()) == Some(expected) {
                    return Ok(());
                }
                // 收到错误事件
                if v.get("event").and_then(|e| e.as_str()).map(|s| s.contains("error")).unwrap_or(false)
                    || v.get("base_resp").is_some()
                {
                    return Err(format!("TTS 错误事件: {}", truncate(&t.to_string(), 400)));
                }
            }
        }
    }
    Err(format!("未收到期望事件: {expected}"))
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() > n {
        format!("{}…", &s[..n])
    } else {
        s.to_string()
    }
}
