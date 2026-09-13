// 无界面冒烟测试：直接验证 LLM 与 TTS 两条链路是否真的可用。
// 运行: cd src-tauri && cargo run --example smoke
use ovoice_lib::{llm, tts};

// 简单的测试用 Emitter（把事件打印到控制台）
struct TestEmitter;
#[async_trait::async_trait]
impl llm::Emitter for TestEmitter {
    async fn thinking(&self, text: &str) {
        println!("  [thinking] {}", if text.len() > 50 { &text[..50] } else { text });
    }
    async fn content(&self, text: &str) {
        println!("  [content] {}", if text.len() > 50 { &text[..50] } else { text });
    }
    async fn tool_call(&self, name: &str, args: &str) {
        println!("  [tool call] {}({})", name, if args.len() > 60 { &args[..60] } else { args });
    }
    async fn tool_result(&self, name: &str, result: &str) {
        println!("  [tool result] {} -> {}", name, if result.len() > 60 { &result[..60] } else { result });
    }
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let cfg = ovoice_lib::config::Config::default();
    let workspace = std::path::PathBuf::from(&cfg.workspace_dir);
    let _ = std::fs::create_dir_all(&workspace);

    println!("== 1) LLM 测试 (MiniMax-M3 with tool loop) ==");
    let resp = llm::run_loop(
        llm::HttpRound,
        TestEmitter,
        &cfg,
        vec![serde_json::json!({
            "role": "user",
            "content": "用一句话介绍你自己。"
        })],
        &workspace,
    ).await;

    if let Some(e) = &resp.error {
        println!("❌ LLM 失败: {e}");
    } else {
        println!("✅ LLM 回复: {}", resp.content);
        println!("   (history: {} 条消息)", resp.history.len());
    }

    println!("\n== 2) TTS 测试 (speech-2.8-hd) ==");
    match tts::synthesize("你好，这是 MiniMax 语音合成的冒烟测试。".into(), &cfg).await {
        Ok(audio) => {
            let path = format!("smoke_output.{}", audio.format);
            std::fs::write(&path, &audio.bytes).expect("写入音频文件失败");
            println!(
                "✅ TTS 成功: {} 字节, format={}, 已保存 {}",
                audio.bytes.len(),
                audio.format,
                path
            );
        }
        Err(e) => println!("❌ TTS 失败: {e}"),
    }
}
