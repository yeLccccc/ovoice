use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use async_trait::async_trait;
use serde::Deserialize;
use super::errors::BaiduError;

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: u64,
}

#[async_trait]
pub trait TokenFetcher: Send + Sync {
    async fn fetch(&self) -> Result<TokenResponse, BaiduError>;
}

struct CachedToken {
    token: String,
    expires_at: u64, // unix 秒
}

const REFRESH_MARGIN_SECS: u64 = 3600; // 剩余不足 1 小时则刷新

pub struct TokenProvider {
    fetcher: Arc<dyn TokenFetcher>,
    cache: Mutex<Option<CachedToken>>,
}

impl TokenProvider {
    pub fn new(fetcher: Arc<dyn TokenFetcher>) -> Self {
        Self { fetcher, cache: Mutex::new(None) }
    }

    pub async fn get_token(&self) -> Result<String, BaiduError> {
        let now = unix_now();
        {
            let c = self.cache.lock().unwrap();
            if let Some(t) = &*c {
                if t.expires_at > now + REFRESH_MARGIN_SECS {
                    return Ok(t.token.clone());
                }
            }
        }
        let resp = self.fetcher.fetch().await?;
        let expires_at = now + resp.expires_in.saturating_sub(REFRESH_MARGIN_SECS);
        let mut c = self.cache.lock().unwrap();
        *c = Some(CachedToken { token: resp.access_token.clone(), expires_at });
        Ok(resp.access_token)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before 1970; cannot compute token expiry")
        .as_secs()
}

// ---- 测试桩 ----
pub struct MockTokenFetcher {
    pub calls: Mutex<u32>,
    pub token: String,
    pub expires_in: u64,
}

#[async_trait]
impl TokenFetcher for MockTokenFetcher {
    async fn fetch(&self) -> Result<TokenResponse, BaiduError> {
        *self.calls.lock().unwrap() += 1;
        Ok(TokenResponse { access_token: self.token.clone(), expires_in: self.expires_in })
    }
}

// ---- 真实网络 fetcher ----
pub struct HttpTokenFetcher {
    api_key: String,
    secret_key: String,
}

impl HttpTokenFetcher {
    pub fn new(api_key: String, secret_key: String) -> Self {
        Self { api_key, secret_key }
    }
}

#[async_trait]
impl TokenFetcher for HttpTokenFetcher {
    async fn fetch(&self) -> Result<TokenResponse, BaiduError> {
        let url = format!(
            "https://aip.baidubce.com/oauth/2.0/token?grant_type=client_credentials&client_id={}&client_secret={}",
            self.api_key, self.secret_key
        );
        let resp = reqwest::get(&url).await.map_err(|e| BaiduError::Http(e.to_string()))?;
        let tr: TokenResponse = resp.json().await.map_err(|e| BaiduError::Parse(e.to_string()))?;
        if tr.access_token.is_empty() {
            return Err(BaiduError::Auth { code: -1, message: "返回的 access_token 为空".into() });
        }
        Ok(tr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(expires_in: u64) -> (TokenProvider, Arc<MockTokenFetcher>) {
        let m = Arc::new(MockTokenFetcher { calls: Mutex::new(0), token: "tok-1".into(), expires_in });
        (TokenProvider::new(m.clone()), m)
    }

    #[tokio::test]
    async fn caches_token_and_skips_refetch() {
        let (p, m) = provider(86400);
        let t1 = p.get_token().await.unwrap();
        let t2 = p.get_token().await.unwrap();
        assert_eq!(t1, "tok-1");
        assert_eq!(t2, "tok-1");
        assert_eq!(*m.calls.lock().unwrap(), 1, "应只 fetch 一次");
    }

    #[tokio::test]
    async fn refreshes_each_call_when_expired() {
        let m = Arc::new(MockTokenFetcher { calls: Mutex::new(0), token: "tok-1".into(), expires_in: 0 });
        let p = TokenProvider::new(m.clone());
        p.get_token().await.unwrap();
        p.get_token().await.unwrap();
        assert_eq!(*m.calls.lock().unwrap(), 2, "过期则每次都 fetch");
    }

    #[tokio::test]
    async fn refreshes_when_within_margin() {
        let (p, m) = provider(100);
        p.get_token().await.unwrap();
        p.get_token().await.unwrap();
        assert_eq!(*m.calls.lock().unwrap(), 2, "当 expires_in <= margin 时应每次 fetch");
    }
}
