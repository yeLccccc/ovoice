// 百度 ASR 端到端冒烟测试：解码一个 wav -> 取 token -> recognize_short。
// 运行: cargo run --example asr_smoke [path/to.wav]
use ovoice_lib::audio::new_cuid;
use ovoice_lib::baidu::asr::recognize_short;
use ovoice_lib::baidu::auth::{HttpTokenFetcher, TokenProvider};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let wav_path = std::env::args()
        .nth(1)
        // 音频路径从参数/环境变量取，缺省落到当前目录（不携带个人路径）
        .unwrap_or_else(|| "debug_capture.wav".into());
    println!("解码 wav: {wav_path}");

    let reader = match hound::WavReader::open(&wav_path) {
        Ok(r) => r,
        Err(e) => {
            println!("❌ 打不开 wav ({wav_path}): {e}");
            return;
        }
    };
    let spec = reader.spec();
    println!("wav 规格: {spec:?}");
    let samples: Vec<i16> = match reader.into_samples::<i16>().collect::<Result<Vec<_>, _>>() {
        Ok(s) => s,
        Err(e) => {
            println!("❌ 读样本失败: {e}");
            return;
        }
    };
    let n = samples.len();
    let mut bytes = Vec::with_capacity(n * 2);
    for s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    println!(
        "PCM: {} 字节 ({} 样本, {:.1}s)",
        bytes.len(),
        n,
        n as f64 / 16000.0
    );

    // 凭据不入库：从环境变量读（开源仓库不携带密钥）。
    let api_key = std::env::var("BAIDU_API_KEY").expect("设置 BAIDU_API_KEY 环境变量后再跑本例");
    let secret = std::env::var("BAIDU_SECRET_KEY").expect("设置 BAIDU_SECRET_KEY 环境变量后再跑本例");
    let dev_pid = 1537i64;

    let fetcher = Arc::new(HttpTokenFetcher::new(api_key.into(), secret.into()));
    let provider = TokenProvider::new(fetcher);
    let token = match provider.get_token().await {
        Ok(t) => {
            let head = &t[..20.min(t.len())];
            println!("✅ access_token: {head}…");
            t
        }
        Err(e) => {
            println!("❌ token 获取失败: {e}");
            return;
        }
    };

    let cuid = new_cuid();
    let client = reqwest::Client::new();
    match recognize_short(&client, &token, &bytes, &cuid, dev_pid).await {
        Ok(text) => println!("✅ ASR 识别结果: {text:?}"),
        Err(e) => println!("❌ ASR 失败: {e}"),
    }
}
