// 语音相关工具。
use uuid::Uuid;

/// 安装内稳定的随机串（百度 ASR cuid）。
pub fn new_cuid() -> String {
    Uuid::new_v4().simple().to_string()
}
