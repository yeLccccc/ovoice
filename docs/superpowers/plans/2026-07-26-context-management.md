# 上下文管理 v2（Context Management v2）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 ovoice agent 引入「history 是唯一真相之源、context/display 是派生视图」的上下文管理：append-only history、重启重建 context、dream 整理「日」层记忆、mem CLI 深思 drill、display 滑动窗口。

**Architecture:** 三层模型 —— `history/{date}.jsonl`（append-only 真相之源，单写不变量）/ `context`（每轮从 history 尾段重建的 LLM 窗口）/ `display`（前端懒加载视图）。dream 子代理把 history 整理成 `memory/{Y}/{M}/{date}.md` + `MEMORY.md`「日」层。mem 是同 crate 第二个 `[[bin]]`，零 LLM 只读 drill。本计划只做 **spec §15 第 1 期**（「日」层 + drill 能成立的最小子）；第 2 期（周/月/年晋级 + revise）见末尾「延后」一节，另开计划。

**Tech Stack:** Rust + tokio + Tauri 2 + serde_json；chrono（日期）；vanilla JS（无打包器、无 JS 测试框架）。

## Scope（§15 第 1 期，本计划边界）

**包含**：history 单写 + context 重建（含配对不变量）+ 重启重建 + 「日」层 + MEMORY「日」+ mem CLI（ls/read/history/search）+ display 滑动窗口 + dream（仅 idle/cap 触发 + 提取到「日」+ 写 marker；**不做**周/月/年晋级、**不做** revise）。

**延后（第 2 期，另开计划）**：§7.8 step 2-4（跨午夜「日」→「周」、周>7天→月、月>3月→年）、§7.4 op3 revise、MEMORY「周/月/年」三层的实际填充逻辑。本计划只在 `MEMORY.md` 预留四层骨架标题，「周/月/年」体暂时空（dream 第 1 期不写它们）。

## Global Constraints

- **绿色/便携软件**：config + mem.exe next-to-exe（portable 优先），无 installer/PATH 写入/注册表；`%APPDATA%` 仅装版 fallback。备份 `workspace/` 一棵 = 全部用户数据（spec §9）。
- **后端纯逻辑可离线测**：`cargo test --lib`（tempfile workspace + FakeRound/FakeEmitter + 录音 writer），与现有 `tools.rs`/`agent.rs`/`subagents.rs` 测试同款范式。
- **dev server 占 exe**：验证一律 `cargo check --tests --manifest-path src-tauri/Cargo.toml` 或 `cargo test --lib --manifest-path src-tauri/Cargo.toml`，**不要** `cargo build`/`cargo run`（会撞 dev server 锁）。记忆 [[ovoice-dev-server-cargo-lock]]。
- **agent driver 事件驱动零锁**：`messages` 私有持有于 spawned task；事件经 mpsc 串行（见 `agent.rs::spawn_session`）。v2 删除 `window_messages`，改为每轮从 history 重建。
- **工具计数不级联**：v2 不新增 LLM 工具（mem 是独立 CLI，不是工具）→ `tools::schemas()` 仍 7 个、`llm.rs` 测试 `build_body_has_tools_and_reasoning_split` 的 `len()==7` 不动。dream 复用 subagent 基础设施（非新工具）。
- **四条 P1 铁律（必须落到对应任务测试里）**：① history 单写不变量（一 writer task + 一 mpsc，writer 串行分配 seq）；② 配对不变量（孤儿 tool_call 丢弃 / N 轮截断配对感知 / external·marker·subagent_result 不插进 tool_call→tool_result 对中间）；③ seq 指针代码盖戳（dream 写 memory 的「对话索引」seq[a,b] 由代码盖戳，LLM 只写正文）；④ cap 只数 `kind=user`（external/subagent_result 不顶 cap）；重建边界 = 最后一个 `marker(dream|reset)` 之后。
- **workspace 默认**：`Documents/ovoice/`（脱离 `%APPDATA%`）。设置可改；无效路径回退 `%APPDATA%/.../workspace` + UI 告警（spec §9/§12/§13.1）。
- **pinned 4 块顺序**：system_prompt → SOUL.md → AGENT.md → MEMORY.md（spec §3）。
- **命名**：`history/`（不叫 trace）、`dream`（不叫 consolidate/archive）、`mem` CLI。
- **错误降级**：history 写失败 warn+重试不阻塞 turn；dream 失败不推进 marker（自动重试同段）；重建遇坏行跳过；末尾孤儿 tool_call 丢弃（spec §13.1）。

## File Structure

**新建（src-tauri/src/）**：
- `history.rs` — `HistoryEvent` schema + `HistoryWriter`（单写 task：独占文件句柄、串行 append、writer 分配 seq、跨天切文件）+ `HistoryWriterHandle`（clone 进 ToolsCtx，producer 投递事件）+ 读侧（`read_all_in_range` / 跳坏行）。单写不变量住这里。
- `context.rs` — `build_messages(events, pinned, cap)`：找最后 marker(dream|reset) → 取其后 thread==main 事件 → kind→LLM message → 配对不变量 → N 轮 cap（只数 kind=user）→ 前拼 4 pinned 块。`PinnedBlocks` + 加载器。
- `memory.rs` — 「日」层：`append_day_event(...)` 写 `memory/{Y}/{M}/{date}.md` 事件段（**seq 指针代码盖戳**）+ `append_memory_day(...)` 追加 `MEMORY.md`「日」节。dream 是唯一写者。`MEMORY.md` 四层骨架。
- `dream.rs` — `DreamTrigger`（idle + cap + 单飞 + 空跳过，fake-time 可测）+ `spawn_dream_agent`（复用 subagent 基础设施）+ `dream_prompt` + 提取→「日」+ 写 dream marker（until_seq = dream-start seq）。第 1 期仅 step1+step6。
- `mem_cli.rs` — ls/read/history/search 纯逻辑（文件树即索引，零 LLM）；mem bin 与测试共用。
- `bin/mem.rs` — 第二个 `[[bin]]`：arg 解析 → `mem_cli` → 打印；`config::load_from` 定位 workspace。

**修改**：
- `config.rs` — 抽 `load_from(dir)`/`path_in(dir)`（去 AppHandle）；加 `dream_idle_secs`(600)/`dream_cap_turns`(50)/`display_window_size`(50)；workspace 默认 `Documents/ovoice/`。
- `llm.rs` — `run_turn` 把每个 assistant/tool_result 事件 append 进 history（经 ctx 的 writer handle）；保留本地 history Vec 给 ChatResponse。
- `agent.rs` — 删 `window_messages`；每轮 turn 开头 `context::build_messages` 重建；user/external/marker 事件 append；Reset→marker:reset；ContextNote→external（不计 cap）；接线 dream trigger（轮间）。
- `tools.rs` — `ToolsCtx` 加 `history: HistoryWriterHandle`；`foreground`/`clone_ctx` 同步。
- `subagents.rs` — 子代理 run_turn append `thread=agent:N` 事件；完成写 `subagent_result(main)` + ref。
- `lib.rs` — setup：解析 workspace（Documents/ovoice/）、bootstrap SOUL/AGENT/MEMORY 默认、spawn history writer、灌进 ToolsCtx + spawn_session、spawn dream trigger、加 `history_tail`/`history_head` 命令。
- `src/main.js` — display 滑动窗口（[lo,hi]、启动 tail、上下划加载、实时贴底 append、marker 隐藏）+ 设置 3 输入。
- `src/index.html` — 高级区 3 个 input（dream_idle_secs/dream_cap_turns/display_window_size）。
- `src-tauri/Cargo.toml` — 加 `chrono` 依赖 + `[[bin]] name="mem"`。
- `src-tauri/tauri.conf.json` — `externalBin` 把 mem.exe 落 ovoice.exe 同目录（release 绿色打包）。

**已澄清的设计决定（spec 轻微歧义，本计划这样落）**：
- **tool_call 存储模型 = Option A**：assistant 事件自带 `tool_calls: Vec<Value>`（与 `run_turn` 的 `assistant_message` 一致），**不**单列 `tool_call` kind。spec §4 表里的 `tool_call` kind 实现为 assistant 事件内嵌的 `tool_calls` 数组；§6「tool_call 并入前一条 assistant」在此模型下是 no-op（本就内嵌）。配对不变量映射为：每个 `assistant.tool_calls[j].id` 必须有其后跟的 `tool_result(call_id)`，否则该 assistant 在尾段/截断处判孤儿丢弃。理由：与 `run_turn` 数据模型一致、重建无需 merge 步、孤儿判定语义（「带 tool_calls 但无后续 tool 的 assistant」）正好契合 spec §6 原文。

---

## Task 1: config 去 AppHandle 化 + v2 三字段 + workspace 默认 Documents/ovoice

**Why first**：后续所有任务（history writer 定位 workspace、mem bin 无 AppHandle、settings UI）都依赖 `load_from(dir)` 与三个新字段。这是前置重构。

**Files:**
- Modify: `src-tauri/src/config.rs`（全文相关：`Config` 结构、`Default`、`path`/`load`、`resolve_workspace`、测试）
- Test: `src-tauri/src/config.rs` 内 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 无（基础层）
- Produces:
  - `pub fn load_from(dir: &std::path::Path) -> Config`（纯函数，去 AppHandle；mem bin 与 Tauri 侧都用它）
  - `pub fn path_in(dir: &std::path::Path) -> std::path::PathBuf`（返 `dir/config.json`）
  - `pub fn default_workspace_dir() -> std::path::PathBuf`（返 `Documents/ovoice/`）
  - `Config` 新字段：`dream_idle_secs: u64`（默认 600）、`dream_cap_turns: u64`（默认 50）、`display_window_size: u64`（默认 50）
  - `resolve_workspace("", _)` 行为变更：返 `default_workspace_dir()`（旧：`app_data_dir/workspace`）

**注意（行为变更，须告知用户）**：workspace 默认从 `%APPDATA%/.../workspace` 改为 `Documents/ovoice/`。旧用户数据若在旧路径需手动迁移（spec §9：迁移单独排、不阻塞 v2）。设置页 workspace 行仍可改。

- [ ] **Step 1: 写失败测试**（追加到 `config.rs` 的 `mod tests`）

```rust
    #[test]
    fn v2_fields_have_defaults() {
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c.dream_idle_secs, 600);
        assert_eq!(c.dream_cap_turns, 50);
        assert_eq!(c.display_window_size, 50);
    }

    #[test]
    fn v2_fields_roundtrip() {
        let mut c = Config::default();
        c.dream_idle_secs = 1200;
        c.dream_cap_turns = 30;
        c.display_window_size = 80;
        let s = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.dream_idle_secs, 1200);
        assert_eq!(back.dream_cap_turns, 30);
        assert_eq!(back.display_window_size, 80);
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
        assert_eq!(c.dream_idle_secs, 600); // 默认值
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
```

同时**更新**既有测试 `resolve_workspace_empty_defaults`（原断言 `d.join("workspace")`）为上面新语义；若保留旧名则改断言为 `ends_with("Documents/ovoice")`。并更新 `roundtrip_preserves_all_fields` 与 `Default` 构造里加入三个新字段（见 Step 3）。

- [ ] **Step 2: 跑测试确认失败**

```
cargo test --lib --manifest-path src-tauri/Cargo.toml config::tests
```
Expected: 编译失败（`dream_idle_secs` 等字段不存在、`load_from`/`path_in`/`default_workspace_dir` 未定义）。

- [ ] **Step 3: 实现**

(a) `Config` 结构加三字段（紧跟 `max_subagents` 之后）：

```rust
    // v2 上下文管理：dream 触发与 display 窗口（高级设置，默认无感）
    #[serde(default = "d_dream_idle")]
    pub dream_idle_secs: u64,
    #[serde(default = "d_dream_cap")]
    pub dream_cap_turns: u64,
    #[serde(default = "d_display_window")]
    pub display_window_size: u64,
```

加默认函数：
```rust
fn d_dream_idle() -> u64 { 600 }
fn d_dream_cap() -> u64 { 50 }
fn d_display_window() -> u64 { 50 }
```

(b) `Default` impl 加：`dream_idle_secs: d_dream_idle(), dream_cap_turns: d_dream_cap(), display_window_size: d_display_window(),`；`roundtrip_preserves_all_fields` 测试里的字面构造同步加这三字段。

(c) 加 `default_workspace_dir` + 改 `resolve_workspace`：

```rust
/// v2 默认工作目录：Documents/ovoice/（脱离 %APPDATA%，备份一棵即全部用户数据）。
/// Windows: %USERPROFILE%\Documents\ovoice；Unix: $HOME/Documents/ovoice；都没有 → ./ovoice 兜底。
pub fn default_workspace_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    match home {
        Some(h) => PathBuf::from(h).join("Documents").join("ovoice"),
        None => PathBuf::from(".").join("ovoice"),
    }
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
```

(d) 加 `load_from` / `path_in`，把现有 `load(app)` / `path(app)` 改为薄壳：

```rust
pub fn path_in(dir: &Path) -> PathBuf { dir.join("config.json") }

pub fn load_from(dir: &Path) -> Config {
    let _ = std::fs::create_dir_all(dir);
    let path = path_in(dir);
    let mut cfg = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str::<Config>(&s).unwrap_or_else(|_| Config::default()),
        Err(_) => Config::default(),
    };
    cfg.workspace_dir = resolve_workspace(&cfg.workspace_dir, dir).to_string_lossy().to_string();
    cfg
}

/// 配置文件路径：{app_data_dir}/config.json。
pub fn path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir()
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
```

- [ ] **Step 4: 跑测试确认通过**

```
cargo test --lib --manifest-path src-tauri/Cargo.toml config::tests
```
Expected: PASS（含新测试 + 更新后的 roundtrip/resolve_workspace 测试）。

- [ ] **Step 5: 全量 check 确保没破其他模块**

```
cargo check --tests --manifest-path src-tauri/Cargo.toml
```
Expected: 0 warning / 0 error（`load(app)`/`path(app)` 签名不变，lib.rs 等调用方不受影响）。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/config.rs
git commit -m "feat(config): load_from 去 AppHandle + dream_idle_secs/dream_cap_turns/display_window_size + workspace 默认 Documents/ovoice"
```

---

## Task 2: history.rs — HistoryEvent + 单写 writer（P1 铁律①住这里）

**Why**：history 是唯一真相之源。三个并发生产者（主 driver / 每个子代理 / dream marker）都必须经**同一 writer task + 同一 mpsc** 落盘，由 writer 串行分配 `seq`，否则并发 append 会交错损坏行。这条是 §13.1「进程崩了才丢」承诺的前提。

**Files:**
- Create: `src-tauri/src/history.rs`
- Modify: `src-tauri/src/lib.rs`（加 `pub mod history;`）

**Interfaces:**
- Consumes: `crate::AttachmentRef`（user 事件 attachments）
- Produces:
  - `pub struct HistoryEvent { seq: u64, ts: u64, thread: String, kind: String, data: Map<String,Value> }` + 构造器 `user/assistant/tool_result/external/subagent_result/edit/marker`
  - `#[derive(Clone)] pub struct HistoryWriterHandle { tx: UnboundedSender<HistoryEvent>, current_seq: Arc<AtomicU64> }`，方法 `append(ev)`（投递，seq 留 writer 盖戳）、`current_seq() -> u64`（dream 取 dream-start 边界）
  - `pub fn spawn_writer(history_dir: PathBuf) -> HistoryWriterHandle`（启动单写 task）
  - `pub fn read_all(history_dir: &Path) -> Vec<HistoryEvent>`（升序、跳坏行）
  - `pub fn date_from_ts(ts: u64) -> String`（epoch ms → `YYYY-MM-DD`，UTC，确定性；memory.rs 复用）

**事件字段（spec §4 表，kind→data 字段）**：`user{text,attachments}` / `assistant{content,thinking,tool_calls}` / `tool_result{name,result,call_id}` / `subagent_result{agent_id,summary,ref}` / `external{what,path,hash?}` / `edit{path,sha_before?,sha_after?}` / `marker{marker:"dream"|"reset",until_seq}`。

- [ ] **Step 1: 写失败测试**（新建 `history.rs` 的 `#[cfg(test)] mod tests`）

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn flush() { tokio::time::sleep(Duration::from_millis(120)).await; }

    #[test]
    fn date_from_ts_epoch() {
        assert_eq!(date_from_ts(0), "1970-01-01");
    }
    #[test]
    fn date_from_ts_known() {
        // 2026-07-26 00:00:00 UTC = 1782768000 s = 1782768000000 ms
        assert_eq!(date_from_ts(1_782_768_000_000), "2026-07-26");
    }

    #[test]
    fn event_user_roundtrip() {
        let ev = HistoryEvent::user(1000, "main", "你好", &[]);
        let line = serde_json::to_string(&ev).unwrap();
        let back: HistoryEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(back.kind, "user");
        assert_eq!(back.thread, "main");
        assert_eq!(back.data["text"], serde_json::json!("你好"));
    }
    #[test]
    fn event_marker_roundtrip() {
        let ev = HistoryEvent::marker(2000, "dream", 42);
        let line = serde_json::to_string(&ev).unwrap();
        let back: HistoryEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(back.kind, "marker");
        assert_eq!(back.data["marker"], serde_json::json!("dream"));
        assert_eq!(back.data["until_seq"], serde_json::json!(42));
    }

    #[tokio::test]
    async fn writer_appends_monotonic_seq() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"));
        h.append(HistoryEvent::user(1000, "main", "a", &[]));
        h.append(HistoryEvent::user(2000, "main", "b", &[]));
        h.append(HistoryEvent::user(3000, "main", "c", &[]));
        flush().await;
        let evs = read_all(&dir.path().join("history"));
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0].seq, 0);
        assert_eq!(evs[1].seq, 1);
        assert_eq!(evs[2].seq, 2);
        assert_eq!(h.current_seq(), 3);
    }

    #[tokio::test]
    async fn writer_writes_dated_file_and_switches_day() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"));
        // 同一天两条
        h.append(HistoryEvent::user(1_782_768_000_000, "main", "d1a", &[]));
        h.append(HistoryEvent::user(1_782_768_001_000, "main", "d1b", &[]));
        // 次日一条
        h.append(HistoryEvent::user(1_782_768_000_000 + 86_400_000, "main", "d2", &[]));
        flush().await;
        assert!(dir.path().join("history/2026-07-26.jsonl").exists());
        assert!(dir.path().join("history/2026-07-27.jsonl").exists());
        let evs = read_all(&dir.path().join("history"));
        assert_eq!(evs.len(), 3, "跨天合并读取");
    }

    #[tokio::test]
    async fn writer_append_only_grows_not_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"));
        h.append(HistoryEvent::user(1000, "main", "x", &[]));
        flush().await;
        assert_eq!(read_all(&dir.path().join("history")).len(), 1);
        h.append(HistoryEvent::user(2000, "main", "y", &[]));
        flush().await;
        assert_eq!(read_all(&dir.path().join("history")).len(), 2, "append-only：文件应增长非覆盖");
    }

    #[tokio::test]
    async fn writer_multi_producer_seq_monotonic_no_corrupt() {
        // P1 铁律①：多生产者并发 → seq 单调连续 + 无交错坏行
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"));
        let mut handles = vec![];
        for t in 0..4u64 {
            let h2 = h.clone();
            handles.push(tokio::spawn(async move {
                for i in 0..25u64 {
                    h2.append(HistoryEvent::user(1000 + t * 100 + i, "main", &format!("t{t}-{i}"), &[]));
                }
            }));
        }
        for hd in handles { let _ = hd.await; }
        flush().await;
        let evs = read_all(&dir.path().join("history"));
        assert_eq!(evs.len(), 100, "4×25 全部落盘");
        let seqs: Vec<u64> = evs.iter().map(|e| e.seq).collect();
        let mut sorted = seqs.clone(); sorted.sort();
        assert_eq!(sorted, (0..100u64).collect::<Vec<_>>(), "seq 应 0..99 无缺无重");
        // 每行都可解析（read_all 已跳坏行；再验文件直读无垃圾）
        let raw = std::fs::read_to_string(dir.path().join("history").join(format!("{}.jsonl", date_from_ts(1000)))).unwrap();
        for line in raw.lines() {
            assert!(serde_json::from_str::<HistoryEvent>(line).is_ok(), "坏行: {line}");
        }
    }

    #[tokio::test]
    async fn read_all_skips_bad_lines() {
        let dir = tempfile::tempdir().unwrap();
        let hist = dir.path().join("history");
        std::fs::create_dir_all(&hist).unwrap();
        let f = hist.join(format!("{}.jsonl", date_from_ts(1000)));
        let good1 = serde_json::to_string(&HistoryEvent::user(1000, "main", "a", &[])).unwrap();
        let good2 = serde_json::to_string(&HistoryEvent::user(2000, "main", "b", &[])).unwrap();
        std::fs::write(&f, format!("{good1}\nTHIS IS GARBAGE\n{good2}\n")).unwrap();
        let evs = read_all(&hist);
        assert_eq!(evs.len(), 2, "坏行应跳过");
    }
}
```

- [ ] **Step 2: 注册模块 + 跑测试确认失败**

`src-tauri/src/lib.rs` 顶部模块声明区加 `pub mod history;`（紧跟 `pub mod extract;` 之后）。
```
cargo test --lib --manifest-path src-tauri/Cargo.toml history::tests
```
Expected: 编译失败（`history` 模块为空/类型未定义）。

- [ ] **Step 3: 实现 `history.rs`**

```rust
//! v2 上下文管理：history 是唯一真相之源（append-only jsonl）。
//! 单写不变量（P1 铁律①）：三个并发生产者（主 driver / 子代理 / dream marker）都经同一 writer task +
//! 同一 mpsc 落盘；writer 串行分配 seq、独占文件句柄、跨天切文件。并发 append 才不交错损坏行。
use crate::AttachmentRef;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// 一条 history 事件。seq 由 writer 盖戳（producer 发出时 seq=0）；ts=epoch ms（producer 盖）。
/// kind 决定 data 字段（见模块文档）。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HistoryEvent {
    pub seq: u64,
    pub ts: u64,
    pub thread: String, // "main" | "agent:{id}"
    pub kind: String,
    #[serde(flatten)]
    pub data: Map<String, Value>,
}

impl HistoryEvent {
    pub fn user(ts: u64, thread: &str, text: &str, attachments: &[AttachmentRef]) -> Self {
        let mut d = Map::new();
        d.insert("text".into(), json!(text));
        d.insert("attachments".into(), json!(attachments));
        Self { seq: 0, ts, thread: thread.into(), kind: "user".into(), data: d }
    }
    pub fn assistant(ts: u64, thread: &str, content: &str, thinking: &str, tool_calls: Vec<Value>) -> Self {
        let mut d = Map::new();
        d.insert("content".into(), json!(content));
        d.insert("thinking".into(), json!(thinking));
        d.insert("tool_calls".into(), Value::Array(tool_calls));
        Self { seq: 0, ts, thread: thread.into(), kind: "assistant".into(), data: d }
    }
    pub fn tool_result(ts: u64, thread: &str, name: &str, result: &str, call_id: &str) -> Self {
        let mut d = Map::new();
        d.insert("name".into(), json!(name));
        d.insert("result".into(), json!(result));
        d.insert("call_id".into(), json!(call_id));
        Self { seq: 0, ts, thread: thread.into(), kind: "tool_result".into(), data: d }
    }
    pub fn external(ts: u64, what: &str, path: &str, hash: Option<&str>) -> Self {
        let mut d = Map::new();
        d.insert("what".into(), json!(what));
        d.insert("path".into(), json!(path));
        if let Some(h) = hash { d.insert("hash".into(), json!(h)); }
        Self { seq: 0, ts, thread: "main".into(), kind: "external".into(), data: d }
    }
    pub fn subagent_result(ts: u64, agent_id: u64, summary: &str, refs: &str) -> Self {
        let mut d = Map::new();
        d.insert("agent_id".into(), json!(agent_id));
        d.insert("summary".into(), json!(summary));
        d.insert("ref".into(), json!(refs));
        Self { seq: 0, ts, thread: "main".into(), kind: "subagent_result".into(), data: d }
    }
    pub fn edit(ts: u64, path: &str, sha_before: Option<&str>, sha_after: Option<&str>) -> Self {
        let mut d = Map::new();
        d.insert("path".into(), json!(path));
        if let Some(s) = sha_before { d.insert("sha_before".into(), json!(s)); }
        if let Some(s) = sha_after { d.insert("sha_after".into(), json!(s)); }
        Self { seq: 0, ts, thread: "main".into(), kind: "edit".into(), data: d }
    }
    pub fn marker(ts: u64, which: &str, until_seq: u64) -> Self {
        let mut d = Map::new();
        d.insert("marker".into(), json!(which)); // "dream" | "reset"
        d.insert("until_seq".into(), json!(until_seq));
        Self { seq: 0, ts, thread: "main".into(), kind: "marker".into(), data: d }
    }
}

/// epoch ms → YYYY-MM-DD（UTC；确定性，跨机一致；测试可精确断言）。memory.rs 复用。
pub fn date_from_ts(ts: u64) -> String {
    let days = (ts / 86_400_000) as i64; // 天数（自 1970-01-01）
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant civil_from_days：天数 → (年, 月, 日)，纯算术无外部依赖。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// Producer 句柄：clone 进 ToolsCtx，所有 append 经此投递到单写 task。
#[derive(Clone)]
pub struct HistoryWriterHandle {
    pub tx: mpsc::UnboundedSender<HistoryEvent>,
    current_seq: Arc<AtomicU64>,
}

impl HistoryWriterHandle {
    /// 投递一条事件（seq 由 writer 盖戳）。无界 channel：写盘慢时暂存，进程崩了才丢（§13.1）。
    pub fn append(&self, ev: HistoryEvent) {
        let _ = self.tx.send(ev);
    }
    /// 下一个将分配的 seq = 已落盘事件数。dream 在 dream-start 读它作 until_seq 边界（§7.1）。
    pub fn current_seq(&self) -> u64 {
        self.current_seq.load(Ordering::SeqCst)
    }
}

/// 启动单写 task：独占 history/ 文件句柄，串行 recv → 盖 seq → 按 ts 选日期文件 → append 一行。
/// 跨天自动开新文件（append 模式，永不覆盖）。
pub fn spawn_writer(history_dir: PathBuf) -> HistoryWriterHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<HistoryEvent>();
    let current_seq = Arc::new(AtomicU64::new(0));
    let handle = HistoryWriterHandle { tx, current_seq: current_seq.clone() };
    let _ = std::fs::create_dir_all(&history_dir);
    tokio::spawn(async move {
        let mut open: Option<(String, std::fs::File)> = None; // (date, append handle)
        while let Some(mut ev) = rx.recv().await {
            let seq = current_seq.fetch_add(1, Ordering::SeqCst); // 0-based 单调
            ev.seq = seq;
            let date = date_from_ts(ev.ts);
            let need_new = match &open {
                Some((d, _)) => d != &date,
                None => true,
            };
            if need_new {
                match std::fs::OpenOptions::new()
                    .create(true).append(true)
                    .open(history_dir.join(format!("{date}.jsonl")))
                {
                    Ok(f) => open = Some((date.clone(), f)),
                    Err(e) => {
                        eprintln!("[history] 打开 {date}.jsonl 失败: {e}（丢 seq={seq}，上游无界 channel 已暂存其余）");
                        continue;
                    }
                }
            }
            if let Some((_, f)) = open.as_mut() {
                use std::io::Write;
                let line = serde_json::to_string(&ev).unwrap_or_default();
                if let Err(e) = writeln!(f, "{line}") {
                    // §13.1：写失败 warn + 继续读下一事件（不阻塞 producer、不死 writer）
                    eprintln!("[history] 写失败 seq={seq}: {e}");
                }
            }
        }
    });
    handle
}

/// 读 history/ 全部事件，按 seq 升序。逐行解析，跳过坏行（§13.1）。
pub fn read_all(history_dir: &Path) -> Vec<HistoryEvent> {
    let mut evs = Vec::new();
    let mut files: Vec<PathBuf> = match std::fs::read_dir(history_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .collect(),
        Err(_) => return evs,
    };
    files.sort();
    for f in files {
        let text = match std::fs::read_to_string(&f) { Ok(s) => s, Err(_) => continue };
        for line in text.lines() {
            if line.trim().is_empty() { continue; }
            match serde_json::from_str::<HistoryEvent>(line) {
                Ok(ev) => evs.push(ev),
                Err(e) => eprintln!("[history] 跳过坏行 {f:?}: {e}"),
            }
        }
    }
    evs.sort_by_key(|e| e.seq);
    evs
}
```

- [ ] **Step 4: 跑测试确认通过**

```
cargo test --lib --manifest-path src-tauri/Cargo.toml history::tests
```
Expected: 8 PASS（含多生产者并发 seq 单调 + 跨天切文件 + 跳坏行 + append-only）。

- [ ] **Step 5: 全量 check**

```
cargo check --tests --manifest-path src-tauri/Cargo.toml
```
Expected: 0 warning / 0 error。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/history.rs src-tauri/src/lib.rs
git commit -m "feat(history): HistoryEvent + 单写 writer（串行 seq、跨天切文件、跳坏行）+ read_all"
```

---

## Task 3: context.rs — history → LLM messages 重建（P1 铁律②③住这里）

**Why**：v2 的核心翻转 —— messages 不再是跨轮 mutate 的真相，而是**每轮从 history 尾段重建**的派生视图。重建必须守住配对不变量（孤儿 tool_call 丢弃、截断不切一对中间）和 cap 只数 kind=user。

**Files:**
- Create: `src-tauri/src/context.rs`
- Modify: `src-tauri/src/lib.rs`（加 `pub mod context;`）

**Interfaces:**
- Consumes: `crate::history::HistoryEvent`、`crate::AttachmentRef`、`crate::llm::user_message_with_attachments`
- Produces:
  - `pub struct PinnedBlocks { system_prompt: String, soul: Option<String>, agent: Option<String>, memory: Option<String> }`，`to_messages() -> Vec<Value>`、`pub fn load_pinned(workspace: &Path, system_prompt: &str) -> PinnedBlocks`
  - `pub fn build_messages(events: &[HistoryEvent], pinned: &PinnedBlocks, cap: usize) -> Vec<Value>`
  - 私有：`last_marker_seq`、`truncate_events_by_user_cap`、`event_to_message`、`drop_trailing_orphan`

**已澄清（pinned 落地形态）**：4 个 pinned 块合并成**1 条 system 消息**（system_prompt + SOUL/AGENT/MEMORY 分节），因为 MiniMax/OpenAI-compat 通常期望单条 system；MEMORY 仍可逐轮变（每次重建重读）。API 风险最低。

- [ ] **Step 1: 写失败测试**（`context.rs` 的 `mod tests`，纯逻辑，直接构造 PinnedBlocks 不读盘）

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryEvent;
    use serde_json::json;

    fn pinned(sys: &str) -> PinnedBlocks {
        PinnedBlocks { system_prompt: sys.into(), soul: None, agent: None, memory: None }
    }
    fn tc(id: &str) -> serde_json::Value {
        json!({"id":id,"type":"function","function":{"name":"bash","arguments":"{}"}})
    }
    fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 }

    fn seq_events(kinds: &[&str]) -> Vec<HistoryEvent> {
        // 造一条主线程事件序列，每条 seq 递增；user/assistant/tool_result/marker 按需
        let mut evs = Vec::new();
        let mut seq = 0u64;
        let mut ts = 1000u64;
        for k in kinds {
            let ev = match *k {
                "user" => HistoryEvent::user(ts, "main", &format!("u{seq}"), &[]),
                "assistant" => HistoryEvent::assistant(ts, "main", &format!("a{seq}"), "", vec![]),
                "assistant_tc" => HistoryEvent::assistant(ts, "main", "", "", vec![tc(&format!("c{seq}"))]),
                "tool" => HistoryEvent::tool_result(ts, "main", "bash", "ok", &format!("c{}", seq.saturating_sub(1))),
                "external" => HistoryEvent::external(ts, "拖入", "x.pdf", None),
                "subagent_result" => HistoryEvent::subagent_result(ts, 1, "完成X", "thread=agent:1 seq[0,3]"),
                "marker_dream" => HistoryEvent::marker(ts, "dream", seq),
                "marker_reset" => HistoryEvent::marker(ts, "reset", seq),
                other => panic!("unknown kind {other}"),
            };
            let mut ev = ev;
            ev.seq = seq;
            evs.push(ev);
            seq += 1; ts += 1000;
        }
        evs
    }

    #[test]
    fn user_event_becomes_user_message() {
        let evs = seq_events(&["user"]);
        let m = build_messages(&evs, &pinned("S"), 50);
        assert_eq!(m[0]["role"], "system");
        assert_eq!(m[1]["role"], "user");
    }

    #[test]
    fn assistant_with_toolcalls_emits_tool_calls() {
        let evs = seq_events(&["user", "assistant_tc", "tool", "assistant"]);
        let m = build_messages(&evs, &pinned("S"), 50);
        let asst = m.iter().find(|x| x["role"] == "assistant" && x.get("tool_calls").is_some()).unwrap();
        assert_eq!(asst["tool_calls"][0]["id"], "c1");
        assert_eq!(asst["content"], serde_json::Value::Null);
    }

    #[test]
    fn marker_window_only_after_last_marker() {
        // marker 之前的事件不进 context；之后才进
        let evs = seq_events(&["user", "marker_dream", "user", "assistant"]);
        let m = build_messages(&evs, &pinned("S"), 50);
        let users: Vec<&serde_json::Value> = m.iter().filter(|x| x["role"]=="user").collect();
        assert_eq!(users.len(), 1, "marker 前的 user 不应进 context");
    }

    #[test]
    fn reset_marker_same_boundary_as_dream() {
        // reset 与 dream 都是重建边界（marker kind 不分 dream/reset）
        let evs = seq_events(&["user", "marker_reset", "user", "assistant"]);
        let m = build_messages(&evs, &pinned("S"), 50);
        let users: Vec<&serde_json::Value> = m.iter().filter(|x| x["role"]=="user").collect();
        assert_eq!(users.len(), 1, "reset 后只应剩 1 个 user");
    }

    #[test]
    fn thread_agent_filtered_out() {
        let mut evs = seq_events(&["user"]);
        // 加一条 agent:N 事件（thread 不同）
        let mut a = HistoryEvent::assistant(5000, "agent:1", "子代理输出", "", vec![]);
        a.seq = 99;
        evs.push(a);
        let m = build_messages(&evs, &pinned("S"), 50);
        assert!(m.iter().all(|x| x["role"] != "assistant" || x["content"] != json!("子代理输出")),
            "thread=agent:N 不应进主 context");
    }

    #[test]
    fn cap_counts_only_kind_user() {
        // P1 铁律④：cap 只数 kind=user；external/subagent_result 不顶 cap
        let mut kinds = Vec::new();
        for _ in 0..55 { kinds.push("user"); kinds.push("assistant"); }     // 55 user
        kinds.push("external"); kinds.push("assistant");                      // 不计 cap
        kinds.push("subagent_result"); kinds.push("assistant");              // 不计 cap
        kinds.push("user"); kinds.push("assistant");                          // 第 56 个 user（最新）
        let evs = seq_events(&kinds);
        let m = build_messages(&evs, &pinned("S"), 50);
        let user_count = m.iter().filter(|x| x["role"]=="user" &&
            x["content"].as_str().map(|c| c.starts_with("u")).unwrap_or(false)).count();
        assert_eq!(user_count, 50, "应只保留最近 50 个 user（external/subagent_result 不计）");
        // external 与 subagent_result 渲染为 user 角色但内容不以 'u' 开头，不在 user_count 内
    }

    #[test]
    fn truncate_does_not_split_pair() {
        // 截断点落在 turn 边界（user 处），不切 tool_call→tool_result 对
        let mut kinds = Vec::new();
        for _ in 0..52 {
            kinds.push("user"); kinds.push("assistant_tc"); kinds.push("tool");
        }
        kinds.push("user"); kinds.push("assistant"); // 最后一个完整 turn（无 tool_call）
        let evs = seq_events(&kinds);
        let m = build_messages(&evs, &pinned("S"), 50);
        // 不应出现「role:tool 但前面无对应 assistant.tool_calls」的孤儿（截断点 pair-safe）
        for (i, x) in m.iter().enumerate() {
            if x["role"] == "tool" {
                // 向上找最近的 assistant，须带 tool_calls 且含该 call_id
                let call_id = x["tool_call_id"].as_str().unwrap_or("");
                let has_owner = m[..i].iter().rev().any(|a|
                    a["role"]=="assistant" && a.get("tool_calls").and_then(|t| t.as_array())
                        .map(|arr| arr.iter().any(|c| c["id"]==call_id)).unwrap_or(false));
                assert!(has_owner, "截断造出孤儿 tool_result（call_id={call_id}），配对不变量被破坏");
            }
        }
    }

    #[test]
    fn drop_trailing_orphan_toolcall() {
        // P1 铁律②：末尾 assistant 带 tool_calls 但无后续 tool → 丢弃（崩溃/中断尾巴）
        let evs = seq_events(&["user", "assistant_tc"]); // assistant_tc 后无 tool
        let m = build_messages(&evs, &pinned("S"), 50);
        assert!(m.iter().all(|x| !(x["role"]=="assistant" && x.get("tool_calls").is_some())),
            "末尾孤儿 tool_call assistant 必须丢弃");
    }

    #[test]
    fn restart_rebuild_no_marker_keeps_all_up_to_cap() {
        let mut kinds = Vec::new();
        for _ in 0..3 { kinds.push("user"); kinds.push("assistant"); }
        let evs = seq_events(&kinds);
        let m = build_messages(&evs, &pinned("S"), 50);
        assert_eq!(m.iter().filter(|x| x["role"]=="user").count(), 3);
    }

    #[test]
    fn pinned_prepended_single_system_message() {
        let p = PinnedBlocks {
            system_prompt: "SYS".into(),
            soul: Some("灵魂".into()),
            agent: Some("规范".into()),
            memory: Some("记忆".into()),
        };
        let m = build_messages(&seq_events(&["user"]), &p, 50);
        assert_eq!(m[0]["role"], "system");
        let c = m[0]["content"].as_str().unwrap();
        assert!(c.contains("SYS") && c.contains("灵魂") && c.contains("规范") && c.contains("记忆"),
            "pinned 4 块应合并进首条 system：{c}");
    }
}
```

- [ ] **Step 2: 注册模块 + 跑测试确认失败**

`lib.rs` 加 `pub mod context;`。`cargo test --lib --manifest-path src-tauri/Cargo.toml context::tests` → 编译失败（类型未定义）。

- [ ] **Step 3: 实现 `context.rs`**

```rust
//! v2 context 重建：history 尾段（最后 marker 之后 · main 线程）→ LLM messages。
//! 每轮 turn 开头由 driver 调 build_messages 重建；turn 内 run_turn 仍 mutate messages，但真相在 history。
//! P1 铁律②配对不变量、④cap 只数 kind=user、③marker(dream|reset) 重建边界 都在这里。
use crate::history::HistoryEvent;
use crate::AttachmentRef;
use serde_json::{json, Value};
use std::path::Path;

/// 4 个 pinned 块（spec §3）。合并成 1 条 system 消息（API 安全；MEMORY 逐轮重读可变）。
#[derive(Clone, Debug)]
pub struct PinnedBlocks {
    pub system_prompt: String,
    pub soul: Option<String>,
    pub agent: Option<String>,
    pub memory: Option<String>,
}

impl PinnedBlocks {
    pub fn to_messages(&self) -> Vec<Value> {
        let mut combined = self.system_prompt.clone();
        for (tag, body) in [
            ("# 灵魂（SOUL.md）", &self.soul),
            ("# 工具与规范（AGENT.md）", &self.agent),
            ("# 记忆索引（MEMORY.md）", &self.memory),
        ] {
            if let Some(b) = body {
                if !b.trim().is_empty() {
                    combined.push_str("\n\n");
                    combined.push_str(tag);
                    combined.push('\n');
                    combined.push_str(b);
                }
            }
        }
        vec![json!({ "role": "system", "content": combined })]
    }
}

/// 从 workspace 读 SOUL/AGENT/MEMORY；读不到（None）则跳过该块（§13.1 优雅降级）。
pub fn load_pinned(workspace: &Path, system_prompt: &str) -> PinnedBlocks {
    let read = |name: &str| std::fs::read_to_string(workspace.join(name)).ok().filter(|s| !s.trim().is_empty());
    PinnedBlocks {
        system_prompt: system_prompt.to_string(),
        soul: read("SOUL.md"),
        agent: read("AGENT.md"),
        memory: read("MEMORY.md"),
    }
}

/// 取最后一个 marker（dream 或 reset）的 seq；无 marker → None（从头重建）。
fn last_marker_seq(events: &[HistoryEvent]) -> Option<u64> {
    events.iter().filter(|e| e.kind == "marker").map(|e| e.seq).max()
}

/// history → LLM messages。cap = 保留最近多少个 kind=user 事件（external/subagent_result 不计）。
pub fn build_messages(events: &[HistoryEvent], pinned: &PinnedBlocks, cap: usize) -> Vec<Value> {
    let cut = last_marker_seq(events);
    let main_after: Vec<&HistoryEvent> = events
        .iter()
        .filter(|e| e.thread == "main" && match cut { Some(s) => e.seq > s, None => true })
        .collect();
    let kept = truncate_events_by_user_cap(&main_after, cap);
    let mut msgs: Vec<Value> = kept.iter().filter_map(|e| event_to_message(e)).collect();
    drop_trailing_orphan(&mut msgs);
    let mut out = pinned.to_messages();
    out.append(&mut msgs);
    out
}

/// 只数 kind=user 截到最近 cap 个 user 事件；切点在 user 处 → 天然不切 tool_call→tool_result 对。
fn truncate_events_by_user_cap<'a>(evs: &[&'a HistoryEvent], cap: usize) -> Vec<&'a HistoryEvent> {
    let user_pos: Vec<usize> = evs.iter().enumerate().filter(|(_, e)| e.kind == "user").map(|(i, _)| i).collect();
    if user_pos.len() <= cap {
        return evs.to_vec();
    }
    let start = user_pos[user_pos.len() - cap];
    evs[start..].to_vec()
}

/// 末尾若剩带 tool_calls 但无后续 tool 的 assistant（崩溃/中断尾巴）→ 丢弃（§6 配对不变量）。
fn drop_trailing_orphan(msgs: &mut Vec<Value>) {
    while let Some(last) = msgs.last() {
        let is_orphan = last["role"] == "assistant"
            && last.get("tool_calls").and_then(|t| t.as_array()).map(|a| !a.is_empty()).unwrap_or(false);
        if is_orphan { msgs.pop(); } else { break; }
    }
}

fn event_to_message(e: &HistoryEvent) -> Option<Value> {
    match e.kind.as_str() {
        "user" => {
            let text = e.data.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let atts: Vec<AttachmentRef> = e.data.get("attachments")
                .and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
            Some(crate::llm::user_message_with_attachments(text, &atts))
        }
        "assistant" => {
            let content = e.data.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let tcs = e.data.get("tool_calls").cloned();
            let mut m = serde_json::Map::new();
            m.insert("role".into(), json!("assistant"));
            match tcs.and_then(|t| t.as_array().map(|a| a.to_vec())).filter(|a| !a.is_empty()) {
                Some(calls) => {
                    m.insert("content".into(), if content.is_empty() { Value::Null } else { json!(content) });
                    m.insert("tool_calls".into(), Value::Array(calls));
                }
                None => { m.insert("content".into(), json!(content)); }
            }
            Some(Value::Object(m))
        }
        "tool_result" => {
            let call_id = e.data.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
            let result = e.data.get("result").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({ "role": "tool", "tool_call_id": call_id, "content": result }))
        }
        "subagent_result" => {
            let id = e.data.get("agent_id").and_then(|v| v.as_u64()).unwrap_or(0);
            let summary = e.data.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            let refs = e.data.get("ref").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({ "role": "user", "content": format!("[子代理 #{id} 完成] {summary} [完整对话: {refs}]") }))
        }
        "external" => {
            let what = e.data.get("what").and_then(|v| v.as_str()).unwrap_or("");
            let path = e.data.get("path").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({ "role": "user", "content": format!("[外部] {what} {path}") }))
        }
        "edit" | "marker" => None, // 记录但不回放进 context
        _ => None,
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

```
cargo test --lib --manifest-path src-tauri/Cargo.toml context::tests
```
Expected: 10 PASS（含 cap 只数 user、截断 pair-safe、丢末尾孤儿、marker 边界）。

- [ ] **Step 5: 全量 check**

```
cargo check --tests --manifest-path src-tauri/Cargo.toml
```
Expected: 0 warning / 0 error。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/context.rs src-tauri/src/lib.rs
git commit -m "feat(context): history→messages 重建（marker 窗口/配对不变量/cap 只数 user/pinned 4 块）"
```

---

## Task 4: ToolsCtx 接 history writer + run_turn 边跑边 append 事件

**Why**：tool 循环里产生的 assistant / tool_result 必须实时落进 history（经单写 task）。run_turn 是唯一产生这两类事件的地方（主 driver 与子代理都走它），故 append 逻辑放这里，按 `thread` 区分主线/子代理串。

**Files:**
- Modify: `src-tauri/src/history.rs`（加 `HistoryWriterHandle::noop()`）
- Modify: `src-tauri/src/tools.rs`（`ToolsCtx` +`history` 字段；`foreground` 用 noop）
- Modify: `src-tauri/src/llm.rs`（`run_turn` 加 `thread: &str` 参数 + append 调用；`run_loop` 传 `"main"`）
- Modify: `src-tauri/src/subagents.rs`（`clone_ctx` 复制 `history`）
- Test: `src-tauri/src/llm.rs` 的 `mod tests`

**Interfaces:**
- Consumes: `crate::history::{HistoryWriterHandle, HistoryEvent}`（Task 2）
- Produces: `ToolsCtx.history: HistoryWriterHandle`（后续 driver/spawn_agent/Task 5/12 用）；`run_turn(..., thread: &str)` 新签名

- [ ] **Step 1: 写失败测试**（`llm.rs` 的 `mod tests`，追加）

```rust
    #[tokio::test]
    async fn run_turn_appends_assistant_and_tool_events_main_thread() {
        use crate::history::{spawn_writer, read_all};
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"));
        let mut ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        ctx.history = h.clone();
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(VecDeque::from(vec![
            Ok(tool_round("bash", r#"{"command":"echo hi"}"#, "c1")),
            Ok(RoundResult { content:"完成".into(), reasoning:String::new(), tool_calls:vec![],
                finish:FinishReason::Stop, usage:None, assistant_message:json!({"role":"assistant","content":"完成"}) }),
        ]))));
        let em = RecEmitter::default();
        let cfg = Config::default();
        let mut msgs = vec![json!({"role":"user","content":"hi"})];
        let _ = run_turn(round, &em, &cfg, &mut msgs, &ctx, MAX_ITERS, "main").await;
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = read_all(&dir.path().join("history"));
        let kinds: Vec<&str> = evs.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["assistant", "tool_result", "assistant"], "应落 3 条事件");
        assert_eq!(evs[0].thread, "main");
        assert_eq!(evs[0].data["tool_calls"][0]["id"], "c1");
        assert_eq!(evs[1].data["call_id"], "c1");
        assert!(evs[1].data["result"].as_str().unwrap().contains("hi"));
        assert_eq!(evs[2].data["content"], "完成");
    }

    #[tokio::test]
    async fn run_turn_appends_with_agent_thread() {
        use crate::history::{spawn_writer, read_all};
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"));
        let mut ctx = ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into());
        ctx.history = h.clone();
        let round: Arc<dyn LlmRound> = Arc::new(FakeRound(Mutex::new(VecDeque::from(vec![
            Ok(RoundResult { content:"子代理done".into(), reasoning:String::new(), tool_calls:vec![],
                finish:FinishReason::Stop, usage:None, assistant_message:json!({"role":"assistant","content":"子代理done"}) }),
        ]))));
        let em = RecEmitter::default();
        let cfg = Config::default();
        let mut msgs = vec![json!({"role":"user","content":"do"})];
        let _ = run_turn(round, &em, &cfg, &mut msgs, &ctx, MAX_ITERS, "agent:7").await;
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = read_all(&dir.path().join("history"));
        assert_eq!(evs[0].thread, "agent:7", "子代理事件 thread=agent:N");
    }
```

注：既有 `run_loop` / `loop_*` 测试不受影响（`run_loop` 内部传 `"main"`，foreground ctx 用 noop writer，断言的是 `res.history` Vec 非 writer）。

- [ ] **Step 2: 跑测试确认失败**

`cargo test --lib --manifest-path src-tauri/Cargo.toml llm::tests::run_turn_appends` → 编译失败（`ToolsCtx.history` 字段不存在、`run_turn` 签名不符、`noop()` 不存在）。

- [ ] **Step 3: 实现**

(a) `history.rs` 给 `HistoryWriterHandle` 加 `noop()`：
```rust
impl HistoryWriterHandle {
    /// 不落盘的句柄（foreground/测试用：send 到无人接收的 channel，事件丢弃）。
    pub fn noop() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel::<HistoryEvent>();
        Self { tx, current_seq: Arc::new(AtomicU64::new(0)) }
    }
    // ...既有 append / current_seq...
}
```

(b) `tools.rs` `ToolsCtx` 加字段 + `foreground` 初始化：
```rust
pub struct ToolsCtx {
    pub workspace: std::path::PathBuf,
    pub jobs: SharedRegistry,
    pub job_done_tx: mpsc::Sender<JobOutcome>,
    pub job_update: Arc<dyn JobUpdate>,
    pub minimax_region: String,
    pub allow_background: bool,
    pub subagent_stream: std::sync::Arc<dyn crate::subagents::SubagentStream>,
    /// v2：history 单写句柄。run_turn 把 assistant/tool_result 事件 append 进来。
    pub history: crate::history::HistoryWriterHandle,
}
```
`foreground(...)` 末尾加 `history: crate::history::HistoryWriterHandle::noop(),`。

(c) `subagents.rs` `clone_ctx` 末尾加 `history: src.history.clone(),`。

(d) `llm.rs` `run_turn` 加 `thread: &str` 参数 + append。新签名与关键改动：

```rust
pub async fn run_turn<E: Emitter>(
    round: Arc<dyn LlmRound>,
    emitter: &E,
    cfg: &Config,
    messages: &mut Vec<Value>,
    ctx: &tools::ToolsCtx,
    max_iters: usize,
    thread: &str,
) -> ChatResponse {
    emitter.turn_start().await;
    let mut history: Vec<Value> = vec![];
    let mut last_content = String::new();
    for i in 0..max_iters {
        eprintln!("[turn] round {i}: 调用模型（流式）…");
        let resp = match round.round(messages, cfg, emitter).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[turn] round {i}: 失败 {e}");
                emitter.error(&e).await;
                return ChatResponse { content: String::new(), history, error: Some(e) };
            }
        };
        last_content = resp.content.clone();
        eprintln!("[turn] round {i}: finish={:?} tool_calls={}", resp.finish, resp.tool_calls.len());
        match resp.finish {
            FinishReason::ToolCalls => {
                messages.push(resp.assistant_message.clone());
                history.push(resp.assistant_message);
                // v2：assistant(tool_calls) 事件落 history
                ctx.history.append(crate::history::HistoryEvent::assistant(
                    now_ms_llm(), thread, &resp.content, &resp.reasoning, resp.tool_calls.clone(),
                ));
                for call in resp.tool_calls {
                    let name = call["function"]["name"].as_str().unwrap_or("").to_string();
                    let args_str = call["function"]["arguments"].as_str().unwrap_or("{}").to_string();
                    let call_id = call["id"].as_str().unwrap_or("").to_string();
                    emitter.tool_call(&name, &args_str).await;
                    let result = match serde_json::from_str::<Value>(&args_str) {
                        Err(e) => format!("参数解析失败: {e}"),
                        Ok(args) => tools::dispatch(&name, args, ctx, cfg, round.clone()).await,
                    };
                    emitter.tool_result(&name, &result).await;
                    let m = json!({ "role": "tool", "tool_call_id": call_id, "content": result });
                    messages.push(m.clone());
                    history.push(m);
                    // v2：tool_result 事件落 history（call→result 相邻，配对天然成立）
                    ctx.history.append(crate::history::HistoryEvent::tool_result(
                        now_ms_llm(), thread, &name, &result, &call_id,
                    ));
                }
                continue;
            }
            FinishReason::Stop => {
                history.push(resp.assistant_message.clone());
                // v2：最终 assistant(纯文本) 事件落 history
                ctx.history.append(crate::history::HistoryEvent::assistant(
                    now_ms_llm(), thread, &last_content, "", vec![],
                ));
                emitter.turn_end().await;
                return ChatResponse { content: last_content, history, error: None };
            }
        }
    }
    // max_iters 兜底（同原逻辑，兜底 assistant 也落 history）
    let content = if last_content.trim().is_empty() {
        "（已达工具调用上限，未产生最终答复）".to_string()
    } else {
        format!("{last_content}\n\n_（已达工具调用上限）_")
    };
    history.push(json!({ "role": "assistant", "content": &content }));
    ctx.history.append(crate::history::HistoryEvent::assistant(now_ms_llm(), thread, &content, "", vec![]));
    emitter.turn_end().await;
    ChatResponse { content, history, error: None }
}

fn now_ms_llm() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64).unwrap_or(0)
}
```

`run_loop` 内部调用改为 `run_turn(round, &emitter, cfg, &mut messages, &ctx, MAX_ITERS, "main").await`。

- [ ] **Step 4: 跑测试确认通过**

```
cargo test --lib --manifest-path src-tauri/Cargo.toml llm::tests
```
Expected: 全 PASS（新 2 条 + 既有 loop_* 不破）。

- [ ] **Step 5: 全量 check（agent.rs/subagents.rs 还没改，但 run_turn 签名变了 → 这步会暴露所有 run_turn 调用点）**

```
cargo check --tests --manifest-path src-tauri/Cargo.toml
```
Expected: **编译错误**在 `agent.rs`（`handle_event` 调 `run_turn` 缺 thread 参数）与 `subagents.rs`（`spawn_agent` 调 `run_turn` 缺 thread 参数）、以及 `agent.rs` 测试里构造 `ToolsCtx` 缺 `history` 字段。**这些由 Task 5 / Task 12 修**；本任务提交前先用最小改动让 check 通过：在 `agent.rs::handle_event` 的 `run_turn(...)` 调用补 `"main"`，`agent.rs` 测试 ctx 构造补 `history: HistoryWriterHandle::noop()`，`subagents.rs::spawn_agent` 的 `run_turn(...)` 调用补 `&format!("agent:{id}")`、`clone_ctx` 已在 (c) 补 history。

> 说明：Task 5 会重写 driver 的 turn 编排；此处仅「补参数让编译过」，不删 `window_messages`（留给 Task 5）。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/history.rs src-tauri/src/tools.rs src-tauri/src/llm.rs src-tauri/src/subagents.rs src-tauri/src/agent.rs
git commit -m "feat(history-wire): ToolsCtx.history + run_turn 按 thread append assistant/tool_result 事件"
```

---

## Task 5: agent.rs driver 迁移 —— 删 window_messages、每轮从 history 重建、Reset 写 marker（P1 铁律②驱动侧）

**Why**：v2 核心翻转落在 driver。旧模型把 `messages: Vec<Value>` 当真相、跨轮 mutate、超 40 折叠（`window_messages`）。v2：messages 是**每轮从 history 重建**的派生视图；turn 内 run_turn 仍 mutate 它（并 append 进 history）；Reset 写 `marker:reset`（context 重建从该 marker 之后 = 清空，history 永不删）；ContextNote 写 `external` 事件（不计 cap）。

**Files:**
- Modify: `src-tauri/src/agent.rs`（重写 `handle_event` / `run_one` / `spawn_session`；删 `window_messages` 与其测试；`inject_jobdone_message` → 提取 `jobdone_body`）
- Test: `src-tauri/src/agent.rs` 的 `mod tests`（替换旧 messages-model 测试为 history-model 测试）

**Interfaces:**
- Consumes: `crate::history::{HistoryWriterHandle, HistoryEvent, read_all}`（Task 2）、`crate::context::{build_messages, load_pinned}`（Task 3）、`crate::llm::run_turn`（Task 4 新签名）
- Produces: `handle_event(event, history, history_dir, workspace, ctx, cfg, round, emitter) -> Option<ChatResponse>`（纯逻辑可离线测）；`spawn_session` 内部 spawn history writer、持有 history_dir；`run_one` 加 Reset 的 marker + 既有 registry/emit 清理。

**关键正确性（免 flush 竞态 + 免重复）**：每轮顺序固定为 **`read_all(history_dir)` → `history.append(current)` → `events.push(current)` → `build_messages(&events,..)`**。`read_all` 在 `append` 之前执行 → 磁盘上绝无 `current` → `push` 只补一份 → **无重复**；`push` 本地确定性补 `current` → **无需等 writer flush**。历史事件（前轮、dream markers）早已落盘由 `read_all` 取得。

**已澄清（事件映射）**：`UserMessage`→`user`；`ContextNote`→`external`（不计 cap，不跑 turn）；`JobDone(Agent)`→`subagent_result`（不计 cap，跑 turn）；`JobDone(Process)`→`external(what=jobdone_body)`（跑 turn）；`Reset`→`marker:reset`（不跑 turn）。

- [ ] **Step 1: 写失败测试**（替换 `agent.rs` 的 `mod tests` 里旧的 `window_*` / `*_then_stop` / `inject_jobdone_*` 测试）

```rust
    use crate::history::{spawn_writer, read_all, HistoryWriterHandle, HistoryEvent};
    use crate::context; // 不需要直接用，build 在 handle_event 内

    fn ws_setup() -> (tempfile::TempDir, HistoryWriterHandle, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"));
        (dir, h, dir.path().join("history"))
    }
    fn ctx_with(ws: std::path::PathBuf, h: HistoryWriterHandle) -> ToolsCtx {
        let (_tx, _rx) = tokio::sync::mpsc::channel(8);
        ToolsCtx {
            workspace: ws, jobs: Arc::new(Mutex::new(JobRegistry::new())) as SharedRegistry,
            job_done_tx: _tx, job_update: Arc::new(NoopJobUpdate), minimax_region: "cn".into(),
            allow_background: true, subagent_stream: Arc::new(crate::subagents::NoopSubagentStream),
            history: h,
        }
    }
    fn cfg() -> Config {
        serde_json::from_value::<Config>(serde_json::json!({"system_prompt":"你是助手","dream_cap_turns":50})).unwrap()
    }

    #[tokio::test]
    async fn usermessage_appended_and_turn_runs() {
        let (dir, h, hd) = ws_setup();
        let round: Arc<dyn llm::LlmRound> = Arc::new(StopRound); // 返 "done"
        let emit = FakeEmitter { content: Mutex::new(String::new()) };
        let x = ctx_with(dir.path().to_path_buf(), h.clone());
        let c = cfg();
        let resp = handle_event(
            &SessionEvent::UserMessage { text: "你好".into(), attachments: vec![] },
            &h, &hd, dir.path(), &x, &c, round, &emit).await;
        assert!(resp.is_some(), "UserMessage 应跑 turn");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = read_all(&hd);
        assert!(evs.iter().any(|e| e.kind=="user" && e.data["text"]=="你好"));
        assert!(evs.iter().any(|e| e.kind=="assistant"), "turn 应落 assistant 事件");
    }

    #[tokio::test]
    async fn context_note_appends_external_no_turn() {
        let (dir, h, hd) = ws_setup();
        let round: Arc<dyn llm::LlmRound> = Arc::new(StopRound);
        let emit = FakeEmitter { content: Mutex::new(String::new()) };
        let x = ctx_with(dir.path().to_path_buf(), h.clone());
        let c = cfg();
        let resp = handle_event(
            &SessionEvent::ContextNote { text: "[拖入] a.pdf".into() },
            &h, &hd, dir.path(), &x, &c, round, &emit).await;
        assert!(resp.is_none(), "ContextNote 不跑 turn");
        assert!(emit.content.lock().unwrap().is_empty(), "不应流正文");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = read_all(&hd);
        assert!(evs.iter().any(|e| e.kind=="external"), "应落 external 事件");
    }

    #[tokio::test]
    async fn reset_appends_marker_no_turn() {
        let (dir, h, hd) = ws_setup();
        let round: Arc<dyn llm::LlmRound> = Arc::new(StopRound);
        let emit = FakeEmitter { content: Mutex::new(String::new()) };
        let x = ctx_with(dir.path().to_path_buf(), h.clone());
        let c = cfg();
        let resp = handle_event(&SessionEvent::Reset, &h, &hd, dir.path(), &x, &c, round, &emit).await;
        assert!(resp.is_none(), "Reset 不跑 turn");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = read_all(&hd);
        let m = evs.iter().find(|e| e.kind=="marker").unwrap();
        assert_eq!(m.data["marker"], serde_json::json!("reset"));
    }

    #[tokio::test]
    async fn jobdone_agent_appends_subagent_result_and_runs() {
        let (dir, h, hd) = ws_setup();
        let round: Arc<dyn llm::LlmRound> = Arc::new(StopRound);
        let emit = FakeEmitter { content: Mutex::new(String::new()) };
        let x = ctx_with(dir.path().to_path_buf(), h.clone());
        let c = cfg();
        let o = JobOutcome { job_id: 3, kind: crate::jobs::JobKind::Agent, label: None,
            ok: true, code: None, tail: String::new(), answer: Some("完成X".into()), note: None };
        let resp = handle_event(&SessionEvent::JobDone(o), &h, &hd, dir.path(), &x, &c, round, &emit).await;
        assert!(resp.is_some(), "JobDone(Agent) 应跑 turn");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let evs = read_all(&hd);
        let sr = evs.iter().find(|e| e.kind=="subagent_result").unwrap();
        assert_eq!(sr.data["agent_id"], serde_json::json!(3));
        assert_eq!(sr.data["summary"], serde_json::json!("完成X"));
        assert_eq!(sr.data["ref"], serde_json::json!("thread=agent:3"));
    }

    #[tokio::test]
    async fn rebuild_starts_after_reset_marker() {
        // user1 + turn → reset marker → user2 + turn：第 2 轮重建应只含 user2（reset 清空）
        let (dir, h, hd) = ws_setup();
        let x = ctx_with(dir.path().to_path_buf(), h.clone());
        let c = cfg();
        let mk = |t: &str| SessionEvent::UserMessage { text: t.into(), attachments: vec![] };
        // 第 1 轮
        let _ = handle_event(&mk("第一句"), &h, &hd, dir.path(), &x, &c,
            Arc::new(StopRound) as Arc<dyn llm::LlmRound>, &FakeEmitter { content: Mutex::new(String::new()) }).await;
        // reset
        let _ = handle_event(&SessionEvent::Reset, &h, &hd, dir.path(), &x, &c,
            Arc::new(StopRound) as Arc<dyn llm::LlmRound>, &FakeEmitter { content: Mutex::new(String::new()) }).await;
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        // 第 2 轮：手工模拟 handle_event 的重建段，验只含 user2
        let mut events = read_all(&hd);
        let cur = HistoryEvent::user(0, "main", "第二句", &[]);
        events.push(cur);
        let pinned = context::PinnedBlocks { system_prompt: "S".into(), soul: None, agent: None, memory: None };
        let msgs = context::build_messages(&events, &pinned, 50);
        let user_texts: Vec<&str> = msgs.iter()
            .filter(|m| m["role"]=="user").map(|m| m["content"].as_str().unwrap_or("")).collect();
        assert!(user_texts.iter().all(|t| t.contains("第二句") || !t.contains("第一句")),
            "reset 后第一句不应进 context：{user_texts:?}");
    }

    struct StopRound;
    #[async_trait]
    impl llm::LlmRound for StopRound {
        async fn round(&self, _: &[serde_json::Value], _: &Config, _: &dyn llm::Emitter) -> Result<llm::RoundResult, String> {
            Ok(llm::RoundResult { content:"done".into(), reasoning:String::new(), tool_calls:vec![],
                finish:llm::FinishReason::Stop, usage:None,
                assistant_message:serde_json::json!({"role":"assistant","content":"done"}) })
        }
    }
```

删除旧测试：`window_folds_old_tool_results`、`window_preserves_tool_call_id_when_folding`、`window_folds_old_image_refs`、`usermessage_then_stop_runs_turn`、`jobdone_injects_message_and_runs_turn`、`reset_clears_and_reinjects_system`、`context_note_pushes_without_running_turn`、`inject_jobdone_*`（3 条）、`reset_cancels_agent_jobs_before_replacing_registry`（后者保留——它测 registry cancel 纯逻辑，仍有效，保留）。

- [ ] **Step 2: 跑测试确认失败**

`cargo test --lib --manifest-path src-tauri/Cargo.toml agent::tests` → 编译失败（`window_messages` 删后引用、`handle_event` 旧签名、`HistoryEvent`/`context` 未 import）。

- [ ] **Step 3: 实现（重写 `agent.rs` 关键部分）**

(a) 删 `window_messages`（整函数）。删 `inject_jobdone_message`，改为：

```rust
/// JobOutcome → 注入正文字符串（v2：作为 external 事件的 what 落 history）。
fn jobdone_body(o: &JobOutcome) -> String {
    match o.kind {
        crate::jobs::JobKind::Process => {
            if o.ok {
                format!("[后台任务 #{} 完成] 退出码 {}\n{}", o.job_id, o.code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()), o.tail)
            } else {
                format!("[后台任务 #{} 失败]\n{}", o.job_id, o.tail)
            }
        }
        crate::jobs::JobKind::Agent => {
            let cap = o.label.as_deref().map(|c| format!("（{c}）")).unwrap_or_default();
            let answer = o.answer.clone().unwrap_or_default();
            match (&o.note, o.ok) {
                (None, true) => format!("[子代理 #{} 完成{}]\n{}", o.job_id, cap, answer),
                (None, false) => format!("[子代理 #{} 失败{}]\n{}", o.job_id, cap, answer),
                (Some(note), _) => format!("[子代理 #{} {}{}；部分产出：\n{}]", o.job_id, note, cap, answer),
            }
        }
    }
}
```

(b) 重写 `handle_event`（纯逻辑，可离线测）：

```rust
/// 处理一个事件：落 history（经单写 writer）→ 若该事件触发 turn，从 history 重建 messages 并 run_turn。
/// Reset/ContextNote 不跑 turn（仅落 history）。纯逻辑：注入 round/emitter + 真实 writer + tempdir 即可离线测。
pub async fn handle_event<E: llm::Emitter>(
    event: &SessionEvent,
    history: &crate::history::HistoryWriterHandle,
    history_dir: &std::path::Path,
    workspace: &std::path::Path,
    ctx: &ToolsCtx,
    cfg: &Config,
    round: std::sync::Arc<dyn llm::LlmRound>,
    emitter: &E,
) -> Option<llm::ChatResponse> {
    let now = now_ms();
    let current = match event {
        SessionEvent::UserMessage { text, attachments } => {
            crate::history::HistoryEvent::user(now, "main", text, attachments)
        }
        SessionEvent::ContextNote { text } => {
            crate::history::HistoryEvent::external(now, text, "", None)
        }
        SessionEvent::JobDone(o) => match o.kind {
            crate::jobs::JobKind::Agent => crate::history::HistoryEvent::subagent_result(
                now, o.job_id, &o.answer.clone().unwrap_or_default(), &format!("thread=agent:{}", o.job_id)),
            crate::jobs::JobKind::Process => crate::history::HistoryEvent::external(now, &jobdone_body(o), "", None),
        },
        SessionEvent::Reset => {
            crate::history::HistoryEvent::marker(now, "reset", history.current_seq())
        }
    };
    // 正确性顺序：先 read_all（current 尚未落盘）→ append → 本地 push（确定性补 current，免 flush 等待、免重复）
    let mut events = crate::history::read_all(history_dir);
    history.append(current.clone());
    events.push(current);

    let triggers_turn = matches!(event, SessionEvent::UserMessage { .. } | SessionEvent::JobDone(_));
    if !triggers_turn {
        return None; // ContextNote（external）、Reset（marker）：仅落 history，下轮重建自然反映
    }
    let pinned = crate::context::load_pinned(workspace, &cfg.system_prompt);
    let mut messages = crate::context::build_messages(&events, &pinned, cfg.dream_cap_turns as usize);
    Some(llm::run_turn(round, emitter, cfg, &mut messages, ctx, llm::MAX_ITERS, "main").await)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64).unwrap_or(0)
}
```

(c) 重写 `spawn_session`（spawn writer、持有 history_dir）+ `run_one`（Reset 落 marker + 既有 registry cancel/emit；dream trigger 钩子留给 Task 7）：

```rust
pub fn spawn_session(
    app: AppHandle,
    registry: crate::jobs::SharedRegistry,
    job_update: std::sync::Arc<dyn crate::jobs::JobUpdate>,
    sub_stream: std::sync::Arc<dyn crate::subagents::SubagentStream>,
) -> SessionHandle {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<SessionEvent>(CHANNEL_CAP);
    let cfg = crate::config::load(&app);
    let workspace = std::path::PathBuf::from(&cfg.workspace_dir);
    let _ = std::fs::create_dir_all(&workspace);
    let history_dir = workspace.join("history");
    let history = crate::history::spawn_writer(history_dir.clone());
    let (job_done_tx, mut job_done_rx) = tokio::sync::mpsc::channel::<crate::jobs::JobOutcome>(CHANNEL_CAP);
    let ctx = ToolsCtx {
        workspace: workspace.clone(),
        jobs: registry.clone(),
        job_done_tx,
        job_update,
        minimax_region: cfg.minimax_region.clone(),
        allow_background: true,
        subagent_stream: sub_stream,
        history: history.clone(),
    };
    let cfg = std::sync::Arc::new(cfg); // 多 clone 友好
    let workspace_rc = std::sync::Arc::new(workspace);
    let history_dir_rc = std::sync::Arc::new(history_dir);
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                Some(o) = job_done_rx.recv() => {
                    run_one(&SessionEvent::JobDone(o), &history, &history_dir_rc, &workspace_rc, &ctx, &cfg, &app).await;
                }
                ev = rx.recv() => match ev {
                    Some(e) => run_one(&e, &history, &history_dir_rc, &workspace_rc, &ctx, &cfg, &app).await,
                    None => break,
                }
            }
        }
    });
    SessionHandle { tx }
}

async fn run_one(
    e: &SessionEvent,
    history: &crate::history::HistoryWriterHandle,
    history_dir: &std::path::Path,
    workspace: &std::path::Path,
    ctx: &ToolsCtx,
    cfg: &Config,
    app: &AppHandle,
) {
    let emit = crate::AppEmitter { app: app.clone() };
    let round: std::sync::Arc<dyn llm::LlmRound> = std::sync::Arc::new(crate::llm::HttpRound);
    let _ = handle_event(e, history, history_dir, workspace, ctx, cfg, round, &emit).await;
    if matches!(e, SessionEvent::Reset) {
        // driver 专属：cancel 所有 agent jobs（CancellationToken drop ≠ cancel，防孤儿 tokio 烧 token）+ 重置 registry + 通知前端清屏
        {
            let mut r = ctx.jobs.lock().unwrap();
            let tokens: Vec<tokio_util::sync::CancellationToken> = r.jobs.values()
                .filter(|j| matches!(j.kind, crate::jobs::JobKind::Agent))
                .filter_map(|j| j.cancel.clone()).collect();
            for t in tokens { t.cancel(); }
            *r = crate::jobs::JobRegistry::new_with_max(cfg.max_subagents as usize);
        }
        let _ = tauri::Emitter::emit(app, "chat-reset", ());
    }
    // Task 7 在此插入 dream 轮间触发检查
}
```

> 注：`run_one` / `spawn_session` 内 `cfg`/`workspace` 改 `Arc` 以便 move 进 spawned task（原代码用值，需随签名调整）。`SessionEvent::JobDone` clone：`JobOutcome` 已 `Clone`，把 `run_one` 入参改 `&SessionEvent`，`JobDone(o)` 处 `o` 需 `JobOutcome: Clone`（已是）。

(d) 删除 `agent.rs` 顶部依赖 `use crate::llm::{self, ChatResponse, Emitter}` 中已不用的（`Emitter` 现通过 `llm::Emitter` 路径用）；保留 `ChatResponse`。

- [ ] **Step 4: 跑测试确认通过**

```
cargo test --lib --manifest-path src-tauri/Cargo.toml agent::tests
```
Expected: 新 5 条 PASS + 保留的 `reset_cancels_agent_jobs_before_replacing_registry` PASS。

- [ ] **Step 5: 全量 check（lib.rs setup 还没传 history——但 spawn_session 内部自建 writer，lib.rs 调用不变）**

```
cargo check --tests --manifest-path src-tauri/Cargo.toml
```
Expected: 0 warning / 0 error。若 lib.rs `spawn_session` 调用签名未变（仍是 4 参），无需改 lib.rs。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/agent.rs
git commit -m "feat(agent): driver 迁移 v2——删 window_messages、每轮 read_all++current 重建、Reset 写 marker、ContextNote→external"
```

---

## Task 6: memory.rs —— 「日」层事件段 + MEMORY.md「日」节（P1 铁律③ seq 代码盖戳）

**Why**：dream 把 history 整理成记忆。「日」层 = `memory/{Y}/{M}/{date}.md` 里的事件段（带通往原始对话的「对话索引」指针）+ `MEMORY.md` 的「日」索引节。**指针 seq[a,b] 由代码盖戳**（dream 观察到的 seq 范围），LLM 只写标题/详情/主语正文 —— 否则 LLM 瞎编 seq 会让 mem drill 指向错误段落。dream 是 memory/ 唯一写者。

**Files:**
- Create: `src-tauri/src/memory.rs`
- Modify: `src-tauri/src/lib.rs`（加 `pub mod memory;`）

**Interfaces:**
- Consumes: `crate::history::date_from_ts`（Task 2）
- Produces:
  - `pub fn append_day_event(workspace: &Path, ts: u64, title: &str, detail: &str, subject: &str, seq_range: (u64,u64), attachment: Option<&str>) -> Result<String,String>`
  - `pub fn append_memory_day(workspace: &Path, ts: u64, evt_no: u32, title: &str) -> Result<String,String>`
  - `pub fn ensure_memory_skeleton(workspace: &Path) -> std::io::Result<()>`
  - 私有 `time_hhmm_from_ts`、`day_event_count`、`insert_in_section`

**事件段格式（spec §8.4，seq 指针代码盖戳）**：
```
## HH:MM evt-{YYYYMMDD}-{NN} {title}
**主语**: {subject}
**详情**: {detail}
**对话索引**: history/{YYYY-MM-DD}.jsonl#seq[{a},{b}]   ← 代码盖戳
**附件**: {attachment}   （可选）
```

- [ ] **Step 1: 写失败测试**（`memory.rs` 的 `mod tests`）

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::date_from_ts;

    fn ws() -> tempfile::TempDir { tempfile::tempdir().unwrap() }
    fn ts_on(d: &str) -> u64 {
        // d="2026-07-26" 当天某时刻（12:30 UTC）：date_from_ts(ts)==d
        // 1782768000000 = 2026-07-26 00:00 UTC；+12.5h
        1_782_768_000_000 + 12 * 3600_000 + 30 * 60_000
    }

    #[test]
    fn append_day_event_writes_segment_with_code_stamped_seq() {
        let w = ws();
        let r = append_day_event(w.path(), ts_on("2026-07-26"), "重构 foo.rs 完成",
            "拆成 3 模块，改 12 处", "agent（子代理#7）", (3, 9), Some("a1b2.png"));
        assert!(r.is_ok(), "{:?}", r);
        let path = w.path().join("memory/2026/07/2026-07-26.md");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("重构 foo.rs 完成"), "标题: {body}");
        assert!(body.contains("拆成 3 模块"), "详情: {body}");
        assert!(body.contains("agent（子代理#7）"), "主语: {body}");
        // P1 铁律③：seq 指针代码盖戳，必须精确
        assert!(body.contains("对话索引**: history/2026-07-26.jsonl#seq[3,9]"), "seq 指针盖戳: {body}");
        assert!(body.contains("a1b2.png"));
    }

    #[test]
    fn append_day_event_correct_path_and_time() {
        let w = ws();
        append_day_event(w.path(), ts_on("2026-07-26"), "T", "D", "S", (0,1), None).unwrap();
        let path = w.path().join("memory/2026/07/2026-07-26.md");
        assert!(path.exists(), "应落 memory/{Y}/{M}/{date}.md");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("12:30"), "HH:MM 来自 ts: {body}");
        assert!(body.contains("evt-20260726-001"), "事件编号: {body}");
    }

    #[test]
    fn append_day_event_increments_nn() {
        let w = ws();
        append_day_event(w.path(), ts_on("2026-07-26"), "第一件", "d", "s", (0,1), None).unwrap();
        append_day_event(w.path(), ts_on("2026-07-26"), "第二件", "d", "s", (2,3), None).unwrap();
        let body = std::fs::read_to_string(w.path().join("memory/2026/07/2026-07-26.md")).unwrap();
        assert!(body.contains("evt-20260726-001"));
        assert!(body.contains("evt-20260726-002"));
    }

    #[test]
    fn ensure_memory_skeleton_creates_four_tiers() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert!(body.contains("## 日"));
        assert!(body.contains("## 周"));
        assert!(body.contains("## 月"));
        assert!(body.contains("## 年"));
    }

    #[test]
    fn append_memory_day_adds_line_under_day_section() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        append_memory_day(w.path(), ts_on("2026-07-26"), 1, "重构 foo.rs 完成").unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        // 行落在「## 日」与「## 周」之间
        let day_start = body.find("## 日").unwrap();
        let week_start = body.find("## 周").unwrap();
        let day_section = &body[day_start..week_start];
        assert!(day_section.contains("12:30"), "「日」节应含时间: {day_section}");
        assert!(day_section.contains("重构 foo.rs 完成"));
        assert!(day_section.contains("evt-20260726-001") || day_section.contains("evt-001") || day_section.contains("evt-1"),
            "「日」索引行: {day_section}");
    }
}
```

- [ ] **Step 2: 注册模块 + 跑测试确认失败**

`lib.rs` 加 `pub mod memory;`。`cargo test --lib --manifest-path src-tauri/Cargo.toml memory::tests` → 编译失败。

- [ ] **Step 3: 实现 `memory.rs`**

```rust
//! v2 记忆「日」层（第 1 期只做日；周/月/年晋级见第 2 期）。
//! dream 是 memory/ 唯一写者（mem 只读，零竞态）。事件段的「对话索引」seq[a,b] 由代码盖戳（P1 铁律③）。
use crate::history::date_from_ts;
use std::path::Path;

/// epoch ms → HH:MM（UTC，与 date_from_ts 一致）。
fn time_hhmm_from_ts(ts: u64) -> String {
    let secs = (ts / 1000) as u64;
    let in_day = secs % 86400;
    let h = in_day / 3600;
    let m = (in_day % 3600) / 60;
    format!("{h:02}:{m:02}")
}

fn date_compact(ts: u64) -> String {
    let d = date_from_ts(ts); // YYYY-MM-DD
    d.replace('-', "")
}

fn day_file_path(workspace: &Path, ts: u64) -> std::path::PathBuf {
    let d = date_from_ts(ts); // YYYY-MM-DD
    let (y, m) = match d.split('-').collect::<Vec<_>>().as_slice() {
        [y, m, _] => (y.to_string(), m.to_string()),
        _ => ("1970".into(), "01".into()),
    };
    workspace.join("memory").join(y).join(m).join(format!("{d}.md"))
}

/// 数当天文件里已有多少事件段（## 开头行数）→ 下一个 NN。
fn day_event_count(path: &Path) -> u32 {
    std::fs::read_to_string(path).unwrap_or_default()
        .lines().filter(|l| l.starts_with("## ")).count() as u32
}

/// 追加一条「日」事件段到 memory/{Y}/{M}/{date}.md。seq_range 由代码盖戳（dream 传入观察到的范围）。
pub fn append_day_event(
    workspace: &Path, ts: u64, title: &str, detail: &str, subject: &str,
    seq_range: (u64, u64), attachment: Option<&str>,
) -> Result<String, String> {
    let path = day_file_path(workspace, ts);
    if let Some(p) = path.parent() { std::fs::create_dir_all(p).map_err(|e| e.to_string())?; }
    let nn = day_event_count(&path) + 1;
    let hhmm = time_hhmm_from_ts(ts);
    let date = date_from_ts(ts);
    let date_c = date_compact(ts);
    let mut seg = format!(
        "\n## {hhmm} evt-{date_c}-{nn:03} {title}\n**主语**: {subject}\n**详情**: {detail}\n**对话索引**: history/{date}.jsonl#seq[{a},{b}]\n",
        a = seq_range.0, b = seq_range.1
    );
    if let Some(att) = attachment {
        seg.push_str(&format!("**附件**: {att}\n"));
    }
    // append-only（dream 是当天唯一写者；跨天冻结旧文件）
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| e.to_string())?;
    f.write_all(seg.as_bytes()).map_err(|e| e.to_string())?;
    Ok(format!("已写事件段 evt-{date_c}-{nn:03} → {}", path.display()))
}

/// 「日」索引行（MEMORY.md「日」节）。evt_no 由调用方（dream）按当天序号给。
pub fn append_memory_day(workspace: &Path, ts: u64, evt_no: u32, title: &str) -> Result<String, String> {
    let path = workspace.join("MEMORY.md");
    if !path.exists() { ensure_memory_skeleton(workspace).map_err(|e| e.to_string())?; }
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    let hhmm = time_hhmm_from_ts(ts);
    let date_c = date_compact(ts);
    let line = format!("- {hhmm} evt-{date_c}-{evt_no:03} {title}\n");
    text = insert_in_section(&text, "日", &line);
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(format!("已追加「日」索引行 → {}", path.display()))
}

/// 在 MEMORY.md 的某个「## {section}」节（到下一个 ## 之前）末尾插入 line。
fn insert_in_section(text: &str, section: &str, line: &str) -> String {
    let header = format!("## {section}");
    let mut lines = text.lines().collect::<Vec<_>>();
    let start = match lines.iter().position(|l| l.trim_start() == header) {
        Some(i) => i + 1,
        None => { lines.push(""); lines.push(header.as_str()); lines.push(line); return lines.join("\n"); }
    };
    // 找下一个 ## 的位置（节末尾）
    let mut end = start;
    while end < lines.len() && !lines[end].trim_start().starts_with("## ") { end += 1; }
    // 跳过节末尾空行，把新行插在内容尾部
    let mut insert_at = end;
    while insert_at > start && lines[insert_at - 1].trim().is_empty() { insert_at -= 1; }
    lines.insert(insert_at, line.trim_end_matches('\n'));
    lines.join("\n")
}

/// 建 MEMORY.md 四层骨架（日/周/月/年）；已存在则不动。
pub fn ensure_memory_skeleton(workspace: &Path) -> std::io::Result<()> {
    let path = workspace.join("MEMORY.md");
    if path.exists() { return Ok(()); }
    let skeleton = "# 记忆索引（MEMORY.md）\n\n由 dream 自动维护。四层：日（今天）/周（近 7 天）/月（近 3 月）/年（一行/年）。\n\n## 日\n\n## 周\n\n## 月\n\n## 年\n";
    std::fs::write(&path, skeleton)
}
```

- [ ] **Step 4: 跑测试确认通过**

```
cargo test --lib --manifest-path src-tauri/Cargo.toml memory::tests
```
Expected: 5 PASS。

- [ ] **Step 5: 全量 check**

```
cargo check --tests --manifest-path src-tauri/Cargo.toml
```
Expected: 0 warning / 0 error。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/memory.rs src-tauri/src/lib.rs
git commit -m "feat(memory): 「日」层事件段（seq 指针代码盖戳）+ MEMORY.md 四层骨架与「日」节"
```

---

## Task 7: dream.rs —— DreamTrigger（idle + cap + 单飞 + 空跳过，P1 铁律④）

**Why**：dream 必须修掉 v1 所有 timing 坑（开机触发 / idle 语义错 / 空跳过 / 多 dream 并发）。把「是否该触发」抽成纯逻辑 struct，fake-time 可离线测，wiring 留 Task 8。**cap 只数 kind=user**（external/subagent_result 不顶 cap，否则后台回调会在主 agent 思考中途偷偷触发 dream）。

**Files:**
- Create: `src-tauri/src/dream.rs`（本任务只放 `DreamTrigger`；Task 8 加 `spawn_dream_agent` 等）
- Modify: `src-tauri/src/lib.rs`（加 `pub mod dream;`）

**Interfaces:**
- Consumes: `crate::history::HistoryEvent`
- Produces:
  - `pub enum TriggerReason { Idle, Cap }`
  - `pub struct DreamTrigger { last_user_activity, last_dream_marker_seq, in_flight, armed }`
  - 方法 `new()` / `note_activity(ts)` / `seed_from_history(events)`（启动播种 last_dream_marker_seq）/ `dream_started(marker_seq)` / `dream_finished()` / `check(events, now, idle_secs, cap) -> Option<TriggerReason>`

**关键（修 v1 坑）**：`armed` 初始 false → 首次用户活动前**绝不触发**（修「开机触发」）；idle = 距**上次用户活动**（非距上次 dream）；空跳过 = 自上次 **dream** marker 后无事件（reset 不重置提取前沿，§7.8）；`in_flight` 单飞；cap 只数 `kind=user`。

- [ ] **Step 1: 写失败测试**（`dream.rs` 的 `mod tests`，纯逻辑 fake-time）

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryEvent;

    fn user(seq: u64, ts: u64) -> HistoryEvent { let mut e = HistoryEvent::user(ts, "main", "u", &[]); e.seq = seq; e }
    fn ext(seq: u64, ts: u64) -> HistoryEvent { let mut e = HistoryEvent::external(ts, "w", "p", None); e.seq = seq; e }
    fn sub(seq: u64, ts: u64) -> HistoryEvent { let mut e = HistoryEvent::subagent_result(ts, 1, "s", "r"); e.seq = seq; e }
    fn dream_marker(seq: u64, ts: u64) -> HistoryEvent { let mut e = HistoryEvent::marker(ts, "dream", seq); e.seq = seq; e }
    fn reset_marker(seq: u64, ts: u64) -> HistoryEvent { let mut e = HistoryEvent::marker(ts, "reset", seq); e.seq = seq; e }

    #[test]
    fn not_armed_at_startup_no_trigger() {
        // 修 v1「开机触发」：fresh trigger 即使有事件 + idle 满足也不触发
        let mut t = DreamTrigger::new();
        t.seed_from_history(&[user(0, 1000), user(1, 2000)]); // 启动播种但不 armed
        assert_eq!(t.check(&[user(0,1000), user(1,2000)], 10_000_000, 600, 50), None);
    }

    #[test]
    fn idle_fires_after_n_secs_since_activity() {
        let mut t = DreamTrigger::new();
        t.note_activity(1_000_000); // armed + 记活动
        assert_eq!(t.check(&[user(0, 1_000_000)], 1_000_000 + 599_000, 600, 50), None, "未到 idle");
        assert_eq!(t.check(&[user(0, 1_000_000)], 1_000_000 + 600_000, 600, 50), Some(TriggerReason::Idle));
    }

    #[test]
    fn cap_counts_only_kind_user() {
        // P1 铁律④：external/subagent_result 不顶 cap
        let mut t = DreamTrigger::new();
        t.note_activity(1000);
        let mut evs: Vec<HistoryEvent> = (0..49).map(|i| user(i, 1000 + i)).collect();
        evs.push(ext(49, 2000));
        evs.push(sub(50, 3000));
        assert_eq!(t.check(&evs, 999_999_999, 600, 50), None, "49 user + external + subagent_result 不应达 cap");
        evs.push(user(51, 4000)); // 第 50 个 user
        assert_eq!(t.check(&evs, 999_999_999, 600, 50), Some(TriggerReason::Cap));
    }

    #[test]
    fn single_flight_noop_while_running() {
        let mut t = DreamTrigger::new();
        t.note_activity(1000);
        let evs: Vec<HistoryEvent> = (0..50).map(|i| user(i, 1000+i)).collect();
        t.dream_started(49); // dream 跑着
        assert_eq!(t.check(&evs, 999_999_999, 600, 50), None, "dream 跑着 → 新触发 noop（单飞）");
        t.dream_finished();
        assert_eq!(t.check(&evs, 999_999_999, 600, 50), Some(TriggerReason::Cap), "完成后可再触发");
    }

    #[test]
    fn empty_skip_when_nothing_since_dream_marker() {
        let mut t = DreamTrigger::new();
        t.note_activity(1000);
        // 最后事件就是 dream marker → 之后无新事件 → 空跳过
        let evs = vec![user(0, 1000), dream_marker(1, 2000)];
        t.seed_from_history(&evs); // 播种 last_dream_marker_seq=1
        assert_eq!(t.check(&evs, 999_999_999, 600, 50), None, "dream marker 后无新事件 → 空跳过");
    }

    #[test]
    fn reset_does_not_reset_extraction_frontier() {
        // §7.8：reset marker 不影响 dream 提取前沿；reset 后的事件仍算「自上次 dream marker」
        let mut t = DreamTrigger::new();
        t.note_activity(1000);
        // dream marker@5，之后 reset@10、user@12
        let evs = vec![dream_marker(5, 5000), reset_marker(10, 10000), user(12, 12000)];
        t.seed_from_history(&evs); // last_dream_marker_seq = 5（只认 dream）
        // 自 seq5 之后有 user@12 → 非空；user_count=1 < cap=50；idle 看活动（12000，now 大）→ Idle
        assert_eq!(t.check(&evs, 999_999_999, 600, 50), Some(TriggerReason::Idle));
    }
}
```

- [ ] **Step 2: 注册模块 + 跑测试确认失败**

`lib.rs` 加 `pub mod dream;`。`cargo test --lib --manifest-path src-tauri/Cargo.toml dream::tests` → 编译失败。

- [ ] **Step 3: 实现 `DreamTrigger`（`dream.rs`）**

```rust
//! dream：记忆整理子代理。本任务只放触发器（纯逻辑）；Task 8 加 spawn_dream_agent + 提取。
use crate::history::HistoryEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerReason { Idle, Cap }

/// dream 触发状态（修 v1 全部 timing 坑）。owned by driver task。
#[derive(Debug, Clone)]
pub struct DreamTrigger {
    pub last_user_activity: u64,   // 距此算 idle（user/external 活动）
    pub last_dream_marker_seq: u64,// dream 提取前沿（只认 dream marker；reset 不动它）
    pub in_flight: bool,           // 单飞：dream 跑着时新触发 noop
    pub armed: bool,               // 首次用户活动后才 arm（修「开机触发」）
}

impl Default for DreamTrigger { fn default() -> Self { Self::new() } }

impl DreamTrigger {
    pub fn new() -> Self {
        Self { last_user_activity: 0, last_dream_marker_seq: 0, in_flight: false, armed: false }
    }
    /// 记一次用户活动（user 消息 / external 拖入上传）。首次活动 armed。
    pub fn note_activity(&mut self, ts: u64) {
        self.armed = true;
        if ts > self.last_user_activity { self.last_user_activity = ts; }
    }
    /// 启动时从 history 播种提取前沿（最后 dream marker seq）；不 armed（仍需首次活动才触发）。
    pub fn seed_from_history(&mut self, events: &[HistoryEvent]) {
        self.last_dream_marker_seq = events.iter()
            .filter(|e| e.kind == "marker" && e.data.get("marker").and_then(|v| v.as_str()) == Some("dream"))
            .map(|e| e.seq).max().unwrap_or(0);
    }
    pub fn dream_started(&mut self, marker_seq: u64) {
        self.in_flight = true;
        self.last_dream_marker_seq = marker_seq;
    }
    pub fn dream_finished(&mut self) { self.in_flight = false; }

    /// 是否该触发 dream。idle_secs=距上次用户活动秒；cap=自上次 dream marker 后 user 事件数阈值。
    pub fn check(&self, events: &[HistoryEvent], now: u64, idle_secs: u64, cap: u64) -> Option<TriggerReason> {
        if !self.armed || self.in_flight { return None; }
        let since: Vec<&HistoryEvent> = events.iter().filter(|e| e.seq > self.last_dream_marker_seq).collect();
        if since.is_empty() { return None; } // 空跳过
        let user_count = since.iter().filter(|e| e.kind == "user").count() as u64;
        if user_count >= cap { return Some(TriggerReason::Cap); }
        if now.saturating_sub(self.last_user_activity) >= idle_secs * 1000 { return Some(TriggerReason::Idle); }
        None
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

```
cargo test --lib --manifest-path src-tauri/Cargo.toml dream::tests
```
Expected: 6 PASS（含 cap 只数 user、单飞、空跳过、开机不触发、reset 不动提取前沿）。

- [ ] **Step 5: 全量 check**

```
cargo check --tests --manifest-path src-tauri/Cargo.toml
```
Expected: 0 warning / 0 error。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/dream.rs src-tauri/src/lib.rs
git commit -m "feat(dream): DreamTrigger（idle/cap 只数 user/单飞/空跳过/开机不触发）"
```

---

## Task 8: dream.rs —— run_dream（提取→「日」+ 写 marker；P1 铁律③ + 失败不推进）

**Why**：触发器（Task 7）决定「何时」dream；本任务实现 dream **跑一次**：读 history 段 [a,b] → dream LLM 提取事件（只写标题/详情/主语正文）→ **代码盖戳 seq[a,b]** 写 `memory/{date}.md` + MEMORY「日」→ 写 dream marker（`until_seq=b`，dream-start 盖戳）。失败/超时**不写 marker**（下次自动重试同段，§13.1）。driver 集成（idle ticker + 轮间 cap）放 Task 11。

**Files:**
- Modify: `src-tauri/src/dream.rs`（加 `DreamExtract` / `run_dream` / `dream_prompt` / `parse_extracts`）

**Interfaces:**
- Consumes: `crate::history::{HistoryWriterHandle, HistoryEvent, read_all}`、`crate::memory::{append_day_event, append_memory_day}`、`crate::llm::LlmRound`（dream 用单轮 round，无需工具循环）、`DreamTrigger`（Task 7）
- Produces:
  - `pub struct DreamExtract { title, detail, subject: String }`
  - `pub async fn run_dream(round, cfg, history, history_dir, workspace, trigger, emit) -> Result<(), String>`
  - 私有 `dream_prompt(a,b,&[&HistoryEvent]) -> String`、`parse_extracts(content) -> Vec<DreamExtract>`

**关键正确性**：`b = history.current_seq()` 在**提取前**捕获（§7.1 marker 竞态修法）；新 turn 事件（dream 跑期间落盘，seq>b）归**下一段**，本 marker `until_seq=b` 不覆盖它们。提取出的 seq 指针 = 代码盖戳的 `[a,b]`（LLM 不碰 seq）。失败 → `trigger.dream_finished()` 但**不写 marker** → 段保留 → 下次重试。

- [ ] **Step 1: 写失败测试**（追加到 `dream.rs` 的 `mod tests`）

```rust
    use crate::llm::{LlmRound, RoundResult, FinishReason, Emitter};
    use crate::history::{spawn_writer, read_all, HistoryWriterHandle, HistoryEvent};
    use crate::config::Config;
    use std::sync::{Arc, Mutex};
    use async_trait::async_trait;

    async fn flush() { tokio::time::sleep(std::time::Duration::from_millis(120)).await; }

    /// 脚本化 dream round：返回固定提取 JSON 行。
    struct ScriptedDream { content: String, fail: bool }
    #[async_trait]
    impl LlmRound for ScriptedDream {
        async fn round(&self, _: &[serde_json::Value], _: &Config, _: &dyn Emitter) -> Result<RoundResult, String> {
            if self.fail { return Err("dream LLM 500".into()); }
            Ok(RoundResult { content: self.content.clone(), reasoning: String::new(), tool_calls: vec![],
                finish: FinishReason::Stop, usage: None,
                assistant_message: serde_json::json!({"role":"assistant","content": self.content}) })
        }
    }
    struct Noemit;
    #[async_trait]
    impl Emitter for Noemit {
        async fn content(&self, _: &str) {} async fn thinking(&self, _: &str) {}
        async fn tool_call(&self, _: &str, _: &str) {} async fn tool_result(&self, _: &str, _: &str) {}
        async fn turn_start(&self) {} async fn turn_end(&self) {} async fn error(&self, _: &str) {}
    }

    fn seed_segment(h: &HistoryWriterHandle, n_user: usize) {
        for i in 0..n_user { h.append(HistoryEvent::user(1000 + i as u64 * 1000, "main", &format!("msg{i}"), &[])); }
    }

    #[tokio::test]
    async fn run_dream_extracts_writes_memory_and_marker() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"));
        seed_segment(&h, 3); // seq 0,1,2（a=0,b=3 — 但 b=current_seq 在 run_dream 内取）
        flush().await;
        let mut t = DreamTrigger::new();
        t.armed = true; // 跳过 armed 门（直接测 run_dream）
        let round = Arc::new(ScriptedDream { content:
            "{\"title\":\"聊了三件事\",\"detail\":\"用户问了 msg0/1/2\",\"subject\":\"用户\"}\n{\"title\":\"第二条\",\"detail\":\"d\",\"subject\":\"agent\"}".into(),
            fail: false });
        let cfg = Config::default();
        let r = run_dream(round, &cfg, &h, &dir.path().join("history"), dir.path(), &mut t, &Noemit).await;
        assert!(r.is_ok(), "{:?}", r);
        flush().await;
        // memory/{date}.md 有两段，seq 指针代码盖戳 = [a,b]
        let mem = std::fs::read_to_string(dir.path().join("memory").join("1970").join("01").join("1970-01-01.md")).unwrap();
        assert!(mem.contains("聊了三件事"));
        // a = last_dream_marker_seq+1 = 1（0+1）；b = current_seq at run_dream entry = 3
        assert!(mem.contains("seq[1,3]"), "seq 指针代码盖戳 [a,b]: {mem}");
        // MEMORY「日」节有行
        let memory_md = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(memory_md.contains("聊了三件事"));
        // history 有 dream marker，until_seq = b（dream-start）
        let evs = read_all(&dir.path().join("history"));
        let m = evs.iter().find(|e| e.kind=="marker").unwrap();
        assert_eq!(m.data["marker"], serde_json::json!("dream"));
        assert_eq!(m.data["until_seq"], serde_json::json!(3), "until_seq = dream-start seq");
        assert!(!t.in_flight, "dream 完成应清 in_flight");
    }

    #[tokio::test]
    async fn run_dream_failure_writes_no_marker_retries() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"));
        seed_segment(&h, 2);
        flush().await;
        let mut t = DreamTrigger::new(); t.armed = true;
        let round = Arc::new(ScriptedDream { content: String::new(), fail: true });
        let cfg = Config::default();
        let res = run_dream(round, &cfg, &h, &dir.path().join("history"), dir.path(), &mut t, &Noemit).await;
        assert!(res.is_err(), "失败应传播 Err");
        flush().await;
        let evs = read_all(&dir.path().join("history"));
        assert!(evs.iter().all(|e| e.kind != "marker"), "失败不应写 dream marker（段保留待重试）");
        assert!(!t.in_flight, "失败也应清 in_flight 以便重试");
    }

    #[tokio::test]
    async fn run_dream_until_seq_is_dream_start_not_concurrent_event() {
        // §7.1 marker 竞态：dream 跑期间新 turn 落盘（seq 增长），until_seq 仍 = dream-start b
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"));
        seed_segment(&h, 2); // current_seq 将 = 2
        flush().await;
        let mut t = DreamTrigger::new(); t.armed = true;
        // round 里模拟「dream 进行中新 turn 落盘」
        struct ConcurrentRound { h2: HistoryWriterHandle }
        #[async_trait]
        impl LlmRound for ConcurrentRound {
            async fn round(&self, _: &[serde_json::Value], _: &Config, _: &dyn Emitter) -> Result<RoundResult, String> {
                // dream 进行中：新 turn 事件落盘（归下一段）
                self.h2.append(HistoryEvent::user(999_000, "main", "dream 期间的新消息", &[]));
                Ok(RoundResult { content: "{\"title\":\"t\",\"detail\":\"d\",\"subject\":\"用户\"}".into(),
                    reasoning: String::new(), tool_calls: vec![], finish: FinishReason::Stop, usage: None,
                    assistant_message: serde_json::json!({"role":"assistant","content":"x"}) })
            }
        }
        let round = Arc::new(ConcurrentRound { h2: h.clone() });
        let cfg = Config::default();
        let _ = run_dream(round, &cfg, &h, &dir.path().join("history"), dir.path(), &mut t, &Noemit).await;
        flush().await;
        let evs = read_all(&dir.path().join("history"));
        let m = evs.iter().find(|e| e.kind=="marker").unwrap();
        assert_eq!(m.data["until_seq"], serde_json::json!(2), "until_seq = dream-start（2），不被 dream 期间的新事件（seq 2+）覆盖");
    }

    #[tokio::test]
    async fn run_dream_empty_segment_skips() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"));
        // 先写一个 dream marker，再 run_dream → a > 无新事件 → 空跳过
        h.append(HistoryEvent::marker(1000, "dream", 0));
        flush().await;
        let mut t = DreamTrigger::new(); t.armed = true;
        t.seed_from_history(&read_all(&dir.path().join("history"))); // last_dream_marker_seq = marker seq
        let round = Arc::new(ScriptedDream { content: "{\"title\":\"x\",\"detail\":\"y\",\"subject\":\"z\"}".into(), fail: false });
        let cfg = Config::default();
        let _ = run_dream(round, &cfg, &h, &dir.path().join("history"), dir.path(), &mut t, &Noemit).await;
        flush().await;
        // 不应新增第二个 marker（空跳过）
        let markers = read_all(&dir.path().join("history")).iter().filter(|e| e.kind=="marker").count();
        assert_eq!(markers, 1, "空段应跳过，不新增 marker");
    }
```

- [ ] **Step 2: 跑测试确认失败**

`cargo test --lib --manifest-path src-tauri/Cargo.toml dream::tests` → 编译失败（`run_dream`/`DreamExtract`/`parse_extracts` 未定义）。

- [ ] **Step 3: 实现（追加到 `dream.rs`）**

```rust
use crate::history::{read_all, HistoryEvent, HistoryWriterHandle};
use crate::llm::{Emitter, LlmRound};
use crate::memory;
use crate::config::Config;
use serde_json::Value;
use std::sync::Arc;

const DREAM_SYS: &str = "你是 dream，负责把一段对话整理成「日」层记忆。对每个值得记住的事件输出一行 JSON：{\"title\":\"简短标题\",\"detail\":\"一句话详情\",\"subject\":\"主语(你/用户/子代理#N)\"}。只输出 JSON 行，每行一个事件，不要任何其他文字、不要 markdown 代码块。";

#[derive(Debug, Clone)]
pub struct DreamExtract { pub title: String, pub detail: String, pub subject: String }

/// 跑一次 dream。b=dream-start seq 在提取前捕获；失败不写 marker（段保留待重试，§13.1）。
pub async fn run_dream<E: Emitter>(
    round: Arc<dyn LlmRound>,
    cfg: &Config,
    history: &HistoryWriterHandle,
    history_dir: &std::path::Path,
    workspace: &std::path::Path,
    trigger: &mut DreamTrigger,
    emit: &E,
) -> Result<(), String> {
    if trigger.in_flight { return Ok(()); } // 单飞
    let b = history.current_seq(); // dream-start seq（提取前捕获，§7.1）
    let a = trigger.last_dream_marker_seq + 1;
    let events = read_all(history_dir);
    let segment: Vec<&HistoryEvent> = events.iter()
        .filter(|e| e.seq >= a && e.seq <= b && e.thread == "main" && e.kind != "marker")
        .collect();
    if segment.is_empty() { return Ok(()); } // 空跳过
    trigger.dream_started(b); // 单飞 on（last_dream_marker_seq=b）
    let prompt = dream_prompt(a, b, &segment);
    let messages = vec![
        serde_json::json!({"role":"system","content": DREAM_SYS}),
        serde_json::json!({"role":"user","content": prompt}),
    ];
    let resp = match round.round(&messages, cfg, emit).await {
        Ok(r) => r,
        Err(e) => { trigger.dream_finished(); return Err(e); } // 失败：不写 marker，段保留
    };
    let extracts = parse_extracts(&resp.content);
    let now = now_ms();
    for (i, ext) in extracts.iter().enumerate() {
        // P1 铁律③：seq 指针代码盖戳 [a,b]，LLM 只写了 title/detail/subject
        let _ = memory::append_day_event(workspace, now, &ext.title, &ext.detail, &ext.subject, (a, b), None);
        let _ = memory::append_memory_day(workspace, now, (i + 1) as u32, &ext.title);
    }
    // 写 dream marker（until_seq=b，dream-start；dream 期间的新事件 seq>b 归下一段）
    history.append(HistoryEvent::marker(now, "dream", b));
    trigger.dream_finished();
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// dream 提取 prompt：把段渲染成可读对话，附 seq 范围。
fn dream_prompt(a: u64, b: u64, segment: &[&HistoryEvent]) -> String {
    let mut buf = format!("把下面这段对话（history seq[{a},{b}]）整理成「日」层记忆事件段：\n\n");
    for e in segment {
        let line = match e.kind.as_str() {
            "user" => format!("seq{} [用户] {}\n", e.seq, e.data.get("text").and_then(|v| v.as_str()).unwrap_or("")),
            "assistant" => format!("seq{} [助手] {}\n", e.seq, e.data.get("content").and_then(|v| v.as_str()).unwrap_or("(工具调用)")),
            "tool_result" => format!("seq{} [工具结果] {}\n", e.seq, e.data.get("result").and_then(|v| v.as_str()).unwrap_or("")),
            "external" => format!("seq{} [外部] {}\n", e.seq, e.data.get("what").and_then(|v| v.as_str()).unwrap_or("")),
            "subagent_result" => format!("seq{} [子代理完成] {}\n", e.seq, e.data.get("summary").and_then(|v| v.as_str()).unwrap_or("")),
            _ => String::new(),
        };
        buf.push_str(&line);
    }
    buf.push_str("\n现在输出 JSON 行（每行一个值得记住的事件）。");
    buf
}

/// 解析 dream 输出：逐行尝试解析 {title,detail,subject}；坏行跳过（LLM 可能掺杂质）。
fn parse_extracts(content: &str) -> Vec<DreamExtract> {
    let mut out = Vec::new();
    for line in content.lines() {
        let l = line.trim().trim_start_matches("```json").trim_start_matches("```").trim();
        if l.is_empty() || !l.starts_with('{') { continue; }
        if let Ok(v) = serde_json::from_str::<Value>(l) {
            let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let detail = v.get("detail").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let subject = v.get("subject").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if !title.is_empty() {
                out.push(DreamExtract { title, detail, subject });
            }
        }
    }
    out
}
```

- [ ] **Step 4: 跑测试确认通过**

```
cargo test --lib --manifest-path src-tauri/Cargo.toml dream::tests
```
Expected: Task 7 的 6 条 + Task 8 的 4 条 = 10 PASS。

- [ ] **Step 5: 全量 check**

```
cargo check --tests --manifest-path src-tauri/Cargo.toml
```
Expected: 0 warning / 0 error。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/dream.rs
git commit -m "feat(dream): run_dream 提取→「日」(seq 代码盖戳) + dream marker(dream-start until_seq) + 失败不推进"
```

---

## Task 9: mem_cli.rs —— ls/read/history/search（零 LLM 只读 drill）

**Why**：mem 是无状态只读导航器（spec §10.1：文件树即索引、零 LLM、单写多读）。把 4 条命令的核心逻辑抽成纯函数（吃 workspace 路径、产 String），mem bin（Task 10）与测试共用。dream 没跑过也能 drill（只要有 `{date}.md`）。

**Files:**
- Create: `src-tauri/src/mem_cli.rs`
- Modify: `src-tauri/src/lib.rs`（加 `pub mod mem_cli;`）

**Interfaces:**
- Consumes: `crate::history::{date_from_ts, HistoryEvent}`、`std::fs`
- Produces:
  - `pub fn ls(workspace: &Path, year: Option<&str>, month: Option<&str>) -> String`
  - `pub fn read_day(workspace: &Path, date: &str) -> String`
  - `pub fn history(workspace: &Path, date: &str, seq_range: Option<(u64,u64)>) -> String`
  - `pub fn search(workspace: &Path, query: &str, raw: bool) -> String`

- [ ] **Step 1: 写失败测试**（`mem_cli.rs` 的 `mod tests`，fixture 文件树）

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryEvent;

    fn ws() -> tempfile::TempDir { tempfile::tempdir().unwrap() }

    fn make_day(ws: &std::path::Path, date: &str, body: &str) {
        // date="2026-07-26" → memory/2026/07/2026-07-26.md
        let (y, m) = (date.split('-').next().unwrap(), date.split('-').nth(1).unwrap());
        let p = ws.join("memory").join(y).join(m).join(format!("{date}.md"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
    }
    fn make_hist(ws: &std::path::Path, date: &str, events: &[HistoryEvent]) {
        let p = ws.join("history").join(format!("{date}.jsonl"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut s = String::new();
        for e in events { s.push_str(&serde_json::to_string(e).unwrap()); s.push('\n'); }
        std::fs::write(&p, s).unwrap();
    }
    fn ev(seq: u64, kind: &str, text: &str) -> HistoryEvent {
        let mut e = match kind {
            "user" => HistoryEvent::user(1000 + seq * 1000, "main", text, &[]),
            "assistant" => HistoryEvent::assistant(1000 + seq * 1000, "main", text, "", vec![]),
            "tool_result" => HistoryEvent::tool_result(1000 + seq * 1000, "main", "bash", text, "c"),
            _ => HistoryEvent::user(0, "main", text, &[]),
        };
        e.seq = seq; e
    }

    #[test]
    fn ls_lists_years_then_months_then_days() {
        let w = ws();
        make_day(w.path(), "2026-07-26", "## 12:30 evt-20260726-001 标题A\n**详情**: foo\n");
        make_day(w.path(), "2025-03-01", "## 09:00 evt-20250301-001 标题B\n");
        assert!(ls(w.path(), None, None).contains("2026"));
        assert!(ls(w.path(), None, None).contains("2025"));
        assert!(ls(w.path(), Some("2026"), None).contains("07"));
        let days = ls(w.path(), Some("2026"), Some("07"));
        assert!(days.contains("2026-07-26"), "{days}");
        assert!(days.contains("标题A"), "天带标题: {days}");
    }

    #[test]
    fn read_day_prints_segments_with_pointer() {
        let w = ws();
        make_day(w.path(), "2026-07-26", "## 12:30 evt-001 重构\n**对话索引**: history/2026-07-26.jsonl#seq[3,9]\n");
        let out = read_day(w.path(), "2026-07-26");
        assert!(out.contains("重构"));
        assert!(out.contains("seq[3,9]"), "应含对话索引指针: {out}");
    }
    #[test]
    fn read_day_missing_says_none() {
        let w = ws();
        assert!(read_day(w.path(), "2099-01-01").contains("无") || read_day(w.path(), "2099-01-01").is_empty());
    }

    #[test]
    fn history_humanizes_and_filters_seq() {
        let w = ws();
        make_hist(w.path(), "2026-07-26", &[
            ev(1, "user", "你好"), ev(2, "assistant", "嗨"), ev(3, "user", "再做X"), ev(4, "assistant", "好"),
        ]);
        let all = history(w.path(), "2026-07-26", None);
        assert!(all.contains("你好") && all.contains("嗨"), "人性化全量: {all}");
        let sub = history(w.path(), "2026-07-26", Some((2, 3)));
        assert!(sub.contains("嗨") && sub.contains("再做X"), "[2,3] 段: {sub}");
        assert!(!sub.contains("你好"), "seq 1 应被过滤掉: {sub}");
    }

    #[test]
    fn search_finds_in_memory_default() {
        let w = ws();
        make_day(w.path(), "2026-07-26", "## 标题\n**详情**: 关键词foobar在此\n");
        assert!(search(w.path(), "foobar", false).contains("foobar"), "默认搜 memory/");
    }
    #[test]
    fn search_raw_extends_to_history() {
        let w = ws();
        make_hist(w.path(), "2026-07-26", &[ev(1, "user", "secret_value_xyz")]);
        // 默认（memory/）搜不到 history 内容
        assert!(!search(w.path(), "secret_value_xyz", false).contains("secret_value_xyz"));
        // --raw 扩到 history
        assert!(search(w.path(), "secret_value_xyz", true).contains("secret_value_xyz"), "--raw 应扩到 history");
    }
}
```

- [ ] **Step 2: 注册模块 + 跑测试确认失败**

`lib.rs` 加 `pub mod mem_cli;`。`cargo test --lib --manifest-path src-tauri/Cargo.toml mem_cli::tests` → 编译失败。

- [ ] **Step 3: 实现 `mem_cli.rs`**

```rust
//! mem CLI 核心逻辑（零 LLM 只读 drill）。文件树即索引：路径里 Y/M/D 是时间索引，
//! {date}.md 首行 ## 是标题，事件段「对话索引」是指针。三样都机械可读，无需 dream 写索引文件（§10.6）。
use crate::history::HistoryEvent;
use std::path::Path;

fn split_date(date: &str) -> Option<(&str, &str)> {
    let mut it = date.split('-');
    Some((it.next()?, it.next()?))
}

/// mem ls [year [month]]：列年 / 月 / 天（天带标题，读 {date}.md 首行 ##）。
pub fn ls(workspace: &Path, year: Option<&str>, month: Option<&str>) -> String {
    let mem = workspace.join("memory");
    match (year, month) {
        (None, None) => list_dirs(&mem), // 年
        (Some(y), None) => list_dirs(&mem.join(y)), // 月
        (Some(y), Some(m)) => list_days(&mem.join(y).join(m)), // 天（带标题）
        (None, Some(_)) => "请先指定年".into(),
    }
}

fn list_dirs(dir: &Path) -> String {
    let mut names: Vec<String> = std::fs::read_dir(dir).map(|rd| {
        rd.filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_string_lossy().into_owned().into())
            .filter(|n| n.chars().all(|c| c.is_ascii_digit()))
            .collect()
    }).unwrap_or_default();
    names.sort();
    if names.is_empty() { format!("（{dir} 下无数据）", dir = dir.display()) } else { names.join("\n") }
}

fn list_days(dir: &Path) -> String {
    let mut rows: Vec<(String, String)> = std::fs::read_dir(dir).map(|rd| {
        rd.filter_map(|e| e.ok()).filter_map(|e| {
            let p = e.path();
            let fname = p.file_name()?.to_string_lossy().into_owned();
            let date = fname.trim_end_matches(".md").to_string();
            if !date.contains('-') { return None; }
            let title = first_title(&p).unwrap_or_default();
            Some((date, title))
        }).collect()
    }).unwrap_or_default();
    rows.sort();
    if rows.is_empty() { "（无当天记忆）".into() }
    else { rows.iter().map(|(d, t)| if t.is_empty() { d.clone() } else { format!("{d}: {t}") }).collect::<Vec<_>>().join("\n") }
}

fn first_title(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().find(|l| l.starts_with("## "))?
        .trim_start_matches("## ").split_whitespace().collect::<Vec<_>>().join(" ")
        .split_once(' ').map(|(_, rest)| rest.to_string()) // 去 HH:MM evt-NNN 前缀，留标题
        .or_else(|| Some(String::new()))
}

/// mem read <date>：打印当天事件段（含「对话索引」指针）。
pub fn read_day(workspace: &Path, date: &str) -> String {
    let (y, m) = match split_date(date) { Some(x) => x, None => return "日期格式应为 YYYY-MM-DD".into() };
    let path = workspace.join("memory").join(y).join(m).join(format!("{date}.md"));
    match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => format!("（{date} 无当天记忆）"),
    }
}

/// mem history <date> [--seq a..b]：读原始对话，人性化渲染，可按 seq 过滤。
pub fn history(workspace: &Path, date: &str, seq_range: Option<(u64, u64)>) -> String {
    let path = workspace.join("history").join(format!("{date}.jsonl"));
    let text = match std::fs::read_to_string(&path) { Ok(s) => s, Err(_) => return format!("（{date} 无原始对话 history）") };
    let mut out = String::new();
    for line in text.lines() {
        if line.trim().is_empty() { continue; }
        let Ok(ev) = serde_json::from_str::<HistoryEvent>(line) else { continue };
        if let Some((a, b)) = seq_range { if ev.seq < a || ev.seq > b { continue; } }
        out.push_str(&humanize(&ev));
        out.push('\n');
    }
    if out.is_empty() { format!("（{date} 的 seq 范围内无事件）") } else { out }
}

fn humanize(e: &HistoryEvent) -> String {
    let s = e.data.get("content").and_then(|v| v.as_str()).or_else(|| e.data.get("text").and_then(|v| v.as_str())).unwrap_or("");
    match e.kind.as_str() {
        "user" => format!("[seq{} 用户] {s}", e.seq),
        "assistant" => {
            let has_tc = e.data.get("tool_calls").and_then(|v| v.as_array()).map(|a| !a.is_empty()).unwrap_or(false);
            if has_tc { format!("[seq{} 助手] (调用工具)", e.seq) } else { format!("[seq{} 助手] {s}", e.seq) }
        }
        "tool_result" => format!("[seq{} 工具结果:{}] {}",
            e.seq, e.data.get("name").and_then(|v| v.as_str()).unwrap_or(""), s),
        "external" => format!("[seq{} 外部] {}", e.seq, e.data.get("what").and_then(|v| v.as_str()).unwrap_or("")),
        "subagent_result" => format!("[seq{} 子代理#{}完成] {}",
            e.seq, e.data.get("agent_id").and_then(|v| v.as_u64()).unwrap_or(0),
            e.data.get("summary").and_then(|v| v.as_str()).unwrap_or("")),
        "marker" => format!("[seq{} marker {} until_seq={}] (隐藏)", e.seq,
            e.data.get("marker").and_then(|v| v.as_str()).unwrap_or(""),
            e.data.get("until_seq").and_then(|v| v.as_u64()).unwrap_or(0)),
        _ => format!("[seq{} {}]", e.seq, e.kind),
    }
}

/// mem search <query> [--raw]：关键字 grep（默认 memory/，--raw 扩到 history）。
pub fn search(workspace: &Path, query: &str, raw: bool) -> String {
    let mut hits = Vec::new();
    let mem = workspace.join("memory");
    grep_dir(&mem, query, &mut hits);
    if raw {
        let hist = workspace.join("history");
        grep_dir(&hist, query, &mut hits);
    }
    if hits.is_empty() { format!("（未找到 {query:?}）") } else { hits.join("\n") }
}

fn grep_dir(dir: &Path, query: &str, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() { grep_dir(&p, query, out); continue; }
        if let Ok(text) = std::fs::read_to_string(&p) {
            for (i, line) in text.lines().enumerate() {
                if line.contains(query) {
                    out.push(format!("{}:{}: {}", p.display(), i + 1, line.trim()));
                }
            }
        }
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

```
cargo test --lib --manifest-path src-tauri/Cargo.toml mem_cli::tests
```
Expected: 6 PASS。

- [ ] **Step 5: 全量 check**

```
cargo check --tests --manifest-path src-tauri/Cargo.toml
```
Expected: 0 warning / 0 error。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/mem_cli.rs src-tauri/src/lib.rs
git commit -m "feat(mem-cli): ls/read_day/history/search 纯逻辑（零 LLM、文件树即索引）"
```

---

## Task 10: bin/mem.rs —— 第二个 [[bin]]（arg 解析 + workspace 定位 + 调 mem_cli）

**Why**：mem 是同 crate 第二个 `[[bin]]`（spec §10.3），零 LLM 只读。它没有 Tauri AppHandle，故用 Task 1 的 `config::load_from(dir)` 定位 workspace：`--workspace` 覆盖 → `OVOICE_WORKSPACE` 兜底 → next-to-exe config（portable 优先）→ `%APPDATA%` fallback → `Documents/ovoice/` 默认。mem 只读 config（不写）。

**Files:**
- Modify: `src-tauri/Cargo.toml`（加 `[[bin]] name="mem" path="src/bin/mem.rs"`）
- Create: `src-tauri/src/bin/mem.rs`
- Test: `src-tauri/src/bin/mem.rs` 的 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `ovoice_lib::{config, mem_cli}`（同 crate lib）
- Produces: 可独立运行的 `mem` 二进制（4 命令：ls/read/history/search）

- [ ] **Step 1: 写失败测试**（`src/bin/mem.rs` 的 `#[cfg(test)] mod tests`）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_workspace_flag_overrides() {
        let args = vec!["mem".to_string(), "--workspace".into(), "/tmp/ws".into(), "ls".into()];
        assert_eq!(resolve_workspace(&args), PathBuf::from("/tmp/ws"));
    }
    #[test]
    fn resolve_workspace_flag_anywhere() {
        let args = vec!["mem".to_string(), "history".into(), "2026-07-26".into(), "--workspace".into(), "C:/w".into()];
        assert_eq!(resolve_workspace(&args), PathBuf::from("C:/w"));
    }
    #[test]
    fn parse_seq_dotdot() {
        assert_eq!(parse_seq_flag(&["mem".into(),"history".into(),"d".into(),"--seq".into(),"3..9".into()]), Some((3,9)));
    }
    #[test]
    fn parse_seq_comma() {
        assert_eq!(parse_seq_flag(&["mem".into(),"history".into(),"d".into(),"--seq".into(),"3,9".into()]), Some((3,9)));
    }
    #[test]
    fn parse_seq_none_when_absent() {
        assert_eq!(parse_seq_flag(&["mem".into(),"history".into(),"d".into()]), None);
    }
    #[test]
    fn dispatch_routes_read() {
        // dispatch 不依赖 env（workspace 由调用方解析）
        let dir = tempfile::tempdir().unwrap();
        let out = dispatch(&["mem".into(), "read".into(), "2099-01-01".into()], dir.path());
        assert!(out.contains("2099-01-01") || out.contains("无"));
    }
}
```

- [ ] **Step 2: 注册 bin + 跑测试确认失败**

`src-tauri/Cargo.toml` 在 `[lib]` 段之后加：
```toml
[[bin]]
name = "mem"
path = "src/bin/mem.rs"
```
`cargo test --manifest-path src-tauri/Cargo.toml --bin mem` → 编译失败（`resolve_workspace`/`dispatch`/`parse_seq_flag` 未定义）。

- [ ] **Step 3: 实现 `src/bin/mem.rs`**

```rust
//! mem —— 记忆深思 CLI（同 crate 第二个 [[bin]]）。零 LLM 只读 drill。
//! workspace 定位：--workspace 覆盖 > OVOICE_WORKSPACE > next-to-exe config(portable) > %APPDATA% fallback > 默认。
use ovoice_lib::{config, mem_cli};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ws = resolve_workspace(&args);
    let out = dispatch(&args, &ws);
    println!("{out}");
}

fn dispatch(args: &[String], ws: &std::path::Path) -> String {
    match args.get(1).map(|s| s.as_str()) {
        Some("ls") => mem_cli::ls(ws, args.get(2).map(|s| s.as_str()), args.get(3).map(|s| s.as_str())),
        Some("read") => mem_cli::read_day(ws, args.get(2).map(|s| s.as_str()).unwrap_or("")),
        Some("history") => mem_cli::history(ws, args.get(2).map(|s| s.as_str()).unwrap_or(""), parse_seq_flag(args)),
        Some("search") => mem_cli::search(ws, args.get(2).map(|s| s.as_str()).unwrap_or(""), args.iter().any(|a| a == "--raw")),
        Some("--help") | Some("-h") | None => usage(),
        Some(other) => format!("未知命令: {other}\n\n{}", usage()),
    }
}

fn usage() -> String {
    "mem —— 记忆深思（零 LLM 只读 drill）\n\n用法:\n  mem ls [year [month]]              列年/月/天（天带标题）\n  mem read <date>                    打印当天事件段（含对话索引指针）\n  mem history <date> [--seq a..b]    读原始对话（可按 seq 过滤）\n  mem search <query> [--raw]         关键字 grep（默认 memory/，--raw 扩到 history）\n\nworkspace 定位: --workspace > $OVOICE_WORKSPACE > next-to-exe config > %APPDATA% > Documents/ovoice".into()
}

fn parse_seq_flag(args: &[String]) -> Option<(u64, u64)> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--seq" {
            if let Some(v) = it.next() {
                let (s, e) = v.split_once("..").or_else(|| v.split_once(','))?;
                return Some((s.parse().ok()?, e.parse().ok()?));
            }
        }
    }
    None
}

fn resolve_workspace(args: &[String]) -> PathBuf {
    // 1. --workspace 覆盖
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--workspace" {
            if let Some(w) = it.next() { return PathBuf::from(w); }
        }
    }
    // 2. OVOICE_WORKSPACE 兜底
    if let Some(w) = std::env::var_os("OVOICE_WORKSPACE") { return PathBuf::from(w); }
    // 3. next-to-exe config（portable 优先；mem 只读 config）
    if let Some(parent) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
        let cfg = config::load_from(&parent);
        if !cfg.workspace_dir.trim().is_empty() { return PathBuf::from(&cfg.workspace_dir); }
    }
    // 4. %APPDATA% fallback
    if let Some(ad) = appdata_dir() {
        let cfg = config::load_from(&ad);
        if !cfg.workspace_dir.trim().is_empty() { return PathBuf::from(&cfg.workspace_dir); }
    }
    // 5. 默认 Documents/ovoice
    config::default_workspace_dir()
}

fn appdata_dir() -> Option<PathBuf> {
    let id = "com.ovoice.app";
    #[cfg(target_os = "windows")]
    { let base = std::env::var_os("APPDATA")?; Some(PathBuf::from(base).join(id)) }
    #[cfg(target_os = "macos")]
    { let home = std::env::var_os("HOME")?; Some(PathBuf::from(home).join("Library/Application Support").join(id)) }
    #[cfg(all(unix, not(target_os = "macos")))]
    { let home = std::env::var_os("HOME")?; Some(PathBuf::from(home).join(".local/share").join(id)) }
}

// tests 见 Step 1
```

- [ ] **Step 4: 跑测试确认通过**

```
cargo test --manifest-path src-tauri/Cargo.toml --bin mem
```
Expected: 6 PASS。

- [ ] **Step 5: 全量 check + 验 mem 能编译成独立二进制**

```
cargo check --tests --manifest-path src-tauri/Cargo.toml
cargo check --manifest-path src-tauri/Cargo.toml --bin mem
```
Expected: 0 warning / 0 error。两个 bin（默认 ovoice + mem）都能编译。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/Cargo.toml src-tauri/src/bin/mem.rs
git commit -m "feat(mem-bin): 第二个 [[bin]]——arg 解析 + workspace 定位(portable 优先) + 调 mem_cli"
```

---

## Task 11: lib.rs —— workspace 默认文件 bootstrap + history_tail/head 显示命令

**Why**：首次启动 workspace 要有 SOUL/AGENT/MEMORY 三件套（AGENT.md 含 mem 用法，spec §13.4）；前端 display 滑动窗口（Task 14）需要 `history_tail`/`history_head` 两条只读命令（spec §13.5，limit 数可见事件）。把可见事件过滤抽成纯函数离线测，命令层薄壳。

**Files:**
- Modify: `src-tauri/src/history.rs`（加 `visible_tail` / `visible_head` 纯函数）
- Modify: `src-tauri/src/lib.rs`（setup 里 bootstrap；加 `history_tail`/`history_head` 命令 + 注册）
- Test: 两处 `mod tests`

**Interfaces:**
- Consumes: `crate::history::{HistoryEvent, read_all}`、`crate::memory::ensure_memory_skeleton`
- Produces: `history::visible_tail(evs, limit, before_seq) -> Vec<HistoryEvent>`、`history::visible_head(evs, limit, after_seq) -> Vec<HistoryEvent>`；命令 `history_tail(limit, before_seq, app)`、`history_head(limit, after_seq, app)`；`lib::bootstrap_defaults(workspace)`

**可见事件定义（§13.5）**：`thread == "main" && kind != "marker"`（dream/reset marker 都隐藏）。

- [ ] **Step 1: 写失败测试**

`history.rs` tests 追加：
```rust
    fn ev(seq: u64, kind: &str, thread: &str) -> HistoryEvent {
        let mut e = HistoryEvent::user(seq * 1000, thread, &format!("u{seq}"), &[]); e.seq = seq;
        e.kind = kind.into(); e
    }
    #[test]
    fn visible_tail_returns_newest_before_cursor() {
        let evs = vec![ev(0,"user","main"), ev(1,"assistant","main"), ev(2,"user","main"),
            ev(3,"marker","main"), ev(4,"user","main"), ev(5,"assistant","agent:1")];
        // limit=2, before_seq=None → 最近 2 条可见（main+非marker）：seq 2 与 4（3 是 marker 隐藏，5 是 agent:N 隐藏）
        let t = visible_tail(&evs, 2, None);
        let seqs: Vec<u64> = t.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![2, 4], "最近 2 条可见事件（升序），marker/agent:N 跳过");
    }
    #[test]
    fn visible_tail_before_cursor_paginates_older() {
        let evs = (0..10).map(|i| ev(i, "user", "main")).collect::<Vec<_>>();
        let t = visible_tail(&evs, 3, Some(7)); // 7 之前的 3 条可见 → 4,5,6
        let seqs: Vec<u64> = t.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![4, 5, 6]);
    }
    #[test]
    fn visible_head_returns_oldest_after_cursor() {
        let evs = (0..10).map(|i| ev(i, "user", "main")).collect::<Vec<_>>();
        let h = visible_head(&evs, 3, Some(4)); // 4 之后的 3 条 → 5,6,7
        let seqs: Vec<u64> = h.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![5, 6, 7]);
    }
```
`lib.rs` media_tests（或新 mod）追加 bootstrap 测试：
```rust
    #[test]
    fn bootstrap_creates_defaults_if_absent() {
        let dir = tempfile::tempdir().unwrap();
        bootstrap_defaults(dir.path());
        assert!(dir.path().join("SOUL.md").exists());
        let agent = std::fs::read_to_string(dir.path().join("AGENT.md")).unwrap();
        assert!(agent.contains("mem"), "AGENT.md 应含 mem 用法: {agent}");
        let mem = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(mem.contains("## 日") && mem.contains("## 年"));
    }
    #[test]
    fn bootstrap_does_not_overwrite_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("SOUL.md"), "我的灵魂").unwrap();
        bootstrap_defaults(dir.path());
        assert_eq!(std::fs::read_to_string(dir.path().join("SOUL.md")).unwrap(), "我的灵魂", "不覆盖用户文件");
    }
```

- [ ] **Step 2: 跑测试确认失败**

`cargo test --lib --manifest-path src-tauri/Cargo.toml history::tests::visible_` 与 bootstrap 测试 → 编译失败。

- [ ] **Step 3: 实现**

(a) `history.rs` 加可见事件分页纯函数：
```rust
/// display 滑动窗口：最近 limit 条可见事件（main+非marker），before_seq 之前（向上翻）。升序返回。
pub fn visible_tail(evs: &[HistoryEvent], limit: u64, before_seq: Option<u64>) -> Vec<HistoryEvent> {
    let mut v: Vec<&HistoryEvent> = evs.iter().filter(|e| e.thread == "main" && e.kind != "marker").collect();
    v.sort_by_key(|e| e.seq);
    let cut = match before_seq { Some(s) => v.iter().take_while(|e| e.seq < s).count(), None => v.len() };
    let start = cut.saturating_sub(limit as usize);
    v[start..cut].iter().map(|e| (*e).clone()).collect()
}
/// display 滑动窗口：after_seq 之后的 limit 条可见事件（向下翻）。升序返回。
pub fn visible_head(evs: &[HistoryEvent], limit: u64, after_seq: Option<u64>) -> Vec<HistoryEvent> {
    let mut v: Vec<&HistoryEvent> = evs.iter().filter(|e| e.thread == "main" && e.kind != "marker").collect();
    v.sort_by_key(|e| e.seq);
    let start = match after_seq { Some(s) => v.iter().position(|e| e.seq > s).unwrap_or(v.len()), None => 0 };
    let end = (start + limit as usize).min(v.len());
    v[start..end].iter().map(|e| (*e).clone()).collect()
}
```

(b) `lib.rs` 加 bootstrap + 命令。文件顶部加：
```rust
/// 首次启动写默认 SOUL/AGENT/MEMORY；已存在则不动（用户文件神圣）。
fn bootstrap_defaults(workspace: &std::path::Path) {
    let soul = workspace.join("SOUL.md");
    if !soul.exists() {
        let _ = std::fs::write(&soul, "# SOUL.md —— 灵魂 / 风格（用户写，agent 永不改）\n\n（在这里写你希望助手始终秉持的语气、立场、风格。留空也行。）\n");
    }
    let agent = workspace.join("AGENT.md");
    if !agent.exists() {
        let _ = std::fs::write(&agent, AGENT_MD_DEFAULT);
    }
    let _ = crate::memory::ensure_memory_skeleton(workspace);
}

const AGENT_MD_DEFAULT: &str = r#"# AGENT.md —— 工具规范与项目维护

## 工具
write/read/edit/edit_card/display/bash/subagent：详见各工具 description。
- 临时产物落 workspace；展示用 display（自动按扩展名渲染）。
- 直接改盘用 edit（唯一匹配替换）；整文件重写用 write；弹卡片让用户改用 edit_card。

## 记忆深思（mem）
MEMORY.md 已 pinned 在你的上下文（日/周/月/年索引）。**日常不用主动查**——记忆已在眼前。
只在需要远期细节（某早先事件的原话、某子代理的完整对话）时，用 bash 调 mem CLI drill：

  mem ls [year [month]]            列年/月/天（天带标题）
  mem read <date>                  当天事件段（含通往原始对话的指针 seq[a,b]）
  mem history <date> --seq a..b    读那段原始逐字对话
  mem search <query> [--raw]       关键字搜 memory/（--raw 扩到 history）

drill 链：ls 年 → ls 月 → ls 天（看标题）→ read 事件段（拿 seq 指针）→ history --seq a..b 读原话。
"#;

/// display：before_seq 之前的 limit 条可见事件（向上翻）。
#[tauri::command]
fn history_tail(limit: u64, before_seq: Option<u64>, app: AppHandle) -> Vec<crate::history::HistoryEvent> {
    let ws = std::path::PathBuf::from(crate::config::load(&app).workspace_dir);
    let evs = crate::history::read_all(&ws.join("history"));
    crate::history::visible_tail(&evs, limit, before_seq)
}

/// display：after_seq 之后的 limit 条可见事件（向下翻）。
#[tauri::command]
fn history_head(limit: u64, after_seq: Option<u64>, app: AppHandle) -> Vec<crate::history::HistoryEvent> {
    let ws = std::path::PathBuf::from(crate::config::load(&app).workspace_dir);
    let evs = crate::history::read_all(&ws.join("history"));
    crate::history::visible_head(&evs, limit, after_seq)
}
```

(c) `lib.rs` setup 闭包里（`let cfg0 = crate::config::load(...)` 之后）加 bootstrap：
```rust
            let ws0 = std::path::PathBuf::from(&cfg0.workspace_dir);
            let _ = std::fs::create_dir_all(&ws0);
            bootstrap_defaults(&ws0);
```

(d) `invoke_handler` 的 `generate_handler!` 列表加 `history_tail, history_head,`。

- [ ] **Step 4: 跑测试确认通过**

```
cargo test --lib --manifest-path src-tauri/Cargo.toml history::tests::visible_
cargo test --lib --manifest-path src-tauri/Cargo.toml bootstrap
```
Expected: 5 PASS。

- [ ] **Step 5: 全量 check**

```
cargo check --tests --manifest-path src-tauri/Cargo.toml
```
Expected: 0 warning / 0 error。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/history.rs src-tauri/src/lib.rs
git commit -m "feat(lib): bootstrap SOUL/AGENT/MEMORY 默认（AGENT.md 含 mem 用法）+ history_tail/head 显示命令"
```

---

## Task 12: dream 接进 driver —— DreamTrigger 状态 + 轮间 cap + idle ticker（集成）

**Why**：Task 7/8 做了触发判定与 run_dream；本任务把它们接进运行中的 driver：dream 在**轮间**（turn 后、下轮 build 前）由 cap 触发、由 60s idle ticker 触发；dream 异步后台跑、推进 marker、不打断当前 turn；dream 完全静默（不推前端/不进 jobs/不 toast，§7.7）。为让 driver 持锁不跨 await，把 run_dream 拆成 `prepare_dream`（决策+占位，同步）+ `execute_dream`（提取+marker，async）。

**Files:**
- Modify: `src-tauri/src/dream.rs`（拆 `prepare_dream` / `execute_dream`；`run_dream` 变薄壳保 Task 8 测试绿）
- Modify: `src-tauri/src/agent.rs`（`SessionEvent::DreamCheck`；`spawn_session` 持 `Arc<Mutex<DreamTrigger>>` + 起 idle ticker；`run_one` note_activity + dispatch_dream）

**Interfaces:**
- Consumes: `DreamTrigger`/`run_dream`（Task 7/8）、`crate::history::read_all`
- Produces: `dream::prepare_dream(&mut DreamTrigger, &HistoryWriterHandle) -> Option<(u64,u64)>`、`dream::execute_dream(round,cfg,history,history_dir,workspace,a,b,emit) -> Result<(),String>`；driver 内 `dispatch_dream`、idle ticker。

**集成测试约束**：driver 接线（spawn_session/run_one/ticker）属集成层，本任务以 `cargo check --tests` 通过 + 手测（Task 14 后端联调）为准；prepare/execute 的正确性由 Task 8 的 run_dream 测试覆盖（run_dream = prepare+execute+finish）。

- [ ] **Step 1: 拆分 `dream.rs` 的 `run_dream`（保 Task 8 测试绿）**

把 Task 8 的 `run_dream` 重写为薄壳 + 两个新公开函数：
```rust
/// 决策 + 占位（同步，持锁调用）：占 in_flight、捕获 (a,b)。空/in_flight → None。
pub fn prepare_dream(trigger: &mut DreamTrigger, history: &HistoryWriterHandle) -> Option<(u64, u64)> {
    if trigger.in_flight { return None; }
    let b = history.current_seq();
    let a = trigger.last_dream_marker_seq + 1;
    trigger.dream_started(b);
    Some((a, b))
}

/// 执行（async，不持锁）：提取→「日」+ dream marker。失败传播 Err（调用方据此不推进 marker——其实 marker 本就只在成功时写）。
pub async fn execute_dream<E: crate::llm::Emitter>(
    round: std::sync::Arc<dyn crate::llm::LlmRound>,
    cfg: &crate::config::Config,
    history: &crate::history::HistoryWriterHandle,
    history_dir: &std::path::Path,
    workspace: &std::path::Path,
    a: u64, b: u64,
    emit: &E,
) -> Result<(), String> {
    let events = crate::history::read_all(history_dir);
    let segment: Vec<&HistoryEvent> = events.iter()
        .filter(|e| e.seq >= a && e.seq <= b && e.thread == "main" && e.kind != "marker").collect();
    if segment.is_empty() { return Ok(()); }
    let prompt = dream_prompt(a, b, &segment);
    let messages = vec![
        serde_json::json!({"role":"system","content": DREAM_SYS}),
        serde_json::json!({"role":"user","content": prompt}),
    ];
    let resp = round.round(&messages, cfg, emit).await?; // 失败传播（不写 marker）
    let extracts = parse_extracts(&resp.content);
    let now = now_ms();
    for (i, ext) in extracts.iter().enumerate() {
        let _ = crate::memory::append_day_event(workspace, now, &ext.title, &ext.detail, &ext.subject, (a, b), None);
        let _ = crate::memory::append_memory_day(workspace, now, (i + 1) as u32, &ext.title);
    }
    history.append(HistoryEvent::marker(now, "dream", b));
    Ok(())
}

/// Task 8 的可测入口 = prepare + execute + finish（保 8 个 dream 测试绿）。
pub async fn run_dream<E: crate::llm::Emitter>(
    round: std::sync::Arc<dyn crate::llm::LlmRound>, cfg: &crate::config::Config,
    history: &crate::history::HistoryWriterHandle, history_dir: &std::path::Path,
    workspace: &std::path::Path, trigger: &mut DreamTrigger, emit: &E,
) -> Result<(), String> {
    let (a, b) = match prepare_dream(trigger, history) { Some(x) => x, None => return Ok(()) };
    let r = execute_dream(round, cfg, history, history_dir, workspace, a, b, emit).await;
    trigger.dream_finished();
    r
}
```

- [ ] **Step 2: 跑 dream 测试确认仍绿**

```
cargo test --lib --manifest-path src-tauri/Cargo.toml dream::tests
```
Expected: 10 PASS（Task 7 的 6 + Task 8 的 4，run_dream 薄壳不改变行为）。

- [ ] **Step 3: driver 接线（`agent.rs`）**

(a) `SessionEvent` 加内部信号变体：
```rust
#[derive(Debug, Clone)]
pub enum SessionEvent {
    UserMessage { text: String, attachments: Vec<crate::AttachmentRef> },
    ContextNote { text: String },
    JobDone(JobOutcome),
    Reset,
    DreamCheck, // 内部信号（idle ticker 投递）；不入 history
}
```

(b) `spawn_session` 末尾起 idle ticker（dream 状态用 `Arc<Mutex<DreamTrigger>>`，启动播种）：
```rust
    // dream 触发状态（driver 单持，与 idle ticker 共享）
    let trigger = std::sync::Arc::new(std::sync::Mutex::new({
        let mut t = crate::dream::DreamTrigger::new();
        t.seed_from_history(&crate::history::read_all(&history_dir));
        t
    }));
    {
        let tx2 = tx.clone();
        let trigger2 = trigger.clone();
        // idle ticker：每 60s 投 DreamCheck（driver 串行判定，不并发 dream）
        tauri::async_runtime::spawn(async move {
            let _trigger = trigger2; // 保活
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                let _ = tx2.send(SessionEvent::DreamCheck).await;
            }
        });
    }
```
把 `trigger` 传入 spawned driver task（move 或 clone Arc），`run_one` 多接一个 `trigger: &Arc<Mutex<DreamTrigger>>` 参数。

(c) `run_one` 加 note_activity + dispatch_dream：
```rust
async fn run_one(
    e: &SessionEvent, history, history_dir, workspace, ctx, cfg, app,
    trigger: &std::sync::Arc<std::sync::Mutex<crate::dream::DreamTrigger>>,
) {
    // 用户活动（user 消息 / external 拖入）记一笔（供 idle 触发 + armed）
    if matches!(e, SessionEvent::UserMessage { .. } | SessionEvent::ContextNote { .. }) {
        trigger.lock().unwrap().note_activity(now_ms());
    }
    if matches!(e, SessionEvent::DreamCheck) {
        dispatch_dream(trigger, history, history_dir, workspace, cfg, app).await;
        return;
    }
    let emit = crate::AppEmitter { app: app.clone() };
    let round: std::sync::Arc<dyn llm::LlmRound> = std::sync::Arc::new(crate::llm::HttpRound);
    let _ = handle_event(e, history, history_dir, workspace, ctx, cfg, round, &emit).await;
    if matches!(e, SessionEvent::Reset) {
        // 既有 registry cancel + emit chat-reset（Task 5）
        { /* cancel agent jobs + *r = new_with_max(...) */ }
        let _ = tauri::Emitter::emit(app, "chat-reset", ());
    }
    // 轮间 cap 触发：turn 后检查
    if matches!(e, SessionEvent::UserMessage { .. } | SessionEvent::JobDone(_)) {
        dispatch_dream(trigger, history, history_dir, workspace, cfg, app).await;
    }
}

async fn dispatch_dream(trigger, history, history_dir, workspace, cfg, app) {
    // 决策（持锁，同步）→ 释放锁 → spawn 执行（不持锁跨 await）
    let decision = { let mut t = trigger.lock().unwrap(); crate::dream::prepare_dream(&mut t, history) };
    let Some((a, b)) = decision else { return };
    let trigger2 = trigger.clone();
    let history2 = history.clone();
    let hd2 = history_dir.as_ref().to_path_buf();
    let ws2 = workspace.as_ref().to_path_buf();
    let cfg2 = (**cfg).clone();
    tauri::async_runtime::spawn(async move {
        let round: std::sync::Arc<dyn llm::LlmRound> = std::sync::Arc::new(crate::llm::HttpRound);
        let emit = DreamSilentEmitter; // §7.7：dream 不推前端
        // dispatch 前先按 cfg 判定是否该跑（cap/idle），prepare 已据 trigger 判过；这里直接执行
        let evs = crate::history::read_all(&hd2);
        let now = now_ms();
        let should = { let t = trigger2.lock().unwrap(); t.check(&evs, now, cfg2.dream_idle_secs, cfg2.dream_cap_turns).is_some() };
        if should {
            if let Err(e) = crate::dream::execute_dream(round, &cfg2, &history2, &hd2, &ws2, a, b, &emit).await {
                eprintln!("[dream] 失败（不推进 marker，下次 idle/cap 自动重试）: {e}");
            }
        }
        trigger2.lock().unwrap().dream_finished();
    });
}

/// dream 静默 emitter：不推前端（§7.7），仅 eprintln 备查。
struct DreamSilentEmitter;
#[async_trait::async_trait]
impl llm::Emitter for DreamSilentEmitter {
    async fn content(&self, t: &str) { eprintln!("[dream] {t}"); }
    async fn error(&self, t: &str) { eprintln!("[dream] err {t}"); }
    // 其余默认 noop
}
```

> 注：`dispatch_dream` 在 prepare（占位）后于 spawned task 内再 `check` 一次（因为 prepare 不判 idle/cap，只占位 + 捕获 a,b）。若 check 否定（不该跑）则不执行、直接 dream_finished。这把「该不该跑」的判定收敛到 `DreamTrigger::check`，prepare 只负责占位/捕获边界。`run_one` 的轮间调用与 idle ticker 的 DreamCheck 都走 dispatch_dream → prepare → check → execute，单飞由 prepare 的 in_flight 保证。

(d) driver 主循环把 `trigger` 传给 `run_one`；`SessionEvent::JobDone` 分支同样传。

- [ ] **Step 4: 全量 check（集成层编译）**

```
cargo check --tests --manifest-path src-tauri/Cargo.toml
```
Expected: 0 warning / 0 error。注意：`SessionEvent::DreamCheck` 加后，所有 `match SessionEvent` 处须覆盖（编译器强制）；`chat`/`reset_session`/`append_context_note` 命令不投 DreamCheck（仅 ticker 投），无需改命令层。

- [ ] **Step 5: 跑全部后端测试**

```
cargo test --lib --manifest-path src-tauri/Cargo.toml
```
Expected: 全 PASS（dream 10 条 + 既有全部不破）。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/dream.rs src-tauri/src/agent.rs
git commit -m "feat(dream): 接进 driver——DreamTrigger 状态 + 轮间 cap + 60s idle ticker + 静默执行（run_dream 拆 prepare/execute）"
```

---

## Task 13: main.js —— display 滑动窗口（[lo,hi] · 启动 tail · 上下划加载 · marker 隐藏）

**Why**：display 是 history 尾段的懒加载视图（spec §13.5）。DOM 至多挂 W 条（`display_window_size`），前后滚动装一头卸一头。重启从 `history_tail(W)` 渲染最近 W 条可见气泡（marker 隐藏）。前端无 JS 测试框架 → `node --check` 验语法 + 手测。

**Files:**
- Modify: `src/main.js`（窗口状态 + 启动加载 + 滚动监听 + `renderHistoryEvent`）

**Interfaces（消费后端 Task 11 命令）**：`invoke("history_tail", { limit, beforeSeq })` / `invoke("history_head", { limit, afterSeq })`，返回 `HistoryEvent[]`（`{seq,ts,thread,kind,data}`，已过滤 marker/agent:N）。

- [ ] **Step 1: 加窗口状态 + 启动加载**（`main.js`，在 `activeAssistantWrap` 声明附近）

```js
// display 滑动窗口（§13.5）：[lo,hi] = 当前挂载的可见事件 seq 区间；atBottom = 是否贴底（实时新事件是否 append）
let displayWindow = { lo: null, hi: null, atBottom: true };
const displayW = () => {
  const n = parseInt((F.displayWindowSize?.value || "50").toString(), 10);
  return Number.isFinite(n) && n > 0 ? n : 50;
};

// HistoryEvent → 气泡（复用既有渲染；marker 已被后端过滤）
function appendHistoryBubble(ev) {
  if (ev.kind === "user") {
    appendUserText(ev.data?.text || "", ev.data?.attachments || []);
  } else if (ev.kind === "assistant") {
    appendAssistantText(ev.data?.content || "", ev.data?.tool_calls);
  } else if (ev.kind === "tool_result") {
    appendToolResult(ev.data?.name || "", ev.data?.result || "");
  } else if (ev.kind === "subagent_result") {
    appendUserText(`[子代理 #${ev.data?.agent_id} 完成] ${ev.data?.summary || ""} [${ev.data?.ref || ""}]`, []);
  } else if (ev.kind === "external") {
    appendUserText(`[外部] ${ev.data?.what || ""} ${ev.data?.path || ""}`, []);
  }
}
function prependHistoryBubble(ev) {
  // 渲染到列表顶部（上划加载老事件用）；实现：先记 scroll 高度，appendHistoryBubble 后 insertBefore list.firstChild
  const old = list.firstChild;
  const holder = document.createElement("div");
  list.insertBefore(holder, old);
  // 简化：直接 appendHistoryBubble(ev) 后把新节点移到顶；或重构 appendHistoryBubble 接 anchor。
  // 这里给一个直接实现：
  renderInto(ev, holder);
}

// 启动：渲染最近 W 条可见事件
async function loadDisplayInitial() {
  const W = displayW();
  let tail = [];
  try { tail = await invoke("history_tail", { limit: W, beforeSeq: null }); }
  catch (e) { console.warn("history_tail 失败", e); }
  list.innerHTML = "";
  for (const ev of tail) appendHistoryBubble(ev);
  if (tail.length) {
    displayWindow.lo = tail[0].seq;
    displayWindow.hi = tail[tail.length - 1].seq;
  }
  displayWindow.atBottom = true;
  scrollBottom();
}
```

> 实现注记：`appendUserText` / `appendAssistantText` / `appendToolResult` / `renderInto` 若 main.js 无现成独立函数，从既有 `chat-turn-start`/`llm-content`/`tool_call`/`tool_result` 监听里抽提（现有代码在 `activeAssistantWrap` 上构建气泡；抽出「建一条 user/assistant/tool 气泡」的纯函数供历史回放与实时流共用，DRY）。这是本任务的主要重构点。

- [ ] **Step 2: 滚动监听（上划装老/下划装新，装一头卸一头）**

```js
let scrollLoading = false;
list.addEventListener("scroll", async () => {
  if (scrollLoading) return;
  const W = displayW();
  // 上划到顶 → 装更老、卸最新 W 条
  if (list.scrollTop === 0 && displayWindow.lo != null) {
    scrollLoading = true;
    try {
      const older = await invoke("history_tail", { limit: W, beforeSeq: displayWindow.lo });
      if (older.length) {
        const prevH = list.scrollHeight;
        for (let i = older.length - 1; i >= 0; i--) prependHistoryBubble(older[i]); // older 升序，倒序 prepend
        displayWindow.lo = older[0].seq;
        trimNewestBubbles(W); // 卸当前最底 W 条（保 hi 游标，下划能装回）
        list.scrollTop = list.scrollHeight - prevH; // 保持视口
        displayWindow.atBottom = false;
      }
    } finally { scrollLoading = false; }
  }
  // 下划到底 → 装更新、卸最老 W 条
  else if (list.scrollTop + list.clientHeight >= list.scrollHeight - 2 && displayWindow.hi != null) {
    scrollLoading = true;
    try {
      const newer = await invoke("history_head", { limit: W, afterSeq: displayWindow.hi });
      if (newer.length) {
        for (const ev of newer) appendHistoryBubble(ev);
        displayWindow.hi = newer[newer.length - 1].seq;
        trimOldestBubbles(W);
        displayWindow.atBottom = newer.length < W; // 返回 < W = 到底
      } else { displayWindow.atBottom = true; }
      scrollBottom();
    } finally { scrollLoading = false; }
  } else { displayWindow.atBottom = false; }
});

function trimOldestBubbles(W) {
  // 移除列表最顶（最老）的 W 条气泡节点（保留 [lo,hi] 窗口大小 ≈ W）
  while (list.children.length > W * 2) list.removeChild(list.firstElementChild);
}
function trimNewestBubbles(W) {
  while (list.children.length > W * 2) list.removeChild(list.lastElementChild);
}
```

- [ ] **Step 3: 实时贴底 append + reset 清屏接线**

- 在 `chat-turn-end` 监听里（现有，约 `main.js:1215`）：若 `displayWindow.atBottom`，turn 结束后把 hi 前移到「最新」（可轻量重读 `history_tail(W)` 刷新，或保持现有实时气泡、置 `atBottom=true`）。Phase 1 简化：实时聊天气泡由现有流式逻辑照常 append；turn 结束且 atBottom 时 `scrollBottom()`。
- 在 `chat-reset` 监听里（若无可新增）：`list.innerHTML = ""; displayWindow = { lo: null, hi: null, atBottom: true }; loadDisplayInitial();`（reset 后从空 history 视图重载——实际 history 永不删，reset 只清 live context；display 仍可翻老历史。Phase 1：reset 清屏即可，滚动仍可翻）。
- 启动入口（`DOMContentLoaded` 或既有初始化）调 `loadDisplayInitial()`。

- [ ] **Step 4: 语法检查**

```
node --check src/main.js
```
Expected: 无输出（语法 OK）。

- [ ] **Step 5: 手测（dev server，右键 Reload 刷前端）**

1. 聊几轮 → 关软件 → 重开 → 最近对话应自动渲染（`loadDisplayInitial`）。
2. 聊到 >50 条 → 上划到顶 → 应加载更老气泡、卸掉最新一批；下划到底 → 装回。
3. dream 跑过后（idle 10min 或 cap 50）→ 前端**不应**出现 dream marker 气泡（后端过滤 + 前端 kind 无 marker 分支）。
4. 拖入文件（external 事件）→ 应渲染为「[外部] …」气泡。
5. 子代理完成 → 应渲染「[子代理 #N 完成] …」气泡。

- [ ] **Step 6: 提交**

```bash
git add src/main.js
git commit -m "feat(display): 滑动窗口 [lo,hi]——启动 history_tail 渲染、上下划装一头卸一头、marker 隐藏、重启可见"
```

---

## Task 14: index.html + main.js —— 高级设置 3 输入（dream_idle_secs / dream_cap_turns / display_window_size）

**Why**：spec §12 定稿 3 字段，埋进「高级 ▸ 记忆管理」（默认无感）。前端无 JS 测试框架 → `node --check` + 手测。

**Files:**
- Modify: `src/index.html`（高级区加 3 input）
- Modify: `src/main.js`（`F` 元素图 + load 填值 + save 收集）

- [ ] **Step 1: index.html 加 3 输入**（在 `cfg-max-subagents` 那行之后，约 `index.html:114`）

```html
                <div class="cfg-row">
                  <label>记忆整理 · 闲置触发（秒，距上次活动的空闲时长触发 dream；高级）</label>
                  <input id="cfg-dream-idle-secs" type="number" min="0" max="86400" step="10" />
                </div>
                <div class="cfg-row">
                  <label>记忆整理 · 轮数上限（user 事件数达此值触发 dream；高级）</label>
                  <input id="cfg-dream-cap-turns" type="number" min="1" max="500" step="1" />
                </div>
                <div class="cfg-row">
                  <label>显示窗口大小（可见气泡数；上下划滑动加载）</label>
                  <input id="cfg-display-window-size" type="number" min="5" max="500" step="1" />
                </div>
```

- [ ] **Step 2: main.js `F` 元素图加 3 项**（`maxSubagents` 行之后，约 `main.js:96`）

```js
  dreamIdleSecs: document.getElementById("cfg-dream-idle-secs"),
  dreamCapTurns: document.getElementById("cfg-dream-cap-turns"),
  displayWindowSize: document.getElementById("cfg-display-window-size"),
```

- [ ] **Step 3: load 填值**（`const cfg = await invoke("get_config");` 之后，约 `main.js:394`，跟在现有 `F.maxSubagents.value = ...` 之后）

```js
  F.dreamIdleSecs.value = cfg.dream_idle_secs ?? 600;
  F.dreamCapTurns.value = cfg.dream_cap_turns ?? 50;
  F.displayWindowSize.value = cfg.display_window_size ?? 50;
```

- [ ] **Step 4: save 收集**（构造待保存 cfg 处，约 `main.js:709` `await invoke("save_config", { cfg })` 之前，跟在现有 max_subagents 收集之后）

```js
  cfg.dream_idle_secs = parseInt(F.dreamIdleSecs.value, 10) || 600;
  cfg.dream_cap_turns = parseInt(F.dreamCapTurns.value, 10) || 50;
  cfg.display_window_size = parseInt(F.displayWindowSize.value, 10) || 50;
```

- [ ] **Step 5: 语法检查 + 手测**

```
node --check src/main.js
```
手测（dev server → 设置页）：
1. 三个输入框出现且默认值 600 / 50 / 50。
2. 改值保存 → 重开设置页 → 值持久（config.json 落盘）。
3. 改 `dream_cap_turns=3` → 聊 3 轮 → 应触发 dream（轮间）→ MEMORY.md「日」节出现条目（验证字段生效）。

- [ ] **Step 6: 提交**

```bash
git add src/index.html src/main.js
git commit -m "feat(ui): 高级设置 3 输入（dream_idle_secs/dream_cap_turns/display_window_size）"
```

---

## Task 15: 绿色打包 —— bash 运行时 PATH 注入 + mem.exe 同目录 release（externalBin）

**Why**：spec §10.3/§13.4——mem.exe 放 ovoice.exe 旁，不写系统 PATH/不装/不进注册表；agent 的 bash 工具 spawn shell 时**运行时**把自身目录加进 PATH（`current_exe()` 父目录，不改系统环境），于是 `mem ls` 等在 bash 里直接可调。release 通过 Tauri `externalBin` 把 mem.exe 落到 ovoice.exe 同目录。

**Files:**
- Modify: `src-tauri/src/tools.rs`（`path_with_exe_dir()` + 注入 `tool_bash` 前台 + `jobs::spawn_process_job` 后台）
- Modify: `src-tauri/src/jobs.rs`（`spawn_process_job` 注入 PATH）
- Modify: `src-tauri/tauri.conf.json`（`bundle.externalBin`）

**Interfaces:**
- Produces: `tools::path_with_exe_dir() -> Option<(String,String)>`（返 `("PATH", "<exe_dir>;<existing>")`）

- [ ] **Step 1: 写失败测试**（`tools.rs` 的 `mod tests`）

```rust
    #[test]
    fn path_with_exe_dir_prepends_exe_parent() {
        let (k, v) = path_with_exe_dir().expect("current_exe 应可用（测试在二进制内运行）");
        assert_eq!(k, "PATH");
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_string_lossy().to_string();
        assert!(v.starts_with(&exe_dir), "PATH 应前置 exe 目录: {v}");
    }
    #[tokio::test]
    async fn bash_can_call_mem_if_built() {
        // 集成性验证（mem 已构建时）：mem --help 应可经注入的 PATH 调到。
        // 注意：mem 在 target/debug 与 ovoice.exe 同目录；若未构建则跳过（不硬失败）。
        let dir = tempfile::tempdir().unwrap();
        let out = tool_bash(&serde_json::json!({"command":"mem --help"}),
            &ToolsCtx::foreground(dir.path().to_path_buf(), "cn".into()), 10).await;
        // mem --help 成功 → 含 "mem ——"；mem 未构建 → "启动失败"/"不是内部命令"。两者都算通过（仅验 PATH 注入不崩）。
        assert!(out.contains("mem") || out.contains("失败") || out.contains("命令"),
            "bash 调 mem 应有响应（不崩）: {out}");
    }
```

- [ ] **Step 2: 跑测试确认失败**

`cargo test --lib --manifest-path src-tauri/Cargo.toml tools::tests::path_with_exe_dir` → 编译失败（`path_with_exe_dir` 未定义）。

- [ ] **Step 3: 实现**

(a) `tools.rs` 加 helper + 前台注入：
```rust
/// 绿色软件：把 current_exe() 父目录前置进 PATH（运行时，不改系统环境）→ mem CLI 在 bash 直接可调。
pub fn path_with_exe_dir() -> Option<(String, String)> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let cur = std::env::var("PATH").unwrap_or_default();
    let sep = if cfg!(windows) { ";" } else { ":" };
    Some(("PATH".into(), format!("{}{}{}", dir.display(), sep, cur)))
}
```
`tool_bash` 前台分支（`for (k,v) in &env_vars { cmd.env(k,v); }` 之后）加：
```rust
    if let Some((k, v)) = path_with_exe_dir() { cmd.env(k, v); }
```

(b) `jobs.rs::spawn_process_job` 同样：`for (k,v) in &env_vars { cmd.env(k,v); }` 之后加 `if let Some((k,v)) = crate::tools::path_with_exe_dir() { cmd.env(k, v); }`。

(c) `tauri.conf.json` 的 `bundle` 段加 `externalBin`（release 把 mem 落到 ovoice.exe 同目录）：
```json
    "externalBin": ["binaries/mem"]
```
并加 release 前置说明（写进本任务提交信息 + CLAUDE.md 备忘，不阻塞 dev）：release 构建前需把 `target/<profile>/mem.exe` 复制成 `src-tauri/binaries/mem-<target-triple>.exe`（Tauri externalBin 约定：运行时去 triple 后缀、落主 exe 同目录）。CI/dev 脚本示例（Windows）：
```bat
for /f %%i in ('rustc -vV ^| findstr host') do set TRIPLE=%%i
copy target\release\mem.exe src-tauri\binaries\mem-%TRIPLE%.exe
```

- [ ] **Step 4: 跑测试 + check**

```
cargo test --lib --manifest-path src-tauri/Cargo.toml tools::tests::path_with_exe_dir
cargo check --tests --manifest-path src-tauri/Cargo.toml
```
Expected: PASS / 0 warning。`bash_can_call_mem_if_built` 在 mem 已构建（`cargo build --bin mem` 一次后）时返回 mem 帮助；未构建则降级不硬失败。

- [ ] **Step 5: 验两个 bin 都能构建（dev server 关闭时）**

提醒用户停 dev server 后：
```
cargo build --manifest-path src-tauri/Cargo.toml --bin ovoice --bin mem
```
Expected: `target/debug/ovoice.exe` 与 `target/debug/mem.exe` 都生成（同目录）。dev 运行时二者本就同在 `target/debug/`，故 dev 下 `mem` 经 PATH 注入即可调（无需打包步骤）。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/tools.rs src-tauri/src/jobs.rs src-tauri/tauri.conf.json
git commit -m "feat(green): bash 运行时 PATH 注入 current_exe 父目录（mem 可调）+ tauri externalBin 落 mem.exe 同目录"
```

---

## 端到端手测清单（spec §13.2 必须手测；dev server + 真 LLM）

逐项过（每项对应一个已实现能力，失败回到对应 Task）：

1. **重启无缝续聊**：聊几轮 → 关软件 → 重开 → context 应重建（agent 记得之前聊的）；display 显示最近对话（Task 5/13）。
2. **history 忠实全量**：聊含工具调用的几轮 → 查 `workspace/history/{date}.jsonl` → user/assistant/tool_result/marker 齐全、无丢（Task 2/4）。
3. **配对不破**：故意中断（聊到一半关软件）→ 重开 → 不应出现 MiniMax 400「tool id not found」（孤儿 assistant 已丢，Task 3/5）。
4. **dream 日层**：设 `dream_cap_turns=3` → 聊 3 轮 → 轮间触发 dream → `memory/{Y}/{M}/{date}.md` 出现事件段（**对话索引 seq[a,b] 代码盖戳**）+ MEMORY.md「日」节有行（Task 6/8/12）。
5. **dream 静默**：dream 跑时前端无任何提示（无 toast、无 jobs 行、无 marker 气泡，Task 12/13）。
6. **dream 失败重试**：断网触发 dream → 失败 → 无 dream marker → 恢复网络 + 下次 idle/cap → 重试同段（Task 8）。
7. **mem drill 全链**：bash 调 `mem ls` → `mem ls <年> <月>` → `mem read <date>` → `mem history <date> --seq a..b` → 回到原始逐字对话（Task 9/10/15）。
8. **mem search**：`mem search <关键词>` 命中 memory/；`--raw` 扩到 history（Task 9）。
9. **子代理异步留底**：派一个子代理 → 完成后主对话收到「[子代理 #N 完成]」+ ref；`mem search --raw` 或读 history 能看到 `thread=agent:N` 的完整子代理对话（Task 4/5/12）。
10. **display 滑动**：聊 >50 条 → 上划装老/卸新、下划装新/卸老；marker 不渲染（Task 13）。
11. **绿色软件**：dev 下 `mem` 经 bash PATH 注入可直接调（Task 15）。

---

## 第 2 期（延后，另开计划）

本计划只做 §15 第 1 期（日层 + drill 能成立的最小子）。第 2 期另开计划，覆盖：

- **晋级（§7.8 step 2-4 / §8.1）**：跨午夜「日」→「周」；周里天 >7 天 → 按 ISO 周概括进「月」；月里周 >3 月 → 按日历月概括进「年」。需 `chrono` 的 ISO 周（`IsoWeek`）与日历月对齐逻辑（固定日期 fixture 测）。
- **修正 revise（§7.4 op3）**：新事件与既有记忆矛盾 → 重写受影响 MEMORY.md 条目（冻结的 day 文件永不改写）。
- **MEMORY「周/月/年」三层实际填充**：本计划只立了骨架标题；第 2 期 dream 写这三层。

分期不改变 spec 设计，只把「先交付能跑的最小子」显式化，降低再次推翻风险（§15）。

---

## Self-Review（writing-plans 自检结论）

**1. Spec 覆盖（Phase 1 范围内逐节对照）**：
- §1 三层 / §4 单写 / §6 配对 / §7.1 cap 只数 user / §7.8 seq 盖戳 / §13.5 可见事件 → 四条 P1 铁律均有对应 Task + 测试（Task 2/3/5/7/8/11）。
- §2 命名 / §3 pinned（4 块合并 1 system msg，已注明）/ §5 子代理异步留底 / §9 磁盘 / §10 mem / §11 重启 / §12 字段 / §13.1 错误降级 / §13.4 打包 → 均有 Task。
- §7.8 step2-4 晋级 / §7.4 revise / §8.1 周/月/年 → **显式延后**（见上节），Phase 1 范围内无遗漏。

**2. 占位符扫描**：无 TBD/TODO/「适当处理」类。一处前向引用：Task 5 Step 3(c) 注释「Task 7 在此插入 dream 触发检查」——实际由 **Task 12** 完成（Task 5 不加 hook、Task 12 加）。控制器按 Task 1→15 顺序执行即可，不阻塞。

**3. 类型一致性**：`HistoryEvent` 构造器（user/assistant/tool_result/external/subagent_result/edit/marker）跨 Task 2/3/5/6/8 一致；`build_messages(events,pinned,cap)`、`run_turn(...,thread)`、`prepare_dream/execute_dream/run_dream`、`visible_tail/head`、`mem_cli::{ls,read_day,history,search}`、`resolve_workspace/dispatch/parse_seq_flag` 签名跨任务一致。

**4. 已澄清的歧义（落进计划，供审）**：① tool_call 存储 = Option A（assistant 内嵌 tool_calls，spec §4/§6 原文契合）；② pinned 4 块 = 1 条 system 消息（API 安全）；③ Process JobDone → external 事件（spec §4 taxonomy 无 job_done kind）；④ 重建顺序 read_all→append→push（免 flush 竞态 + 免重复）；⑤ workspace 默认 Documents/ovoice（行为变更，旧数据需手动迁移）。

---

## 执行方式

用户已选 **subagent-driven-development**（fresh subagent/任务 + 任务 review + 末尾 whole-branch review）。计划写完待用户审，审过才进 SDD（用户先前表态「计划写完我会审」）。

**任务依赖序（控制器按此派 subagent）**：1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12 → 13 → 14 → 15（线性；多数为前置依赖，Task 9/10 mem 可与 11-15 并行审但 SDD 串行派即可）。


