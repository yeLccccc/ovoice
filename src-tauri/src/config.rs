// 应用配置：持久化到 app_data_dir/config.json。
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use std::path::{Path, PathBuf};

/// 音色候选（设置页下拉用，可扩展）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VoiceOption {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "d_llm_model")]
    pub llm_model: String,
    #[serde(default = "d_sys")]
    pub system_prompt: String,
    // 用户提示词前缀开关：开启后每条 user-role 消息发给 LLM 前 content 前拼 user_prompt_prefix。
    #[serde(default)]
    pub user_prompt_prefix_enabled: bool,
    // 用户提示词前缀文本（典型用途：做事与思考逻辑；每条 user-role 输入前拼接，落盘 data["prefix"]）。
    #[serde(default)]
    pub user_prompt_prefix: String,
    #[serde(default = "d_tts")]
    pub tts_model: String,
    #[serde(default = "d_voice")]
    pub voice_id: String,
    #[serde(default = "d_speed")]
    pub speed: f64,
    #[serde(default = "d_vol")]
    pub vol: f64,
    #[serde(default)]
    pub pitch: i32,
    #[serde(default = "d_fmt")]
    pub audio_format: String,
    // 百度语音识别 (ASR)
    #[serde(default = "d_baidu_appid")]
    pub baidu_app_id: i64,
    #[serde(default = "d_baidu_apikey")]
    pub baidu_api_key: String,
    #[serde(default = "d_baidu_secret")]
    pub baidu_secret_key: String,
    #[serde(default = "d_baidu_devpid")]
    pub baidu_dev_pid: i64,
    // 全局推说话：双击 Ctrl 后需「持续按住」多久（毫秒）才真正开始录音（防误触）。
    #[serde(default = "d_hold_gate")]
    pub hold_gate_ms: u64,
    // 全局推说话触发方式："hold"(双击Ctrl并按住、松开结束) | "toggle"(双击Ctrl开、再按一次停)。
    #[serde(default = "d_voice_trigger")]
    pub voice_trigger: String,
    // 语音识别完成后："send"(立即发送助手) | "insert"(填入输入框不发送)。
    #[serde(default = "d_voice_action")]
    pub voice_action: String,
    // Agent 工作目录（write/read/bash 相对路径根；用户文件。空 → Documents/ovoice）
    #[serde(default)]
    pub workspace_dir: String,
    // ovoice 内部缓存目录（memory/history/MEMORY.md/SOUL.md/AGENT.md/attachments/.ovoice-jobs；
    // 与 workspace 分离：cache 不跟用户目录走，避免云盘同步泄露原始对话。空 → app_data_dir/cache）
    #[serde(default)]
    pub cache_dir: String,
    // 额外媒体根目录（media:// 除 workspace/app_data 外额外允许服务的绝对路径；display_media/display_doc 展示范围）
    #[serde(default)]
    pub media_roots: Vec<String>,
    // 音色候选列表（设置页下拉；编辑 config.json 或设置页「自定义」可扩展）
    #[serde(default = "d_voices")]
    pub voices: Vec<VoiceOption>,
    // MiniMax 区域（cn 或 global；供 ToolsCtx 注入 bash env 基线）
    #[serde(default = "d_region")]
    pub minimax_region: String,
    // 单个附件大小上限（MB）：图片恒≤10（MiniMax 硬限制）；其余类型受此值约束，超限拒绝
    #[serde(default = "d_max_attachment_mb")]
    pub max_attachment_mb: u64,
    // 全局背景图：启用开关 / 图片磁盘路径(绝对) / 明暗遮罩(0=原图, 1=最淡)
    #[serde(default)]
    pub bg_enabled: bool,
    #[serde(default)]
    pub bg_path: String,
    #[serde(default = "d_bg_opacity")]
    pub bg_opacity: f64,
    // 玻璃面板/气泡不透明度（0=全透 1=不透）；前端注入 --glass-alpha
    #[serde(default = "d_glass_opacity")]
    pub glass_opacity: f64,
    // 运行时工具集覆盖：None=全量 tools::schemas()（主 session）；Some=子集（子代理）。不持久化。
    #[serde(skip)]
    pub active_tools: Option<Vec<serde_json::Value>>,
    // 子代理专用 system prompt（spawn 子代理时作 messages[0]）。
    #[serde(default = "d_subagent_sys")]
    pub subagent_system_prompt: String,
    // 子代理最大并发数（默认 4；可配置；0 = 禁用子代理）。
    #[serde(default = "d_max_subagents")]
    pub max_subagents: u64,
    // 子代理最大工具循环轮数（默认 5000，覆盖数小时/数千循环的长任务；0 = 不限制）。
    // 失控保护主保险丝是螺旋熔断（SPIRAL_LIMIT），这里只兜「缓慢漂移型失控」的底，因此给得宽、可调。
    #[serde(default = "d_subagent_max_iters")]
    pub subagent_max_iters: u64,
    // v2 上下文管理：dream 触发 + context 重建窗口（高级设置，默认无感）
    #[serde(default = "d_dream_idle")]
    pub dream_idle_secs: u64,
    // dream 触发条件①：当前 context input token ≥ 此值 → 触发 dream（token 触发，留最近 3 回合）。
    // 当前 context 量取最近一轮 main assistant 落盘的 usage.prompt_tokens（服务器真实计数）。
    #[serde(default = "d_dream_context_trigger_tokens")]
    pub dream_context_trigger_tokens: u64,
    // context 重建窗口：build_messages 保留最近多少个 kind=user 事件（context.rs::build_messages 的 cap）。
    // 【注意】此字段【不是】dream 触发条件（触发条件是上面的 token 阈值 + 下面的 idle_secs）；
    // 字段名 dream_cap_turns 沿用以免破坏既有 config.json，语义已变为「context 窗口」。
    #[serde(default = "d_dream_cap")]
    pub dream_cap_turns: u64,
    #[serde(default = "d_display_window")]
    pub display_window_size: u64,
    // 单轮工具调用迭代上限（run_turn 的 max_iters；超出 emit「已达工具调用上限」并结束 turn）。
    // 默认 100：复杂多步任务（连续 bash/read/write…）留足空间，避免过早截断、被迫续发拆成多段。
    #[serde(default = "d_max_tool_iters")]
    pub max_tool_iters: u64,
    // dream 完整性改造：留尾回合数（最近 N 个 user 回合不进 dream，下轮 build_messages 仍可见）。
    // dream 合并组上限：单个 LLM 分组最多覆盖多少回合；超出强制拆（F11）。
    #[serde(default = "d_dream_merge_max_rounds")]
    pub dream_merge_max_rounds: u64,
    // dream 一批提取的最大事件数（活动段边界对齐切批；防 LLM 长上下文失焦）。
    #[serde(default = "d_dream_batch_max_events")]
    pub dream_batch_max_events: u64,
}

fn d_dream_idle() -> u64 { 7200 }
fn d_dream_context_trigger_tokens() -> u64 { 300_000 }
fn d_dream_cap() -> u64 { 3 }
fn d_display_window() -> u64 { 50 }
fn d_max_tool_iters() -> u64 { 100 }
fn d_dream_merge_max_rounds() -> u64 { 5 }
fn d_dream_batch_max_events() -> u64 { 100 }

fn d_region() -> String { "cn".into() }
fn d_bg_opacity() -> f64 { 0.35 }
fn d_glass_opacity() -> f64 { 0.85 }
fn d_subagent_sys() -> String {
    // 提示词设计对齐 pi-agent 子代理定义（受众意识 / 工作区约定 / 结构化输出骨架 / 具体性要求）。
    // 子代理上下文干净（只有这段 prompt + 任务 prompt），它不知道的一切都必须写在这里。
    "你是 ovoice 的子代理：受主代理委派、在独立干净上下文中完成单一任务的执行者。你看不到主对话历史。\n\n\
     工作区约定：workspace 根下 projects/（项目）、scripts/（脚本）、datas/（数据与文档）、render/（渲染产物），新建文件按类归位；任务若提到 AGENT.md/SOUL.md/MEMORY.md，用 read 自行查看。\n\n\
     动手原则：用 write/read/edit/bash 完成任务；先 read 摸清现状再改，不盲写；长任务每完成一个里程碑让磁盘状态自洽（改一半的东西要么完成要么回退）。\n\n\
     最终总结（这是主代理唯一能看到的东西，主代理没看过你读的任何文件）。格式：\n\
     ## 结论\n完成了什么；若失败，失败在哪（一句话）。\n\
     ## 改动与产出\n逐项：精确文件路径 + 做了什么。\n\
     ## 关键发现\n主代理后续需要的数字、结论、路径。\n\
     ## 遗留\n未完成项/风险/需主代理决策的点；没有写「无」。\n\n\
     要求：具体到文件路径（必要时至行号）；给路径与摘要，不贴大段原文。".into()
}
fn d_max_subagents() -> u64 { 4 }
fn d_subagent_max_iters() -> u64 { 5000 }
fn d_llm_model() -> String { "MiniMax-M3".into() }
fn d_sys() -> String { "你是 ovoice 的 AI 助手，基于 MiniMax-M3。请用简洁、自然的中文回答。".into() }
fn d_tts() -> String { "speech-2.8-hd".into() }
fn d_voice() -> String { "male-qn-qingse".into() }
fn d_speed() -> f64 { 1.0 }
fn d_vol() -> f64 { 1.0 }
fn d_fmt() -> String { "mp3".into() }
// 百度 ASR 凭据不入库（开源仓库）：默认空，用户在「设置」填写；本机已配置者不受影响。
fn d_baidu_appid() -> i64 { 0 }
fn d_baidu_apikey() -> String { String::new() }
fn d_baidu_secret() -> String { String::new() }
fn d_baidu_devpid() -> i64 { 1537 }
fn d_hold_gate() -> u64 { 1000 }
fn d_voice_trigger() -> String { "hold".into() }
fn d_voice_action() -> String { "send".into() }
fn d_max_attachment_mb() -> u64 { 30 }

/// 默认中文音色候选（普通话，官方系统音色子集；可在 config.json 或设置页「自定义」扩展）。
fn d_voices() -> Vec<VoiceOption> {
    vec![
        VoiceOption { id: "male-qn-qingse".into(), label: "青涩青年（男·普通话）".into() },
        VoiceOption { id: "male-qn-jingying".into(), label: "精英青年（男·普通话）".into() },
        VoiceOption { id: "male-qn-badao".into(), label: "霸道青年（男·普通话）".into() },
        VoiceOption { id: "male-qn-daxuesheng".into(), label: "青年大学生（男·普通话）".into() },
        VoiceOption { id: "female-shaonv".into(), label: "少女（普通话）".into() },
        VoiceOption { id: "female-yujie".into(), label: "御姐（普通话）".into() },
        VoiceOption { id: "female-chengshu".into(), label: "成熟女性（普通话）".into() },
        VoiceOption { id: "female-tianmei".into(), label: "甜美女性（普通话）".into() },
        VoiceOption { id: "clever_boy".into(), label: "聪明男童（普通话）".into() },
        VoiceOption { id: "lovely_girl".into(), label: "萌萌女童（普通话）".into() },
        VoiceOption { id: "Chinese (Mandarin)_News_Anchor".into(), label: "新闻女声（普通话）".into() },
        VoiceOption { id: "Chinese (Mandarin)_Male_Announcer".into(), label: "播报男声（普通话）".into() },
        VoiceOption { id: "Chinese (Mandarin)_Warm_Girl".into(), label: "温暖少女（普通话）".into() },
        VoiceOption { id: "Chinese (Mandarin)_Gentleman".into(), label: "温润男声（普通话）".into() },
    ]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key: std::env::var("MINIMAX_API_KEY").unwrap_or_default(),
            llm_model: d_llm_model(),
            system_prompt: d_sys(),
            user_prompt_prefix_enabled: false,
            user_prompt_prefix: String::new(),
            tts_model: d_tts(),
            voice_id: d_voice(),
            speed: d_speed(),
            vol: d_vol(),
            pitch: 0,
            audio_format: d_fmt(),
            baidu_app_id: d_baidu_appid(),
            baidu_api_key: d_baidu_apikey(),
            baidu_secret_key: d_baidu_secret(),
            baidu_dev_pid: d_baidu_devpid(),
            hold_gate_ms: d_hold_gate(),
            voice_trigger: d_voice_trigger(),
            voice_action: d_voice_action(),
            workspace_dir: String::new(),
            cache_dir: String::new(),
            media_roots: Vec::new(),
            voices: d_voices(),
            minimax_region: d_region(),
            max_attachment_mb: d_max_attachment_mb(),
            bg_enabled: false,
            bg_path: String::new(),
            bg_opacity: d_bg_opacity(),
            glass_opacity: d_glass_opacity(),
            active_tools: None,
            subagent_system_prompt: d_subagent_sys(),
            max_subagents: d_max_subagents(),
            subagent_max_iters: d_subagent_max_iters(),
            dream_idle_secs: d_dream_idle(),
            dream_context_trigger_tokens: d_dream_context_trigger_tokens(),
            dream_cap_turns: d_dream_cap(),
            display_window_size: d_display_window(),
            max_tool_iters: d_max_tool_iters(),
            dream_merge_max_rounds: d_dream_merge_max_rounds(),
            dream_batch_max_events: d_dream_batch_max_events(),
        }
    }
}

/// 配置文件路径（纯函数）：`dir/config.json`。
pub fn path_in(dir: &Path) -> PathBuf { dir.join("config.json") }

/// v2 默认工作目录：Documents/ovoice/（脱离 %APPDATA%，备份一棵即全部用户数据）。
/// Windows: %USERPROFILE%\Documents\ovoice；Unix: $HOME/Documents/ovoice；都没有 → ./ovoice 兜底。
pub fn default_workspace_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    match home {
        Some(h) => PathBuf::from(h).join("Documents").join("ovoice"),
        None => PathBuf::from(".").join("ovoice"),
    }
}

/// 从给定目录读取 config.json（纯函数，去 AppHandle；mem bin 与 Tauri 侧共用）。
pub fn load_from(dir: &Path) -> Config {
    let _ = std::fs::create_dir_all(dir);
    let path = path_in(dir);
    let mut cfg = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str::<Config>(&s).unwrap_or_else(|_| Config::default()),
        Err(_) => Config::default(),
    };
    cfg.workspace_dir = resolve_workspace(&cfg.workspace_dir, dir).to_string_lossy().to_string();
    cfg.cache_dir = resolve_cache_dir(&cfg.cache_dir, dir).to_string_lossy().to_string();
    cfg
}

/// 原样读 dir/config.json，不做 workspace_dir 的 resolve 副作用（load_from 会把空字段重写成
/// Documents/ovoice 绝对路径，导致无法判"config 是否真配了 workspace_dir"——bug #3 根因）。
/// mem CLI 用它判空 → 跳档；非空再自行 resolve_workspace。
pub fn load_raw(dir: &Path) -> Config {
    let path = path_in(dir);
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str::<Config>(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

/// 配置文件路径：{app_data_dir}/config.json。
pub fn path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("解析 app_data_dir 失败: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建配置目录失败: {e}"))?;
    Ok(path_in(&dir))
}

pub fn load(app: &AppHandle) -> Config {
    match app.path().app_data_dir() {
        Ok(d) => load_from(&d),
        Err(_) => Config::default(),
    }
}

pub fn save(app: &AppHandle, cfg: &Config) -> Result<(), String> {
    let path = path(app)?;
    save_to(path.as_path(), cfg)
}

/// 写 cfg 到指定路径（序列化 + 落盘）。save(app) 的可测纯逻辑核（load_from 的对偶）。
/// driver 每轮经 load(app)→load_from 重读盘，故 save→load_from 必须往返一致——这是
/// system_prompt / prefix 等保存后下轮生效所依赖的契约（bug: 保存配置不生效 / 清历史 的根因修复点）。
pub fn save_to(path: &std::path::Path, cfg: &Config) -> Result<(), String> {
    let json = serde_json::to_string_pretty(cfg).map_err(|e| format!("序列化配置失败: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("写入配置失败: {e}"))?;
    Ok(())
}

/// 解析工作目录：空 → Documents/ovoice/（v2 默认）；绝对路径原样；相对路径拼到 app_data_dir 下。
pub fn resolve_workspace(field: &str, app_data_dir: &Path) -> PathBuf {
    let trimmed = field.trim();
    if trimmed.is_empty() {
        return default_workspace_dir();
    }
    let p = PathBuf::from(trimmed);
    if p.is_absolute() { p } else { app_data_dir.join(trimmed) }
}

/// 解析缓存目录：空 → app_data_dir/cache（**不**走 Documents——cache 是 ovoice 内部产物，
/// 不该跟 workspace 进云盘同步/备份）；绝对路径原样；相对路径拼到 app_data_dir 下。
pub fn resolve_cache_dir(field: &str, app_data_dir: &Path) -> PathBuf {
    let trimmed = field.trim();
    if trimmed.is_empty() {
        return app_data_dir.join("cache");
    }
    let p = PathBuf::from(trimmed);
    if p.is_absolute() { p } else { app_data_dir.join(trimmed) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_all_fields() {
        let cfg = Config {
            api_key: "sk-test".into(),
            llm_model: "MiniMax-M3".into(),
            system_prompt: "hi".into(),
            user_prompt_prefix_enabled: true,
            user_prompt_prefix: "PREFIX".into(),
            tts_model: "speech-02-hd".into(),
            voice_id: "female-001".into(),
            speed: 1.2,
            vol: 2.0,
            pitch: -3,
            audio_format: "wav".into(),
            baidu_app_id: 123,
            baidu_api_key: "ak".into(),
            baidu_secret_key: "sk".into(),
            baidu_dev_pid: 1737,
            hold_gate_ms: 800,
            voice_trigger: "toggle".into(),
            voice_action: "insert".into(),
            workspace_dir: "D:/w".into(),
            cache_dir: "D:/c".into(),
            media_roots: vec!["D:/pics".into()],
            voices: vec![VoiceOption { id: "custom-x".into(), label: "自定义".into() }],
            minimax_region: "cn".into(),
            max_attachment_mb: 50,
            bg_enabled: true,
            bg_path: "D:/photos/bg.jpg".into(),
            bg_opacity: 0.5,
            glass_opacity: 0.7,
            active_tools: None,
            subagent_system_prompt: d_subagent_sys(),
            max_subagents: 4,
            subagent_max_iters: 5000,
            dream_idle_secs: 7200,
            dream_context_trigger_tokens: 300_000,
            dream_cap_turns: 3,
            display_window_size: 50,
            max_tool_iters: 100,
            dream_merge_max_rounds: 10,
            dream_batch_max_events: 100,
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.llm_model, "MiniMax-M3");
        assert_eq!(back.voice_id, "female-001");
        assert_eq!(back.pitch, -3);
        assert_eq!(back.audio_format, "wav");
        assert_eq!(back.baidu_app_id, 123);
        assert_eq!(back.baidu_dev_pid, 1737);
        assert_eq!(back.hold_gate_ms, 800);
        assert_eq!(back.voice_trigger, "toggle");
        assert_eq!(back.voice_action, "insert");
        assert!((back.speed - 1.2).abs() < 1e-9);
        assert_eq!(back.workspace_dir, "D:/w");
        assert_eq!(back.cache_dir, "D:/c");
        assert_eq!(back.media_roots, vec!["D:/pics".to_string()]);
        assert_eq!(back.voices.len(), 1);
        assert_eq!(back.voices[0].id, "custom-x");
        assert!(back.bg_enabled);
        assert_eq!(back.max_attachment_mb, 50);
        assert_eq!(back.bg_path, "D:/photos/bg.jpg");
        assert!((back.bg_opacity - 0.5).abs() < 1e-9);
        assert!((back.glass_opacity - 0.7).abs() < 1e-9);
        assert!(back.user_prompt_prefix_enabled);
        assert_eq!(back.user_prompt_prefix, "PREFIX");
    }

    #[test]
    fn missing_fields_use_defaults() {
        let back: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(back.llm_model, "MiniMax-M3");
        assert_eq!(back.tts_model, "speech-2.8-hd");
        assert_eq!(back.voice_id, "male-qn-qingse");
        assert_eq!(back.audio_format, "mp3");
        assert_eq!(back.baidu_app_id, 0, "凭据默认值必须为空（开源仓库不携带密钥）");
        assert_eq!(back.baidu_dev_pid, 1537);
        assert_eq!(back.hold_gate_ms, 1000);
        assert_eq!(back.voice_trigger, "hold");
        assert_eq!(back.voice_action, "send");
        assert!((back.speed - 1.0).abs() < 1e-9);
        assert_eq!(back.pitch, 0);
        assert!(!back.bg_enabled);
        assert_eq!(back.max_attachment_mb, 30);
        assert_eq!(back.bg_path, "");
        assert!((back.bg_opacity - 0.35).abs() < 1e-9);
        assert!((back.glass_opacity - 0.85).abs() < 1e-9);
        assert_eq!(back.max_subagents, 4);
        assert_eq!(back.subagent_max_iters, 5000, "默认须覆盖数小时/数千循环的长任务");
    }

    #[test]
    fn default_subagent_prompt_has_pi_aligned_skeleton() {
        let p = d_subagent_sys();
        for head in ["## 结论", "## 改动与产出", "## 关键发现", "## 遗留"] {
            assert!(p.contains(head), "默认子代理提示词缺结构化骨架段 {head}");
        }
        assert!(p.contains("主代理唯一能看到的东西"), "须含受众意识声明");
        assert!(p.contains("projects/"), "须含 workspace 归档约定");
        assert!(p.contains("看不到主对话历史"), "须声明干净上下文");
    }

    #[test]
    fn default_voices_are_chinese_and_nonempty() {
        let v = d_voices();
        assert!(!v.is_empty());
        assert!(v.iter().any(|o| o.id == "male-qn-qingse"), "默认含 male-qn-qingse");
        for o in &v {
            assert!(!o.id.starts_with("English_"), "非中文: {}", o.id);
            assert!(!o.id.starts_with("Japanese_"), "非中文: {}", o.id);
            assert!(!o.id.starts_with("Korean_"), "非中文: {}", o.id);
        }
    }

    #[test]
    fn missing_voices_uses_default() {
        let back: Config = serde_json::from_str("{}").unwrap();
        assert!(!back.voices.is_empty());
    }

    #[test]
    fn resolve_workspace_empty_defaults() {
        // v2 行为变更：空 → Documents/ovoice（旧断言 d.join("workspace") 已废弃）
        let d = Path::new("C:/fake/appdata");
        let got = resolve_workspace("", d);
        let s = got.to_string_lossy().replace('\\', "/");
        assert!(s.ends_with("Documents/ovoice"), "空应走默认 Documents/ovoice，got {s}");
        let got_ws = resolve_workspace("   ", d);
        let s2 = got_ws.to_string_lossy().replace('\\', "/");
        assert!(s2.ends_with("Documents/ovoice"), "空白应走默认 Documents/ovoice，got {s2}");
    }

    #[test]
    fn v2_fields_have_defaults() {
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c.dream_idle_secs, 7200);
        assert_eq!(c.dream_context_trigger_tokens, 300_000);
        assert_eq!(c.dream_cap_turns, 3);
        assert_eq!(c.display_window_size, 50);
        assert_eq!(c.max_tool_iters, 100);
    }

    #[test]
    fn v2_fields_roundtrip() {
        let mut c = Config::default();
        c.dream_idle_secs = 1200;
        c.dream_context_trigger_tokens = 250_000;
        c.dream_cap_turns = 30;
        c.display_window_size = 80;
        c.max_tool_iters = 200;
        let s = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.dream_idle_secs, 1200);
        assert_eq!(back.dream_context_trigger_tokens, 250_000);
        assert_eq!(back.dream_cap_turns, 30);
        assert_eq!(back.display_window_size, 80);
        assert_eq!(back.max_tool_iters, 200);
    }

    #[test]
    fn load_from_reads_config_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"llm_model":"X","dream_cap_turns":7}"#,
        ).unwrap();
        let c = load_from(dir.path());
        assert_eq!(c.llm_model, "X");
        assert_eq!(c.dream_cap_turns, 7);
    }

    #[test]
    fn load_from_missing_file_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = load_from(dir.path());
        assert_eq!(c.dream_idle_secs, 7200); // 默认值
    }

    // 回归：save_to → load_from 往返一致。driver run_one 每轮 load(app)→load_from 重读盘，
    // 若 save 写的内容 load 读不回，则保存的 system_prompt / prefix 下轮不生效（旧 bug 根因）。
    #[test]
    fn save_to_then_load_from_roundtrips_turn_invariant_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        let mut cfg = Config::default();
        cfg.system_prompt = "新人格".into();
        cfg.user_prompt_prefix_enabled = true;
        cfg.user_prompt_prefix = "请简洁".into();
        cfg.max_tool_iters = 150;
        cfg.dream_cap_turns = 42;
        save_to(&path, &cfg).unwrap();
        let back = load_from(dir.path());
        assert_eq!(back.system_prompt, "新人格");
        assert!(back.user_prompt_prefix_enabled);
        assert_eq!(back.user_prompt_prefix, "请简洁");
        assert_eq!(back.max_tool_iters, 150);
        assert_eq!(back.dream_cap_turns, 42);
    }

    // 回归：再次 save 覆盖后，load_from 读到的是最新值（不是首次写入的旧值）。
    // 锁定 driver 每轮重读能拿到用户最近一次保存。
    #[test]
    fn save_to_overwrites_and_load_from_sees_latest() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(dir.path());
        let mut cfg = Config::default();
        cfg.system_prompt = "第一版".into();
        save_to(&path, &cfg).unwrap();
        cfg.system_prompt = "第二版".into();
        cfg.user_prompt_prefix = "新前缀".into();
        save_to(&path, &cfg).unwrap();
        let back = load_from(dir.path());
        assert_eq!(back.system_prompt, "第二版");
        assert_eq!(back.user_prompt_prefix, "新前缀");
    }

    #[test]
    fn path_in_is_dir_config_json() {
        let p = path_in(std::path::Path::new("C:/fake"));
        assert_eq!(p, std::path::PathBuf::from("C:/fake/config.json"));
    }

    #[test]
    fn default_workspace_ends_with_documents_ovoice() {
        let d = default_workspace_dir();
        let s = d.to_string_lossy().replace('\\', "/");
        assert!(s.ends_with("Documents/ovoice"), "got {s}");
    }

    #[test]
    fn resolve_workspace_empty_uses_documents_default() {
        // v2 行为变更：空 → Documents/ovoice（旧测试断言 app_data/workspace 须改）
        let d = std::path::Path::new("C:/fake/appdata");
        let got = resolve_workspace("", d);
        let s = got.to_string_lossy().replace('\\', "/");
        assert!(s.ends_with("Documents/ovoice"), "空应走默认 Documents/ovoice，got {s}");
    }

    #[test]
    fn resolve_workspace_absolute_passthrough() {
        let d = Path::new("C:/fake/appdata");
        assert_eq!(resolve_workspace("D:/proj", d), PathBuf::from("D:/proj"));
    }

    #[test]
    fn resolve_workspace_relative_joined() {
        let d = Path::new("C:/fake/appdata");
        assert_eq!(resolve_workspace("myproj", d), d.join("myproj"));
    }

    #[test]
    fn resolve_cache_dir_empty_defaults_to_appdata_cache() {
        // 与 workspace 不同：cache 空 → app_data_dir/cache（不进 Documents）
        let d = Path::new("C:/fake/appdata");
        assert_eq!(resolve_cache_dir("", d), d.join("cache"));
        assert_eq!(resolve_cache_dir("   ", d), d.join("cache"));
    }
    #[test]
    fn resolve_cache_dir_absolute_passthrough() {
        let d = Path::new("C:/fake/appdata");
        assert_eq!(resolve_cache_dir("E:/ovoice-cache", d), PathBuf::from("E:/ovoice-cache"));
    }
    #[test]
    fn resolve_cache_dir_relative_joined() {
        let d = Path::new("C:/fake/appdata");
        assert_eq!(resolve_cache_dir("mycache", d), d.join("mycache"));
    }

    #[test]
    fn minimax_region_defaults_to_cn() {
        let j = serde_json::json!({});
        let c: Config = serde_json::from_value(j).unwrap();
        assert_eq!(c.minimax_region, "cn");
    }

    #[test]
    fn active_tools_defaults_none_and_skipped() {
        let c: Config = serde_json::from_str("{}").unwrap();
        assert!(c.active_tools.is_none(), "默认 None=全量 tools::schemas()");
        let s = serde_json::to_string(&c).unwrap();
        assert!(!s.contains("active_tools"), "active_tools 不持久化: {s}");
    }

    #[test]
    fn subagent_system_prompt_has_default() {
        let c: Config = serde_json::from_str("{}").unwrap();
        assert!(c.subagent_system_prompt.contains("子代理"), "默认值: {}", c.subagent_system_prompt);
    }

    #[test]
    fn max_subagents_defaults_to_four() {
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c.max_subagents, 4);
    }

    // ─── Task 1: dream 完整性改造配置字段 ───

    #[test]
    fn dream_defaults() {
        let c = Config::default();
        assert_eq!(c.dream_merge_max_rounds, 5, "合并上限默认 5 回合");
        // 既有默认不回归
        assert_eq!(c.dream_idle_secs, 7200);
        assert_eq!(c.dream_context_trigger_tokens, 300_000);
        assert_eq!(c.dream_cap_turns, 3);
    }

    #[test]
    fn dream_fields_survive_json_roundtrip() {
        let c = Config { dream_merge_max_rounds: 10, ..Config::default() };
        let json = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dream_merge_max_rounds, 10);
    }

    #[test]
    fn user_prompt_prefix_disabled_and_empty_by_default() {
        let c = Config::default();
        assert!(!c.user_prompt_prefix_enabled, "默认关闭");
        assert_eq!(c.user_prompt_prefix, "", "默认空串");
    }

    #[test]
    fn user_prompt_prefix_roundtrip() {
        let c = Config {
            user_prompt_prefix_enabled: true,
            user_prompt_prefix: "请用英文回答".into(),
            ..Config::default()
        };
        let s = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert!(back.user_prompt_prefix_enabled);
        assert_eq!(back.user_prompt_prefix, "请用英文回答");
    }

    #[test]
    fn user_prompt_prefix_missing_field_uses_default() {
        let back: Config = serde_json::from_str("{}").unwrap();
        assert!(!back.user_prompt_prefix_enabled);
        assert_eq!(back.user_prompt_prefix, "");
    }
}
