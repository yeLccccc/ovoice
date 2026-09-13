use base64::{engine::general_purpose, Engine};
use serde::{Deserialize, Serialize};
use super::errors::{friendly_short_msg, BaiduError};

const SERVER_API: &str = "https://vop.baidu.com/server_api";

#[derive(Debug, Serialize)]
pub struct ShortAsrRequest {
    pub format: String,
    pub rate: u32,
    pub channel: u16,
    pub cuid: String,
    pub token: String,
    pub speech: String, // base64
    pub len: u64,       // 原始字节数
    pub dev_pid: i64,
}

impl ShortAsrRequest {
    pub fn new(pcm: &[u8], token: &str, cuid: &str, dev_pid: i64) -> Self {
        Self {
            format: "pcm".into(),
            rate: 16000,
            channel: 1,
            cuid: cuid.into(),
            token: token.into(),
            speech: general_purpose::STANDARD.encode(pcm),
            len: pcm.len() as u64,
            dev_pid,
        }
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap()
    }
}

#[derive(Debug, Deserialize)]
struct ShortAsrResponse {
    err_no: i64,
    err_msg: Option<String>,
    result: Option<Vec<String>>,
}

pub fn parse_short_response(body: &str) -> Result<String, BaiduError> {
    let r: ShortAsrResponse = serde_json::from_str(body)
        .map_err(|e| BaiduError::Parse(e.to_string()))?;
    if r.err_no != 0 {
        return Err(BaiduError::Api {
            code: r.err_no,
            message: format!("{}（{}）", friendly_short_msg(r.err_no), r.err_msg.unwrap_or_default()),
        });
    }
    let text = r.result.unwrap_or_default().join("");
    Ok(text)
}

pub async fn recognize_short(
    client: &reqwest::Client,
    token: &str,
    pcm: &[u8],
    cuid: &str,
    dev_pid: i64,
) -> Result<String, BaiduError> {
    let body = ShortAsrRequest::new(pcm, token, cuid, dev_pid).to_json();
    let resp = client
        .post(SERVER_API)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| BaiduError::Http(e.to_string()))?;
    let text = resp.text().await.map_err(|e| BaiduError::Http(e.to_string()))?;
    parse_short_response(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_has_correct_fields_and_len() {
        let pcm = vec![1u8, 2, 3, 4];
        let req = ShortAsrRequest::new(&pcm, "TOK", "cuid-1", 1537);
        let json = req.to_json();
        assert!(json.contains("\"format\":\"pcm\""));
        assert!(json.contains("\"rate\":16000"));
        assert!(json.contains("\"channel\":1"));
        assert!(json.contains("\"token\":\"TOK\""));
        assert!(json.contains("\"len\":4"));
        assert!(json.contains("\"dev_pid\":1537"));
        assert!(json.contains(r#""cuid":"cuid-1""#));
        assert!(json.contains(r#""speech":"AQIDBA==""#)); // base64 of [1,2,3,4]
    }

    #[test]
    fn parses_success_result() {
        let body = r#"{"err_no":0,"err_msg":"success.","result":["你好世界"]}"#;
        assert_eq!(parse_short_response(body).unwrap(), "你好世界");
    }

    #[test]
    fn joins_multi_result() {
        let body = r#"{"err_no":0,"err_msg":"success.","result":["你好","世界"]}"#;
        assert_eq!(parse_short_response(body).unwrap(), "你好世界");
    }

    #[test]
    fn maps_error_code() {
        let body = r#"{"err_no":3301,"err_msg":"audio quality error.","result":[]}"#;
        let e = parse_short_response(body).unwrap_err();
        let msg = format!("{e}");
        assert!(msg.contains("3301"));
        assert!(msg.contains("音频质量差"));
    }
}
