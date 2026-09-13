use thiserror::Error;

#[derive(Debug, Error)]
pub enum BaiduError {
    #[error("网络请求失败: {0}")]
    Http(String),
    #[error("解析响应失败: {0}")]
    Parse(String),
    #[error("鉴权失败 (百度): code={code} {message}")]
    Auth { code: i64, message: String },
    #[error("百度返回错误: code={code} {message}")]
    Api { code: i64, message: String },
    #[error("内部错误: {0}")]
    Internal(String),
}

impl serde::Serialize for BaiduError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

/// 短语音 server_api err_no 友好映射
pub fn friendly_short_msg(err_no: i64) -> &'static str {
    match err_no {
        3300 => "请求参数缺失",
        3301 => "音频质量差（识别错误）",
        3302 => "鉴权失败：token 或 appkey 无效",
        3303 => "语音后端识别错误：音频时长过短",
        3304 => "鉴权失败：请检查 API Key/Secret Key 或额度",
        3305 => "请求超配额（QPS 或频次限制）",
        3307 => "服务端内部错误（请联系百度）",
        3308 => "音频过长（请压缩到 60 秒内）",
        3309 => "音频数据错误",
        _ => "未知百度错误",
    }
}
