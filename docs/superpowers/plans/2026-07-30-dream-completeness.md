# dream 完整性改造 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 dream 提取从「LLM 主观裁剪 + 坏行静默丢」改造成「回合制 + 每回合必出（程序化机械兜底）+ LLM 增强（受约束合并）+ 留尾防失忆」，并修复 P0：`build_messages` 认 `marker.seq` 而非 `until_seq` 导致留尾根本不工作。

**Architecture:** dream 段切成「回合」（一个 user + 后续 assistant/tool_result/external/subagent_result）。每个回合先经 `mechanical_extract` 产出程序化真相（机械底座，保完整）；LLM 再把相邻同主题回合合并成分组（受 `max_rounds` + 连续性约束）；`reconcile_groups` 强制全覆盖（漏的用机械底座补）+ 非连续拆子组 + 超限强制拆。留尾 `tail_aware_b` 让最近 N 回合不进 dream，`build_messages` 改认 `marker.until_seq` 后 tail 下轮仍可见。落盘 IO 失败 propagate（不写 marker、段重试）。

**Tech Stack:** Rust（src-tauri），tokio async，serde_json，现有 `HistoryEvent`/`HistoryWriterHandle`/`LlmRound`/`Emitter` trait，`tempfile` 测试。MiniMax-M3 LLM。

**Spec:** `docs/superpowers/specs/2026-07-30-dream-completeness-design.md`（v2，已吸收 13 review findings）。**Review:** `docs/superpowers/specs/2026-07-30-dream-completeness-review.md`。

## Global Constraints

- **分支**：`feat/dream-completeness`（基于 master `bf37d6a`），严禁直接落 master。每个 Task 末尾独立 commit，信息以 `Co-Authored-By: Claude <noreply@anthropic.com>` 结尾。
- **dev server 锁 exe**（[[ovoice-dev-server-cargo-lock]]）：验证只用 `cargo check --tests --manifest-path src-tauri/Cargo.toml` + `cargo test --lib --manifest-path src-tauri/Cargo.toml <module>::tests`，**不** `cargo build`/`cargo run`（debug 下锁 exe）。
- **数据保留硬约束（D2）**：`memory/` day 文件 + `MEMORY.md` + `history/*.jsonl` 全部 append-only，新逻辑只增不删旧；旧 day 文件/MEMORY.md 原样保留（N5 不回填）。
- **不改 mem 读端契约**：`mem_cli.rs` 零改；新 dream 产出的 day 文件段格式（`## HH:MM evt-NNN title` + `**对话索引**: history/date.jsonl#seq[a,b]`）与 mem 现有解析兼容。F6 连续保证 seq 并集 `[min_a,max_b]` drill 不含其它组回合。
- **F1 影响面**：改 `context.rs::build_messages`（主对话历史路径）——必须配 P0 回归测试（Task 2）+ 充分手测。
- **命令行环境**：Windows PowerShell + Git Bash；ovoice `tool_bash` 跑 POSIX sh（busybox），非 cmd。
- **路径锚定**：本 plan 所有 `file:line` 已对照 master `bf37d6a` 核对。实现时若行号漂移（先前 Task 改动），以函数名/签名定位为准。
- **工具计数级联**（[[ovoice-tool-count-cascade]]）：本改造不动 `tools.rs`，工具数不变；但改 `llm.rs`/`agent.rs` 时勿误改 `tools.len()` 断言。

## Spec 关键决策（已锁，写入本 plan 供 implementer 参考）

- **F1=A**：`build_messages` 的 `last_marker_seq` 改读 `marker.data["until_seq"]`（非 `marker.seq`）。
- **F9=A（温和合并）**：`MechanicalEvent` 并入 `FinalEvent`，用 `round_idx: Option<usize>` 区分（`Some` = 机械底座供 reconcile 用；`None` = 已落盘最终事件）。
- **F11=A（强制拆）**：单组回合数超 `dream_merge_max_rounds` → 强制拆成 max_rounds 块，保留 LLM title/detail + `(续K)` 后缀。
- **F7**：`tail_aware_b` 移入 `execute_dream` 内（用 execute 透传的 events 同源算 b）；`prepare_dream` 只捕获 `a`，不再算 `b`。
- **F8**：`execute_dream` 接 `events: &[HistoryEvent]`（dispatch_dream 决策阶段 read_all 一次透传），删内部 `read_all`。
- **F4**：`append_day_event` 不再 `let _ =`，Err 用 `?` propagate（不写 marker、段重试）。
- **plan 对 spec 的细化（spec 未写透，本 plan 补）**：
  - **`execute_dream` 返回类型 `Result<u64, String>`**：`Ok(b)` 中 `b` = 本次 marker 的 `until_seq`（正常 = `tail_aware_b` 结果；空段早退 = `cur`）；`Err` = 失败不推进。`dispatch_dream`/`run_dream` 用返回值推进 `last_dream_marker_seq`（解决 spec §5.8 `Ok(())` 但 dispatch 需 `b` 推进的缺口）。
  - **`cur` = events 过滤后（main、非 marker、`seq >= a`）的最大 seq**，不调 `history.current_seq()`（透传同源；无并发时与 `current_seq()` 相等）。理由：F7 要求 tail 用同源 events 算，`current_seq()` 跨 await 可能漂移。

## File Structure

| 文件 | 责任 | 本 plan 改动 |
|---|---|---|
| `src-tauri/src/config.rs` | Config 持久化 | 加 `dream_tail_rounds`/`dream_merge_max_rounds` 字段 + 默认函数（Task 1） |
| `src-tauri/src/context.rs` | history → LLM messages 重建 | `last_marker_seq` 改读 `until_seq`（Task 2，F1 P0） |
| `src-tauri/src/memory.rs` | day 文件 + MEMORY.md 读写 | 暴露 `day_event_count_pub`（Task 3，F10） |
| `src-tauri/src/dream.rs` | dream 提取子代理 | 新增纯逻辑层（Task 4/5）+ execute/prepare/run 重写（Task 6）+ 新集成测试（Task 7） |
| `src-tauri/src/agent.rs` | driver + dispatch_dream | dispatch_dream 透传 events + 接新签名（Task 8，F8） |
| `src-tauri/defaults/AGENT.md` | 助手规范 | dream 行为说明更新（Task 9） |
| `docs/dream-flow.md` | dream 流程参考文档 | 流程图 + 提示词原文更新（Task 9） |

---

## Task 1: config 加 dream_tail_rounds + dream_merge_max_rounds

**Files:**
- Modify: `src-tauri/src/config.rs:89-95`（Config dream 字段段）+ `src-tauri/src/config.rs:102-105`（默认函数段）
- Test: `src-tauri/src/config.rs`（`#[cfg(test)]` 段，新增或复用现有 tests mod）

**Interfaces:**
- Produces: `Config::dream_tail_rounds: u64`（默认 3）、`Config::dream_merge_max_rounds: u64`（默认 5），供 Task 6 execute_dream 读取。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/config.rs` 的 `#[cfg(test)] mod tests` 内（若无 tests mod 则新建；现有 config.rs 可能无 tests mod——若如此，在文件末尾加）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dream_defaults() {
        let c = Config::default();
        assert_eq!(c.dream_tail_rounds, 3, "留尾默认 3 回合");
        assert_eq!(c.dream_merge_max_rounds, 5, "合并上限默认 5 回合");
        // 既有默认不回归
        assert_eq!(c.dream_idle_secs, 7200);
        assert_eq!(c.dream_cap_turns, 50);
    }

    #[test]
    fn dream_fields_survive_json_roundtrip() {
        let c = Config { dream_tail_rounds: 7, dream_merge_max_rounds: 10, ..Config::default() };
        let json = serde_json::to_string(&c).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dream_tail_rounds, 7);
        assert_eq!(back.dream_merge_max_rounds, 10);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cargo test --lib --manifest-path src-tauri/Cargo.toml config::tests
```
Expected: 编译失败（`dream_tail_rounds`/`dream_merge_max_rounds` 字段不存在）。

- [ ] **Step 3: 实现 —— 加字段**

`src-tauri/src/config.rs`，在 `display_window_size` 字段之后（约 `:94-95`）、`max_tool_iters` 字段之前插入：

```rust
    // dream 完整性改造：留尾回合数（最近 N 个 user 回合不进 dream，下轮 build_messages 仍可见）。
    #[serde(default = "d_dream_tail_rounds")]
    pub dream_tail_rounds: u64,
    // dream 合并组上限：单个 LLM 分组最多覆盖多少回合；超出强制拆（F11）。
    #[serde(default = "d_dream_merge_max_rounds")]
    pub dream_merge_max_rounds: u64,
```

- [ ] **Step 4: 实现 —— 加默认函数**

`src-tauri/src/config.rs`，在 `d_display_window`/`d_max_tool_iters` 默认函数段（约 `:104-105`）后加：

```rust
fn d_dream_tail_rounds() -> u64 { 3 }
fn d_dream_merge_max_rounds() -> u64 { 5 }
```

- [ ] **Step 5: 跑测试确认通过 + 零 warning**

```bash
cargo check --tests --manifest-path src-tauri/Cargo.toml
cargo test --lib --manifest-path src-tauri/Cargo.toml config::tests
```
Expected: PASS（2 个新测试 + 编译零 warning）。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/config.rs
git commit -m "feat(config): dream_tail_rounds + dream_merge_max_rounds（默认 3/5）" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 2: context.rs F1 P0 — build_messages 认 until_seq

**背景**：`context.rs:70-72` `last_marker_seq` 取 `marker.seq`（marker 事件在 history 的位置），filter `e.seq > marker.seq`。dream marker 在 `execute_dream` 末尾 append → `marker.seq > until_seq`。现状因 `b=current_seq`、无并发时 `marker.seq == until_seq == b` 巧合相等没暴露；引入留尾（`b = cur − tail`）后 `marker.seq > until_seq` → tail 全被排除 → 留尾静默失效。**改认 `until_seq` 修这个 P0 + 顺带修现状潜在 bug**（dream 期间用户发的事件 seq ∈ (b, marker.seq) 也被切掉）。

**Files:**
- Modify: `src-tauri/src/context.rs:69-72`（`last_marker_seq` 函数）
- Test: `src-tauri/src/context.rs:162-352`（现有 tests mod，加回归测试）

**Interfaces:**
- Consumes: `HistoryEvent`（`kind=="marker"` 且 `data["until_seq"]: u64`）。
- Produces: `last_marker_seq` 语义从「marker 位置 seq」→「marker.until_seq」；`build_messages` filter `e.seq > until_seq`。

- [ ] **Step 1: 写失败测试（P0 回归）**

在 `src-tauri/src/context.rs` 的 `mod tests`（约 `:162` 起）末尾加：

```rust
    // ─── F1 P0 回归：build_messages 认 until_seq 而非 marker.seq ───

    #[test]
    fn tail_events_survive_marker_in_next_build_messages() {
        // dream 跑后：marker.seq=5（append 时位置），until_seq=2（b=cur−tail）。
        // build_messages 应认 until_seq=2 → tail 事件（seq 3,4）保留可见。
        // 旧逻辑认 marker.seq=5 → tail 全被切掉（P0 bug）。
        let mut evs = seq_events(&["user", "user"]);           // seq 0,1（dream 前）
        // 模拟 dream 段已被 marker 收尾：marker until_seq=2（b=2），marker 自己 seq=5
        let mut m = HistoryEvent::marker(2000, "dream", 2);    // until_seq = 2
        m.seq = 5;
        evs.push(m);
        // tail 事件（dream 之后、marker 之前发生的；seq ∈ (2, 5)）
        let mut t1 = HistoryEvent::user(3000, "main", "tail 消息 1", &[]);
        t1.seq = 3;
        let mut t2 = HistoryEvent::user(4000, "main", "tail 消息 2", &[]);
        t2.seq = 4;
        evs.push(t1);
        evs.push(t2);
        let m_out = build_messages(&evs, &pinned("S"), 50);
        let users: Vec<&str> = m_out.iter()
            .filter(|x| x["role"] == "user")
            .filter_map(|x| x["content"].as_str())
            .collect();
        assert!(users.iter().any(|c| c.contains("tail 消息 1")), "tail 事件 seq=3 应保留可见（认 until_seq=2）: {users:?}");
        assert!(users.iter().any(|c| c.contains("tail 消息 2")), "tail 事件 seq=4 应保留可见: {users:?}");
    }

    #[test]
    fn dream_during_events_survive_after_marker() {
        // dream 期间新事件（marker 之后落盘）也应可见（顺带修的现状 bug）。
        let mut evs = seq_events(&["user"]);                   // seq 0
        let mut m = HistoryEvent::marker(1000, "dream", 1);    // until_seq=1, marker.seq=1
        m.seq = 1;
        evs.push(m);
        // marker 之后的新事件
        let mut after = HistoryEvent::user(3000, "main", "marker 后新消息", &[]);
        after.seq = 5;
        evs.push(after);
        let m_out = build_messages(&evs, &pinned("S"), 50);
        let has_after = m_out.iter().any(|x| x["content"].as_str().unwrap_or("").contains("marker 后新消息"));
        assert!(has_after, "marker 后新事件应可见（认 until_seq）");
    }

    #[test]
    fn marker_until_seq_equal_marker_seq_legacy_unchanged() {
        // 向后兼容：旧数据 marker.seq == until_seq（无 tail 时），filter 行为不变。
        let mut evs = seq_events(&["user", "user"]);           // seq 0,1
        let mut m = HistoryEvent::marker(2000, "dream", 2);    // until_seq=2 == marker.seq=2
        m.seq = 2;
        evs.push(m);
        let mut after = HistoryEvent::user(3000, "main", "新", &[]);
        after.seq = 3;
        evs.push(after);
        let m_out = build_messages(&evs, &pinned("S"), 50);
        // marker 后只 1 个 user
        let users: Vec<&serde_json::Value> = m_out.iter().filter(|x| x["role"] == "user").collect();
        assert_eq!(users.len(), 1, "until_seq==marker.seq 时行为不变");
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cargo test --lib --manifest-path src-tauri/Cargo.toml context::tests::tail_events_survive
```
Expected: FAIL（`tail 消息 1/2` 不在输出里——旧逻辑 `marker.seq=5` 把 seq 3,4 切掉）。

- [ ] **Step 3: 实现 —— 改 last_marker_seq**

`src-tauri/src/context.rs:69-72`，替换整个函数：

```rust
/// 取最后一个 marker（dream 或 reset）的 `until_seq`（dream 留尾边界）；无 marker → None（从头重建）。
///
/// **F1 P0**：旧实现取 `marker.seq`（marker 事件在 history 的位置序号），但 marker 在 dream
/// 末尾 append → `marker.seq > until_seq`，filter `e.seq > marker.seq` 会把 tail（seq ∈ (until_seq, marker.seq)）
/// 全切掉，留尾静默失效。改认 `data["until_seq"]`：所有 marker 都带（dream `marker(now,"dream",b)`、
/// reset `marker(ts,"reset",seq)` —— `HistoryEvent::marker` 第三参即 until_seq）。
/// 向后兼容：无 tail 时 `marker.seq == until_seq`，行为不变。
fn last_marker_seq(events: &[HistoryEvent]) -> Option<u64> {
    events.iter()
        .filter(|e| e.kind == "marker")
        .filter_map(|e| e.data.get("until_seq").and_then(|v| v.as_u64()))
        .max()
}
```

- [ ] **Step 4: 跑测试确认通过 + 现有 context 测试不回归**

```bash
cargo check --tests --manifest-path src-tauri/Cargo.toml
cargo test --lib --manifest-path src-tauri/Cargo.toml context::tests
```
Expected: PASS（3 个新测试 + 现有 `marker_window_only_after_last_marker`/`reset_marker_same_boundary_as_dream` 仍绿——它们的 test helper `marker(ts,"dream",seq)` 设 `until_seq=seq`，巧合相等，改后 filter 结果一致）。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/context.rs
git commit -m "fix(context): F1 P0 build_messages 认 marker.until_seq（留尾才真生效）" -m "旧 last_marker_seq 取 marker.seq（位置），marker 末尾 append → marker.seq>until_seq → tail 被切 → 留尾静默失效。改读 data.until_seq；顺带修 dream 期间新事件被切的现状 bug。向后兼容（无 tail 时两者相等）。+3 回归测试。" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 3: memory.rs 暴露 day_event_count_pub（F10 evt_no 同源）

**背景**：day 文件 evt-NNN 用 `day_event_count`（当天累计段数 +1，`memory.rs:36`）；但 MEMORY.md 日节 evt_no 现状由 dream 调用方按 `(i+1)` 给（`dream.rs:172`），一天两次 dream → 编号对不上。统一用 `day_event_count` 起算（Task 6 execute 落盘时调）。

**Files:**
- Modify: `src-tauri/src/memory.rs:22-26`（`day_event_count` 私有函数附近，加 pub 包装）
- Test: `src-tauri/src/memory.rs` `#[cfg(test)] mod tests`（若无则新建）

**Interfaces:**
- Produces: `pub fn day_event_count_pub(cache: &Path, ts: u64, offset_secs: i64) -> u32`——返回当天 day 文件已有事件段数（`## ` 开头行数），供 Task 6 算 evt_no。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/memory.rs` 末尾的 `#[cfg(test)] mod tests`（若不存在则新建）加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_day(cache: &std::path::Path, date: &str, body: &str) {
        let (y, m) = {
            let v: Vec<&str> = date.split('-').collect();
            (v[0], v[1])
        };
        let p = cache.join("memory").join(y).join(m).join(format!("{date}.md"));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, body).unwrap();
    }

    #[test]
    fn day_event_count_pub_counts_segments() {
        let dir = tempfile::tempdir().unwrap();
        // offset=0 → ts=0 对应 1970-01-01
        write_day(dir.path(), "1970-01-01",
            "## 00:00 evt-19700101-001 A\n**主语**: 用户\n**详情**: d\n**对话索引**: x\n## 00:01 evt-19700101-002 B\n**主语**: 用户\n**详情**: d\n**对话索引**: x\n");
        let n = day_event_count_pub(dir.path(), 0, 0);
        assert_eq!(n, 2, "应数到 2 个 ## 段");
    }

    #[test]
    fn day_event_count_pub_zero_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let n = day_event_count_pub(dir.path(), 0, 0);
        assert_eq!(n, 0, "文件不存在 → 0");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cargo test --lib --manifest-path src-tauri/Cargo.toml memory::tests
```
Expected: 编译失败（`day_event_count_pub` 未定义）。注：若 memory.rs 无 `#[cfg(test)]`，`tempfile` 已是 dev-dep（dream.rs 测试在用），无需加。

- [ ] **Step 3: 实现 —— 加 pub 包装**

`src-tauri/src/memory.rs`，紧跟私有 `day_event_count`（`:23-26`）之后加：

```rust
/// `day_event_count` 的公开包装：按 (cache, ts, offset) 定位 day 文件后数段数（F10 evt_no 同源用）。
pub fn day_event_count_pub(cache: &Path, ts: u64, offset_secs: i64) -> u32 {
    day_event_count(&day_file_path(cache, ts, offset_secs))
}
```

- [ ] **Step 4: 跑测试确认通过 + 零 warning**

```bash
cargo check --tests --manifest-path src-tauri/Cargo.toml
cargo test --lib --manifest-path src-tauri/Cargo.toml memory::tests
```
Expected: PASS（2 新测试）。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/memory.rs
git commit -m "feat(memory): day_event_count_pub 暴露（F10 evt_no 同源用）" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 4: dream.rs 纯逻辑层 A — Round/split_rounds + FinalEvent/mechanical_extract + tail_aware_b

**背景**：回合切分 + 机械底座（程序化真相，保完整）+ 留尾边界。全部纯函数（无 LLM/IO），可完全单测。这是 G1 完整性的硬保证基础。

**Files:**
- Modify: `src-tauri/src/dream.rs`（在 `parse_extracts` 之后、`// ─── 月主题` 注释 `:271` 之前插入新代码块；或放 `dream_prompt` 之前的工具函数区）
- Test: `src-tauri/src/dream.rs` `mod tests`（`:296` 起）

**Interfaces:**
- Produces:
  - `struct Round<'a>`（`seq_a/seq_b/last_ts/user_text/assistant/tools`）
  - `fn split_rounds<'a>(segment: &'a [&HistoryEvent]) -> Vec<Round<'a>>`
  - `struct FinalEvent`（`round_idx: Option<usize>, seq_a/seq_b/last_ts, title/detail/subject`）
  - `fn mechanical_extract(idx: usize, r: &Round) -> FinalEvent`
  - `fn tail_aware_b(events: &[HistoryEvent], cur: u64, tail_rounds: u64) -> u64`
  - 辅助 `collapse_ws`/`truncate_chars`/`truncate_title`/`truncate_first_sentence`/`has_tool_calls`
- Consumes: `HistoryEvent`（字段 `seq/ts/kind/data`，均 pub）。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/dream.rs` `mod tests`（`:296` 起，在现有 trigger 测试之后、`// ─── Task 8: run_dream` 注释 `:425` 之前插入）加：

```rust
    // ─── Task 4: 纯逻辑层 A（split_rounds / mechanical_extract / tail_aware_b）───

    fn asst(seq: u64, ts: u64, content: &str) -> HistoryEvent {
        let mut e = HistoryEvent::assistant(ts, "main", content, "", vec![]);
        e.seq = seq; e
    }
    fn asst_tc(seq: u64, ts: u64) -> HistoryEvent {
        let mut e = HistoryEvent::assistant(ts, "main", "", "", vec![
            serde_json::json!({"id":"c","type":"function","function":{"name":"bash","arguments":"{}"}})
        ]);
        e.seq = seq; e
    }
    fn tool(seq: u64, ts: u64) -> HistoryEvent {
        let mut e = HistoryEvent::tool_result(ts, "main", "bash", "ok", "c");
        e.seq = seq; e
    }

    #[test]
    fn split_rounds_single_user_then_assistant() {
        let seg = vec![user(1, 1000), asst(2, 2000, "回复")];
        let r = split_rounds(&seg);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].seq_a, 1);
        assert_eq!(r[0].seq_b, 2);
        assert_eq!(r[0].user_text, "u");
        assert_eq!(r[0].assistant.len(), 1);
    }

    #[test]
    fn split_rounds_multiple_rounds_boundary_at_user() {
        let seg = vec![
            user(1, 1000), asst(2, 2000, "a1"),
            user(3, 3000), asst(4, 4000, "a2"), tool(5, 5000),
            user(6, 6000),
        ];
        let r = split_rounds(&seg);
        assert_eq!(r.len(), 3, "3 个 user → 3 回合");
        assert_eq!(r[0].seq_b, 2, "回合0 到 assistant seq=2");
        assert_eq!(r[1].seq_b, 5, "回合1 含 tool_result seq=5");
        assert_eq!(r[1].tools.len(), 1);
        assert_eq!(r[2].seq_a, 6);
    }

    #[test]
    fn split_rounds_leading_non_user_attaches_to_next_user() {
        // 段首非 user 前导事件（理论边界）：挂到下一个 user 回合（实现取此）
        let seg = vec![asst(0, 500, "前导"), user(1, 1000), asst(2, 2000, "r")];
        let r = split_rounds(&seg);
        assert_eq!(r.len(), 1, "前导 assistant 不单成回合，并入下一个 user");
        assert_eq!(r[0].seq_a, 1);
    }

    #[test]
    fn mechanical_extract_title_truncated_and_punct_stripped() {
        let r = Round { seq_a: 1, seq_b: 2, last_ts: 2000,
            user_text: "这是一段非常非常长的用户消息，后面还有标点。，！！", assistant: vec![], tools: vec![] };
        let fe = mechanical_extract(0, &r);
        assert!(fe.title.chars().count() <= 30, "title 截到 ≤30 char");
        assert!(!fe.title.ends_with('。') && !fe.title.ends_with('！'), "末尾标点已去");
        assert_eq!(fe.round_idx, Some(0));
        assert_eq!(fe.subject, "用户");
    }

    #[test]
    fn mechanical_extract_empty_user_text_no_assistant() {
        let r = Round { seq_a: 1, seq_b: 1, last_ts: 1000,
            user_text: "", assistant: vec![], tools: vec![] };
        let fe = mechanical_extract(0, &r);
        assert_eq!(fe.title, "（无文本）");
    }

    #[test]
    fn mechanical_extract_assistant_empty_but_has_toolcall() {
        let e = asst_tc(2, 2000);
        let r = Round { seq_a: 1, seq_b: 2, last_ts: 2000,
            user_text: "做一下", assistant: vec![&e], tools: vec![] };
        let fe = mechanical_extract(0, &r);
        assert!(fe.detail.contains("(调用工具)"), "无 content 但有 tool_calls → detail 含 (调用工具): {}", fe.detail);
    }

    #[test]
    fn mechanical_extract_unicode_safe_truncate() {
        let r = Round { seq_a: 1, seq_b: 2, last_ts: 2000,
            user_text: &"あ".repeat(50), assistant: vec![], tools: vec![] };
        let fe = mechanical_extract(0, &r);
        let _ = fe; // 不 panic（多字节字符截断安全）
    }

    #[test]
    fn tail_aware_b_zero_means_no_tail() {
        let evs = vec![user(1, 1000), user(2, 2000), user(3, 3000)];
        assert_eq!(tail_aware_b(&evs, 3, 0), 3, "tail=0 → b=cur");
    }

    #[test]
    fn tail_aware_b_skips_n_user_rounds() {
        // cur=5，最近 3 个 user（seq 3,4,5）应被跳过；第 4 个 user（seq 2）之后为 b
        let evs = vec![asst(1, 500, "x"), user(2, 1000), asst(3, 1500, "x"),
                       user(4, 2000), asst(5, 2500, "x")];
        // 注意：这里 user 是 seq 2,4 —— cur 取 evs max seq = 5
        // tail_rounds=1 → 跳过最近的 1 个 user（seq 4）→ b = 4-1 = 3
        let b = tail_aware_b(&evs, 5, 1);
        assert_eq!(b, 3, "tail=1 跳过 user seq=4，b=3（含 seq≤3）");
    }

    #[test]
    fn tail_aware_b_returns_below_a_when_user_le_tail() {
        // a=5，只有 1 个 user（seq 6），tail=3 → 跳过该 user 后 b < 5（空段）
        let evs = vec![user(6, 1000)];
        let b = tail_aware_b(&evs, 10, 3);
        assert!(b < 5, "user 数 ≤ tail → b<a（空段信号）: b={b}");
    }

    #[test]
    fn tail_aware_b_ignores_future_events() {
        // seq > cur 的事件不数（dream 期间新事件归下一段）
        let evs = vec![user(2, 1000), user(3, 2000), user(10, 9000)]; // seq 10 > cur
        let b = tail_aware_b(&evs, 5, 1);
        assert_eq!(b, 2, "seq>cur 不数；tail=1 跳 user seq=3 → b=2");
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cargo test --lib --manifest-path src-tauri/Cargo.toml dream::tests
```
Expected: 编译失败（`Round`/`split_rounds`/`FinalEvent`/`mechanical_extract`/`tail_aware_b` 未定义）。

- [ ] **Step 3: 实现 —— 插入纯逻辑层 A**

在 `src-tauri/src/dream.rs`，找到 `parse_extracts` 函数结束（`:269`）之后的空行，在 `// ─── 月主题 LLM`（`:271`）之前插入：

```rust
// ─── dream 完整性改造：回合切 + 机械底座 + 留尾（纯逻辑层 A）───

/// 一个回合：一个 user + 后续 assistant/tool_result/external/subagent_result（到下个 user 前）。
struct Round<'a> {
    seq_a: u64, seq_b: u64, last_ts: u64,
    user_text: &'a str,
    assistant: Vec<&'a HistoryEvent>,
    tools: Vec<&'a HistoryEvent>,
}

/// 按 kind=user 边界切回合。段首非 user 前导事件挂到下一个 user 回合。
fn split_rounds<'a>(segment: &'a [&HistoryEvent]) -> Vec<Round<'a>> {
    let mut rounds = Vec::new();
    let mut cur: Option<Round> = None;
    for e in segment {
        match e.kind.as_str() {
            "user" => {
                if let Some(r) = cur.take() { rounds.push(r); }
                cur = Some(Round {
                    seq_a: e.seq, seq_b: e.seq, last_ts: e.ts,
                    user_text: e.data.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                    assistant: vec![], tools: vec![],
                });
            }
            "assistant" => {
                if let Some(r) = cur.as_mut() { r.assistant.push(e); r.seq_b = e.seq; r.last_ts = e.ts; }
            }
            "tool_result" | "external" | "subagent_result" => {
                if let Some(r) = cur.as_mut() { r.tools.push(e); r.seq_b = e.seq; r.last_ts = e.ts; }
            }
            _ => {}
        }
    }
    if let Some(r) = cur { rounds.push(r); }
    rounds
}

/// 最终事件（落盘单位）。F9 温和合并：机械底座（round_idx=Some）与 reconcile 输出（None）共用此 struct。
struct FinalEvent {
    round_idx: Option<usize>,   // Some(i)=第 i 回合的机械底座（reconcile 引用）；None=已合并的最终事件
    seq_a: u64, seq_b: u64, last_ts: u64,
    title: String, detail: String, subject: String,
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Unicode 安全截断：按 char_indices 截到 max chars（不切多字节字符中段）。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max { return s.to_string(); }
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    s[..end].to_string()
}

/// 截到 max chars + 去末尾标点（。，！？、,.!?）。
fn truncate_title(s: &str, max: usize) -> String {
    let mut t = truncate_chars(s, max);
    while let Some(c) = t.chars().last() {
        if "。，！？、,.!?".contains(c) { t.pop(); } else { break; }
    }
    t
}

/// 取首句（首个句号/换行/分号前）或前 80 字。
fn truncate_first_sentence(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() { return String::new(); }
    let cut = s.find(|c: char| c == '。' || c == '\n' || c == '；' || c == '.')
        .map(|i| {
            let c_len = s[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
            i + c_len
        })
        .unwrap_or_else(|| s.char_indices().nth(80).map(|(i, _)| i).unwrap_or(s.len()));
    s[..cut].to_string()
}

fn has_tool_calls(r: &Round) -> bool {
    r.assistant.iter().any(|a| a.data.get("tool_calls").is_some())
}

/// 机械底座：从单个回合产出程序化真相（G1 完整性兜底）。LLM 漏的回合用此落盘。
fn mechanical_extract(idx: usize, r: &Round) -> FinalEvent {
    let user_clean = collapse_ws(r.user_text);
    let title = truncate_title(&user_clean, 30);
    let asst_first = r.assistant.first()
        .and_then(|a| a.data.get("content").and_then(|v| v.as_str()))
        .map(truncate_first_sentence)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| if has_tool_calls(r) { "(调用工具)".into() } else { String::new() });
    let detail = {
        let u = truncate_chars(&user_clean, 100);
        if asst_first.is_empty() { u } else { format!("{} → {}", u, truncate_chars(&asst_first, 80)) }
    };
    FinalEvent {
        round_idx: Some(idx),
        seq_a: r.seq_a, seq_b: r.seq_b, last_ts: r.last_ts,
        title: if title.is_empty() { "（无文本）".into() } else { title },
        detail: truncate_chars(&detail, 200),
        subject: "用户".into(),
    }
}

/// 留尾：从 cur 往回跳过 tail_rounds 个 user，返回应整理到的上界 b。
/// tail_rounds=0 → b=cur（不留尾）。user 数 ≤ tail → b<a（空段信号，调用方推进 marker 到 cur —— F12）。
fn tail_aware_b(events: &[HistoryEvent], cur: u64, tail_rounds: u64) -> u64 {
    if tail_rounds == 0 { return cur; }
    let mut seen = 0u64;
    let mut b = cur;
    for e in events.iter().rev() {
        if e.seq > cur { continue; } // 未来事件不数（dream 期间新事件归下一段）
        if e.kind == "user" {
            seen += 1;
            if seen > tail_rounds { break; }
        }
        b = e.seq.saturating_sub(1);
    }
    b
}
```

- [ ] **Step 4: 跑测试确认通过 + 零 warning**

```bash
cargo check --tests --manifest-path src-tauri/Cargo.toml
cargo test --lib --manifest-path src-tauri/Cargo.toml dream::tests
```
Expected: PASS（10 个新测试 + 现有 dream 测试不回归——新纯函数不影响旧 execute 路径）。零 warning（无未使用，因 mechanical_extract/tail_aware_b 暂未被 execute 调用——会有 `dead_code` warning！）。

> **注**：`mechanical_extract`/`tail_aware_b`/`FinalEvent`/`Round` 在 Task 6 接入 execute 前是 dead code。Rust 默认对私有 dead code 发 warning。**临时处理**：在 `split_rounds` 上方加 `#[allow(dead_code)]`（逐个函数加），或在 Task 4 commit message 注明「Task 6 接线后移除 allow」。**推荐**：给 `Round`/`FinalEvent`/`split_rounds`/`mechanical_extract`/`tail_aware_b`/辅助函数加 `#[allow(dead_code)]`，Task 6 接线后删除。若 `cargo check` 仍 0 warning（某些版本对 `cfg(test)` 内引用的私有项不发 dead_code），则无需加。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/dream.rs
git commit -m "feat(dream): 纯逻辑层 A——Round/split_rounds + FinalEvent/mechanical_extract + tail_aware_b" -m "回合切分（user 边界）+ 机械底座（G1 完整性兜底，Unicode 安全截断）+ 留尾边界。纯函数全单测。Task 6 接线前暂 #[allow(dead_code)]。" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 5: dream.rs 纯逻辑层 B — parse_groups + reconcile_groups（F6 连续拆 + F11 超限拆）

**背景**：LLM 输出解析 + 合并 reconciliation。强制全覆盖（漏的机械补 = G1）+ 非连续拆子组（F6，保 seq 并集 drill 干净）+ 超限强制拆（F11，守上限不丢回合）。纯函数，全单测。

**Files:**
- Modify: `src-tauri/src/dream.rs`（紧跟 Task 4 插入的 `tail_aware_b` 之后）
- Test: `src-tauri/src/dream.rs` `mod tests`

**Interfaces:**
- Consumes: `FinalEvent`（Task 4）、`Value`（serde_json）。
- Produces:
  - `struct Group { rounds: Vec<usize>, title, detail, subject }`
  - `struct ReconcileStats { filled_by_mechanical, duplicate, out_of_range, over_limit, non_contiguous_split: u32 }`
  - `fn parse_groups(content: &str) -> (Vec<Group>, u32)`（u32 = bad_lines）
  - `fn reconcile_groups(groups, mechanical: &[FinalEvent], max_rounds: u64) -> (Vec<FinalEvent>, ReconcileStats)`
  - `fn merge_event(idxs: &[usize], mechanical, title, detail, subject) -> FinalEvent`

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/dream.rs` `mod tests`，紧跟 Task 4 测试块之后加：

```rust
    // ─── Task 5: 纯逻辑层 B（parse_groups / reconcile_groups）───

    fn mech_n(n: usize) -> Vec<FinalEvent> {
        // n 个机械底座，seq_a/seq_b 各递增（回合 i → seq [i+1, i+1] 简化）
        (0..n).map(|i| FinalEvent {
            round_idx: Some(i), seq_a: (i + 1) as u64, seq_b: (i + 1) as u64, last_ts: 1000 + i as u64,
            title: format!("机械{}", i), detail: format!("d{}", i), subject: "用户".into(),
        }).collect()
    }

    #[test]
    fn parse_groups_normal_multiline() {
        let (g, bad) = parse_groups(
            "{\"rounds\":[1,2],\"title\":\"A\",\"detail\":\"d\",\"subject\":\"用户\"}\n{\"rounds\":[3],\"title\":\"B\",\"detail\":\"d\",\"subject\":\"agent\"}\n");
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].rounds, vec![1, 2]);
        assert_eq!(g[1].title, "B");
        assert_eq!(bad, 0);
    }

    #[test]
    fn parse_groups_bad_line_counted() {
        let (g, bad) = parse_groups(
            "not json\n{\"rounds\":[1],\"title\":\"ok\"}\n{\"title\":\"no rounds\"}\n```json\n{\"rounds\":[2],\"title\":\"x\"}\n```");
        assert_eq!(g.len(), 2, "只 2 行合法（rounds+title 齐全）");
        assert_eq!(bad, 3, "坏行 3（not json / no rounds / 带```包的算合法解析后去 fence）");
        // 注：带 ```json fence 的行经 trim_start_matches 后合法 → 不计 bad；按实现实算
    }

    #[test]
    fn parse_groups_strips_code_fence() {
        let (g, _) = parse_groups("```json\n{\"rounds\":[1],\"title\":\"A\"}\n```");
        assert_eq!(g.len(), 1, "代码块 fence 行被剥离后合法解析");
    }

    #[test]
    fn reconcile_full_coverage() {
        // LLM 把 3 回合分 2 组：[1,2]+[3]
        let g = vec![
            Group { rounds: vec![1, 2], title: "组A".into(), detail: "d".into(), subject: "用户".into() },
            Group { rounds: vec![3], title: "组B".into(), detail: "d".into(), subject: "agent".into() },
        ];
        let (out, st) = reconcile_groups(g, &mech_n(3), 5);
        assert_eq!(out.len(), 2);
        assert_eq!(st.filled_by_mechanical, 0, "全覆盖无漏补");
        // 组A seq 并集 [1,2]；组B seq [3,3]
        assert_eq!(out[0].seq_a, 1); assert_eq!(out[0].seq_b, 2);
        assert_eq!(out[1].seq_a, 3); assert_eq!(out[1].seq_b, 3);
        assert_eq!(out[0].title, "组A");
    }

    #[test]
    fn reconcile_partial_fills_mechanical() {
        // LLM 只覆盖回合 1，回合 2/3 漏 → 机械补
        let g = vec![Group { rounds: vec![1], title: "只有1".into(), detail: "d".into(), subject: "用户".into() }];
        let (out, st) = reconcile_groups(g, &mech_n(3), 5);
        assert_eq!(out.len(), 3);
        assert_eq!(st.filled_by_mechanical, 2, "回合 2,3 机械补");
    }

    #[test]
    fn reconcile_duplicate_round_dedup() {
        let g = vec![
            Group { rounds: vec![1, 2], title: "A".into(), detail: "d".into(), subject: "u".into() },
            Group { rounds: vec![2, 3], title: "B".into(), detail: "d".into(), subject: "u".into() }, // 2 重复
        ];
        let (out, st) = reconcile_groups(g, &mech_n(3), 5);
        assert_eq!(st.duplicate, 1, "回合 2 重复覆盖，跳过第二次");
        assert_eq!(st.filled_by_mechanical, 0, "1,2,3 都被覆盖（2 首次算）");
        assert!(out.len() >= 2);
    }

    #[test]
    fn reconcile_out_of_range_skipped() {
        let g = vec![
            Group { rounds: vec![1], title: "ok".into(), detail: "d".into(), subject: "u".into() },
            Group { rounds: vec![0, 9], title: "越界".into(), detail: "d".into(), subject: "u".into() }, // 0 和 9 越界（n=3）
        ];
        let (out, st) = reconcile_groups(g, &mech_n(3), 5);
        assert_eq!(st.out_of_range, 2, "0 和 9 都越界");
        assert_eq!(st.filled_by_mechanical, 2, "回合 2,3 漏 → 机械补");
        assert!(out.iter().any(|e| e.title == "ok"));
    }

    #[test]
    fn reconcile_over_limit_force_split() {
        // F11：单组 6 回合（max=5）→ 强制拆 [1-5]+[6]
        let g = vec![Group { rounds: vec![1, 2, 3, 4, 5, 6], title: "大组".into(), detail: "d".into(), subject: "u".into() }];
        let (out, st) = reconcile_groups(g, &mech_n(6), 5);
        assert_eq!(out.len(), 2, "6 回合 max=5 → 拆 2 个 FinalEvent");
        assert_eq!(st.over_limit, 1);
        // 第一块 [1-5] 无后缀；第二块 [6] 带 (续1)
        assert!(out.iter().any(|e| e.detail.contains("(续1)")), "第二块 detail 含 (续1): {:?}", out);
    }

    #[test]
    fn reconcile_non_contiguous_splits_subgroups() {
        // F6：rounds=[1,5,9]（n=9）非连续 → 拆 3 个连续子组，各带 LLM title
        let g = vec![Group { rounds: vec![1, 5, 9], title: "跳着选".into(), detail: "d".into(), subject: "u".into() }];
        let (out, st) = reconcile_groups(g, &mech_n(9), 5);
        // 1,5,9 各自成单回合并（相邻不连续 → 3 子组）；回合 2,3,4,6,7,8 漏 → 机械补（6 个）
        assert_eq!(st.non_contiguous_split, 2, "1→5 和 5→9 两次非连续跳");
        assert_eq!(out.len(), 9, "3 子组 + 6 机械补 = 9");
        // 每个 LLM 子组 title 是"跳着选"
        let llm_titled = out.iter().filter(|e| e.title == "跳着选").count();
        assert_eq!(llm_titled, 3, "3 个非连续子组各带 LLM title");
    }

    #[test]
    fn reconcile_seq_pointer_union_contiguous() {
        // 连续合并 [回合0(seq1),回合1(seq2)] → FinalEvent seq=[1,2]
        let g = vec![Group { rounds: vec![1, 2], title: "合并".into(), detail: "d".into(), subject: "u".into() }];
        let (out, _) = reconcile_groups(g, &mech_n(2), 5);
        assert_eq!(out[0].seq_a, 1);
        assert_eq!(out[0].seq_b, 2, "连续合并 seq 并集 [min_a, max_b]");
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cargo test --lib --manifest-path src-tauri/Cargo.toml dream::tests
```
Expected: 编译失败（`Group`/`ReconcileStats`/`parse_groups`/`reconcile_groups`/`merge_event` 未定义）。

- [ ] **Step 3: 实现 —— 插入纯逻辑层 B**

在 `src-tauri/src/dream.rs`，紧跟 Task 4 的 `tail_aware_b` 函数之后插入：

```rust
// ─── dream 完整性改造：LLM 分组解析 + reconcile（纯逻辑层 B）───

/// LLM 输出的一个分组（覆盖若干回合）。
struct Group { rounds: Vec<usize>, title: String, detail: String, subject: String }

#[derive(Default)]
struct ReconcileStats {
    filled_by_mechanical: u32,
    duplicate: u32,
    out_of_range: u32,
    over_limit: u32,
    non_contiguous_split: u32,
}

/// 解析 LLM 输出：逐行 JSON `{rounds,title,detail,subject}`；坏行计数（不静默丢）。
fn parse_groups(content: &str) -> (Vec<Group>, u32) {
    let mut groups = Vec::new();
    let mut bad_lines = 0u32;
    for line in content.lines() {
        let l = line.trim().trim_start_matches("```json").trim_start_matches("```").trim();
        if l.is_empty() || !l.starts_with('{') { continue; }
        match serde_json::from_str::<Value>(l) {
            Ok(v) => {
                let rounds = v.get("rounds").and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|n| n.as_u64().map(|n| n as usize)).collect()).unwrap_or_default();
                let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if rounds.is_empty() || title.is_empty() { bad_lines += 1; continue; }
                groups.push(Group {
                    rounds, title,
                    detail: v.get("detail").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    subject: v.get("subject").and_then(|x| x.as_str()).unwrap_or("用户").to_string(),
                });
            }
            Err(_) => { bad_lines += 1; }
        }
    }
    (groups, bad_lines)
}

/// 合并若干机械底座成一个 FinalEvent（用 LLM 的 title/detail/subject 覆盖）。
/// idxs 须升序（reconcile 内已 sort）；seq_a = 首元素，seq_b = 末元素（连续 → 并集 [min,max]）。
fn merge_event(idxs: &[usize], mechanical: &[FinalEvent], title: &str, detail: &str, subject: &str) -> FinalEvent {
    let first = *idxs.first().unwrap();
    let last = *idxs.last().unwrap();
    FinalEvent {
        round_idx: None,
        seq_a: mechanical[first].seq_a,
        seq_b: mechanical[last].seq_b,
        last_ts: mechanical[last].last_ts,
        title: title.to_string(),
        detail: detail.to_string(),
        subject: subject.to_string(),
    }
}

/// reconcile：LLM 分组 → 最终事件。强制全覆盖（漏的机械补）+ 非连续拆子组（F6）+ 超限强制拆（F11）。
fn reconcile_groups(
    groups: Vec<Group>, mechanical: &[FinalEvent], max_rounds: u64,
) -> (Vec<FinalEvent>, ReconcileStats) {
    let n = mechanical.len();
    let mut covered = vec![false; n];
    let mut out = Vec::new();
    let mut stats = ReconcileStats::default();

    for g in groups {
        // 1-based → 0-based；越界/重复标记
        let mut idxs: Vec<usize> = Vec::new();
        for r in &g.rounds {
            if *r == 0 || *r > n { stats.out_of_range += 1; continue; }
            let i = r - 1;
            if covered[i] { stats.duplicate += 1; continue; }
            covered[i] = true;
            idxs.push(i);
        }
        if idxs.is_empty() { continue; }

        // F6：非连续拆子组（idxs 升序后检查相邻是否连续）
        idxs.sort();
        let mut chunks: Vec<Vec<usize>> = vec![vec![idxs[0]]];
        for &i in &idxs[1..] {
            let last = *chunks.last().unwrap().last().unwrap();
            if i == last + 1 { chunks.last_mut().unwrap().push(i); }
            else { stats.non_contiguous_split += 1; chunks.push(vec![i]); }
        }

        for chunk in chunks {
            // F11：超限强制拆成 max_rounds 块（保留 LLM title/detail + (续K) 后缀）
            if chunk.len() as u64 > max_rounds {
                stats.over_limit += 1;
                let big = chunk.len() > max_rounds as usize;
                for (k, sub) in chunk.chunks(max_rounds as usize).enumerate() {
                    let suffix = if big { format!(" (续{})", k + 1) } else { String::new() };
                    out.push(merge_event(sub, mechanical, &g.title, &format!("{}{}", g.detail, suffix), &g.subject));
                }
            } else {
                out.push(merge_event(&chunk, mechanical, &g.title, &g.detail, &g.subject));
            }
        }
    }

    // 漏的回合 → 机械兜底（G1 完整性硬保证）
    for (i, &c) in covered.iter().enumerate() {
        if !c {
            stats.filled_by_mechanical += 1;
            let m = &mechanical[i];
            out.push(FinalEvent {
                round_idx: None, seq_a: m.seq_a, seq_b: m.seq_b, last_ts: m.last_ts,
                title: m.title.clone(), detail: m.detail.clone(), subject: m.subject.clone(),
            });
        }
    }
    out.sort_by_key(|e| e.seq_a);
    (out, stats)
}
```

- [ ] **Step 4: 跑测试确认通过 + 零 warning**

```bash
cargo check --tests --manifest-path src-tauri/Cargo.toml
cargo test --lib --manifest-path src-tauri/Cargo.toml dream::tests
```
Expected: PASS（8 个新测试 + Task 4 测试 + 现有 trigger 测试）。`reconcile_groups`/`parse_groups` 暂未被 execute 调用 → 加 `#[allow(dead_code)]`（同 Task 4，Task 6 接线后删）。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/dream.rs
git commit -m "feat(dream): 纯逻辑层 B——parse_groups + reconcile_groups（F6 连续拆 + F11 超限拆 + 漏补机械）" -m "LLM 分组解析（坏行计数）+ reconcile 强制全覆盖（G1）+ 非连续拆子组（F6 drill 干净）+ 超限强制拆（F11 守上限）。纯函数全单测。" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 6: dream.rs 集成重写 — execute_dream/prepare/run + DREAM_SYS + 迁移现有测试

**背景**：把 Task 4/5 的纯逻辑层接入 `execute_dream`；改 `prepare_dream`（只算 a，b 移入 execute）；`run_dream`/`execute_dream` 签名重写（execute 接 events 透传，返回 `Result<u64,String>` 让 dispatch 推进 frontier）；新 prompt（F13 去机械底座行）；迁移现有 7 测试（ScriptedDream shape、调用点签名、seq assert、cfg `dream_tail_rounds:0` 关留尾以隔离提取逻辑）。

**Files:**
- Modify: `src-tauri/src/dream.rs:112`（DREAM_SYS）、`:114-119`（删 DreamExtract）、`:121-135`（prepare_dream）、`:139-179`（execute_dream）、`:212-231`（run_dream）、`:234-269`（删 dream_prompt + parse_extracts，替换为 build_dream_messages）
- Test: `src-tauri/src/dream.rs:425-739`（迁移 run_dream + execute_dream 系列 7 测试）

**Interfaces:**
- `prepare_dream(trigger: &mut DreamTrigger) -> Option<u64>`（删 history 参数，返回 just `a`）
- `execute_dream(round, cfg, events: &[HistoryEvent], history, history_dir, cache, a, emit) -> Result<u64, String>`（接 events 透传；返回 `Ok(b)` = marker until_seq，Err 不推进）
- `run_dream` 签名不变（`Result<(), String>`）；内部 read_all 一次透传给 execute，`Ok(b)` 推进 frontier。

- [ ] **Step 1: 替换 DREAM_SYS + 删 DreamExtract + 加 build_dream_messages**

`src-tauri/src/dream.rs:112`，把整个 `const DREAM_SYS` 替换为（spec §5.5，F13 去机械底座行，含 `{MAX_ROUNDS}` 占位）：

```rust
const DREAM_SYS: &str = "你是 dream，负责把一段对话整理成「日」层记忆。输入按回合编号给出。请把属于同一事件的回合合并成一个分组，对每个分组输出一行 JSON：{\"rounds\":[回合编号...],\"title\":\"联想向简短标题\",\"detail\":\"一句话详情\",\"subject\":\"主语(用户/agent/子代理#N)\"}。硬性要求：①每个回合编号必须恰好被一个分组覆盖，不许漏、不许重复（回合编号 1..N 全覆盖）；②单个分组的回合数不超过 {MAX_ROUNDS}；③一个分组内的回合编号必须连续（如 [3,4,5]，不允许 [3,5]）；④标题要适合联想回忆（看到标题能想起这段对话）；⑤只输出 JSON 行，不要任何其他文字、不要 markdown 代码块。";
```

删除 `:114-119` 的 `struct DreamExtract { ... }`（整个 struct 删掉，reconcile 用 Task 4 的 `FinalEvent`）。

删除 `:234-269` 的 `fn dream_prompt` 和 `fn parse_extracts`（两个函数整删，已被 `build_dream_messages` + Task 5 `parse_groups` 取代）。

在 Task 5 的 `reconcile_groups` 之后（或 Task 4 纯逻辑层之后）加 `build_dream_messages`：

```rust
/// 构造 dream LLM 的 messages（system + 渲染回合的 user prompt）。F13：不含机械底座行。
fn build_dream_messages(rounds: &[Round], a: u64, b: u64, cfg: &Config) -> Vec<Value> {
    let max = cfg.dream_merge_max_rounds;
    let mut prompt = format!("把下面 {} 个回合（history seq[{a},{b}]）整理成事件分组：\n\n", rounds.len());
    for (i, r) in rounds.iter().enumerate() {
        let u = truncate_chars(&collapse_ws(r.user_text), 100);
        let asst = r.assistant.first()
            .and_then(|x| x.data.get("content").and_then(|v| v.as_str()))
            .map(|c| truncate_first_sentence(c))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| if has_tool_calls(r) { "(调用工具)".into() } else { "-".into() });
        prompt.push_str(&format!("回合{} [seq{}-{}] 用户：{}\n  助手：{}\n",
            i + 1, r.seq_a, r.seq_b, u, truncate_chars(&asst, 80)));
    }
    prompt.push_str(&format!(
        "\n输出 JSON 行（每行一个分组，必须覆盖所有回合编号 1..{}，单组回合数 ≤ {}，组内编号连续）。",
        rounds.len(), max));
    vec![
        serde_json::json!({"role":"system","content": DREAM_SYS.replace("{MAX_ROUNDS}", &max.to_string())}),
        serde_json::json!({"role":"user","content": prompt}),
    ]
}
```

- [ ] **Step 2: 改 prepare_dream（删 history 参数，只算 a）**

`src-tauri/src/dream.rs:121-135`，整个 `prepare_dream` 替换为：

```rust
/// 占位（同步，持锁调用）：if in_flight → None；否则 set in_flight=true + 捕获 a。
/// **不在此算 b**（F7：tail 边界移入 execute 用同源 events）；不推进 frontier（仅 execute 成功后由调用方推进）。
pub fn prepare_dream(trigger: &mut DreamTrigger) -> Option<u64> {
    if trigger.in_flight { return None; }
    let a = trigger.last_dream_marker_seq + 1;
    trigger.in_flight = true; // 单飞锁；不动 frontier；不算 b
    Some(a)
}
```

- [ ] **Step 3: 重写 execute_dream（新签名 + 接 events + 返回 Result<u64,String>）**

`src-tauri/src/dream.rs:139-179`，整个 `execute_dream` 替换为：

```rust
/// 执行（async，不持锁）：tail→段→回合→机械底座→跨月检查→LLM 增强→reconcile→落盘→marker。
/// 接 events 透传（F8，删内部 read_all）；返回 Ok(b)=marker until_seq（dispatch/run_dream 推进 frontier 用）。
/// 空段（F12）→ 写 marker(until_seq=cur) 推进；LLM 失败 catch→全机械；IO 失败 propagate（不写 marker）。
pub async fn execute_dream<E: Emitter>(
    round: Arc<dyn LlmRound>,
    cfg: &Config,
    events: &[HistoryEvent],
    history: &HistoryWriterHandle,
    _history_dir: &Path,
    cache: &Path,
    a: u64,
    emit: &E,
) -> Result<u64, String> {
    // cur = events 里 main 非 marker 的最大 seq（透传同源；不调 current_seq() 避免 await 漂移）
    let cur = events.iter()
        .filter(|e| e.thread == "main" && e.kind != "marker")
        .map(|e| e.seq).max().unwrap_or(0);
    let b = tail_aware_b(events, cur, cfg.dream_tail_rounds);

    // F12：空段（user ≤ tail 或无新事件）→ 推进 marker 到 cur，避免 idle tight-loop
    if b < a {
        let now = events.iter().filter(|e| e.seq <= cur).map(|e| e.ts).max().unwrap_or(cur);
        history.append(HistoryEvent::marker(now, "dream", cur));
        return Ok(cur);
    }

    let segment: Vec<&HistoryEvent> = events.iter()
        .filter(|e| e.seq >= a && e.seq <= b && e.thread == "main" && e.kind != "marker")
        .collect();
    if segment.is_empty() {
        return Ok(cur);
    }
    let last_ts = segment.last().expect("non-empty").ts;
    let offset = crate::history::local_offset_secs();

    let rounds = split_rounds(&segment);
    if rounds.is_empty() {
        return Ok(b);
    }
    let mechanical: Vec<FinalEvent> = rounds.iter().enumerate()
        .map(|(i, r)| mechanical_extract(i, r)).collect();

    // 跨月检查（段末月；段跨月 edge case 见 spec §11 R2）
    let seg_ym = memory::ym_from_ts(last_ts, offset);
    check_and_rotate_month(round.clone(), cfg, cache, &seg_ym, emit).await?;

    // LLM 增强（失败 catch → 全机械兜底）
    let (groups, bad_lines) = match round.round(&build_dream_messages(&rounds, a, b, cfg), cfg, emit).await {
        Ok(resp) => parse_groups(&resp.content),
        Err(e) => { eprintln!("[dream] LLM 失败，全机械兜底: {e}"); (vec![], 0) }
    };

    let (events_out, stats) = reconcile_groups(groups, &mechanical, cfg.dream_merge_max_rounds);
    eprintln!("[dream] 回合{} 组{} 坏行{} 漏补{} 重复{} 越界{} 超限{} 非连续拆{}",
        mechanical.len(), events_out.len(), bad_lines,
        stats.filled_by_mechanical, stats.duplicate, stats.out_of_range, stats.over_limit, stats.non_contiguous_split);

    // 落盘：每个 FinalEvent 用自己的 last_ts（多 ts）。F4：append_day_event 任一 Err → propagate（不写 marker）
    for ev in &events_out {
        memory::append_day_event(cache, ev.last_ts, offset, &ev.title, &ev.detail,
            &ev.subject, (ev.seq_a, ev.seq_b), None)?;
    }
    // F10：evt_no 用 day_event_count 起算（与 day 文件 evt-NNN 同源）
    for (i, ev) in events_out.iter().enumerate() {
        let base = memory::day_event_count_pub(cache, ev.last_ts, offset);
        let _ = memory::append_memory_day(cache, ev.last_ts, offset, base + i as u32 + 1, &ev.title);
    }
    // F2：distinct 日期 upsert（修复跨天漏刷）
    let mut seen_days: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for ev in &events_out {
        seen_days.insert(crate::history::date_from_ts_local(ev.last_ts, offset));
    }
    seen_days.insert(crate::history::date_from_ts_local(last_ts, offset));
    for day_str in &seen_days {
        let rep_ts = events_out.iter()
            .find(|ev| crate::history::date_from_ts_local(ev.last_ts, offset) == *day_str)
            .map(|e| e.last_ts).unwrap_or(last_ts);
        let _ = memory::upsert_current_month_today(cache, rep_ts, offset);
    }

    // 写 dream marker（until_seq = b）；F1 build_messages 认 until_seq → tail 保留
    history.append(HistoryEvent::marker(last_ts, "dream", b));
    Ok(b)
}
```

- [ ] **Step 4: 改 run_dream（透传 events + Ok(b) 推进）**

`src-tauri/src/dream.rs:212-231`，整个 `run_dream` 替换为：

```rust
/// 可测入口 = prepare + read_all + execute + (Ok(b) 才推进 frontier) + finish。
pub async fn run_dream<E: Emitter>(
    round: Arc<dyn LlmRound>,
    cfg: &Config,
    history: &HistoryWriterHandle,
    history_dir: &Path,
    cache: &Path,
    trigger: &mut DreamTrigger,
    emit: &E,
) -> Result<(), String> {
    let a = match prepare_dream(trigger) {
        Some(x) => x,
        None => return Ok(()),
    };
    let events = crate::history::read_all(history_dir); // read_all 一次透传（F8）
    let r = execute_dream(round, cfg, &events, history, history_dir, cache, a, emit).await;
    if let Ok(b) = &r {
        trigger.last_dream_marker_seq = *b; // 成功才推进（失败段保留待重试，§13.1）
    }
    trigger.dream_finished();
    r.map(|_| ())
}
```

- [ ] **Step 5: 迁移现有 7 测试**

现有测试因签名/content 变化编译失败。迁移规则（**所有** dream 集成测试适用）：

1. **ScriptedDream / DualDream 的 content（day_extracts）必须改成新 shape**：`{"rounds":[...],"title":"...","detail":"...","subject":"..."}`——加 `rounds` 字段（覆盖该测试 seed 的段内回合编号）。无 `rounds` → `parse_groups` 判空 → 全机械兜底（测试静默退化）。
2. **cfg 关留尾**：所有提取类测试用 `let cfg = Config { dream_tail_rounds: 0, ..Config::default() };`（否则默认 tail=3，seed 少量 user 会触发 F12 空段，不写 day 文件）。跨月测试（DualDream）同理。
3. **execute_dream 直接调用点（:647/668/696/730）改签名**：调用前加 `let events = crate::history::read_all(&dir.path().join("history"));`，调用改为 `execute_dream(round, &cfg, &events, &h, &dir.path().join("history"), dir.path(), 0, &Noemit).await`（删 `b` 位置参数 `1`/`2`，加 `&events`）。
4. **seq assert（:476 `seq[1,3]`）改**：新逻辑 b=events max seq（tail=0 时），seed_segment(&h,3) 写 seq 0,1,2、a=1 → 段 seq 1,2 → marker until_seq=2。改成断言 marker `until_seq==2`（而非 day 文件 seq[1,3]）。day 文件 seq 指针按 LLM 分组（单组覆盖 [1,2] → seq[1,2]）。

**代表性迁移**：`run_dream_extracts_writes_memory_and_marker`（`:457-486`）替换为：

```rust
    #[tokio::test]
    async fn run_dream_extracts_writes_memory_and_marker() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        seed_segment(&h, 3); // seq 0,1,2
        flush_dream().await;
        let mut t = DreamTrigger::new();
        t.armed = true;
        // 新 shape：1 组覆盖段内回合 [1,2]（a=1 排除 seq 0）
        let round = Arc::new(ScriptedDream { content:
            "{\"rounds\":[1,2],\"title\":\"聊了两件事\",\"detail\":\"d\",\"subject\":\"用户\"}".into(),
            fail: false });
        let cfg = Config { dream_tail_rounds: 0, ..Config::default() };
        let r = run_dream(round, &cfg, &h, &dir.path().join("history"), dir.path(), &mut t, &Noemit).await;
        assert!(r.is_ok(), "{:?}", r);
        flush_dream().await;
        let mem = std::fs::read_to_string(dir.path().join("memory").join("1970").join("01").join("1970-01-01.md")).unwrap();
        assert!(mem.contains("聊了两件事"));
        assert!(mem.contains("seq[1,2]"), "合并组 seq 并集 [1,2]: {mem}");
        let memory_md = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(memory_md.contains("聊了两件事"));
        let evs = read_all(&dir.path().join("history"));
        let m = evs.iter().find(|e| e.kind=="marker").unwrap();
        assert_eq!(m.data["marker"], serde_json::json!("dream"));
        assert_eq!(m.data["until_seq"], serde_json::json!(2), "until_seq = b = events max seq（tail=0）");
        assert!(!t.in_flight);
    }
```

**其余 6 测试的具体改动**（implementer 按此改）：

- `run_dream_failure_writes_no_marker_retries`（:488-503）：cfg 改 `dream_tail_rounds:0`；ScriptedDream content 无需 rounds（fail=true 走不到 parse_groups，但为一致加 `{"rounds":[1],"title":"x","detail":"d","subject":"z"}`）；其余 assert（无 marker、in_flight 清）不变。
- `run_dream_until_seq_is_dream_start_not_concurrent_event`（:505-532）：seed 2 user（seq 0,1），cfg tail=0 → b=1；ConcurrentRound content 改 `{"rounds":[1],"title":"t","detail":"d","subject":"用户"}`；assert `until_seq==1`（events max seq=1，dream 期间 append 的事件不在 events 快照里）。
- `run_dream_empty_segment_skips`（:534-550）：逻辑不变（a>无新事件→空段）。注意：新 execute 空段路径写 `marker(until_seq=cur)`，而旧测试期望"不新增第二个 marker"。**需调整**：新逻辑空段会写 marker 推进——但本测试 seed 一个 marker(0) 后 run_dream，a=marker.seq+1。看 `prepare_dream` a=last_dream_marker_seq+1。seed_from_history 播种 last_dream_marker_seq=该 marker 的 seq。若 marker.seq=某值，a=+1，events 里无 seq>=a → cur 可能 <a 或 segment 空 → 写 marker(until_seq=cur) 或 return Ok(cur)。**这会新增 marker**，与旧 assert `markers==1` 冲突。**改为**：assert `markers<=2` 且新 marker until_seq <= 旧（即没倒退）；或 seed 后再无任何事件时 `check` 返回 None（prepare 不触发）。实际 run_dream 不经 check（直接 prepare），prepare 总置 in_flight 并返回 a。所以空段会写 marker。**迁移决策**：把本测试改为验证"空段写 marker(until_seq=cur) 推进 frontier 且不写 day 文件"（即 F12 行为），assert day 文件不存在 + marker until_seq 合理。
- `run_dream_failure_leaves_frontier_to_retry`（:553-581）：cfg tail=0；ok_round content 改 `{"rounds":[1,2],"title":"重试成功","detail":"d","subject":"用户"}`；assert `until_seq==1`（seed 2 user seq 0,1，a=1，段 seq 1 → 1 回合，events max seq=1，b=1）。注意段只 seq 1（a=1 排除 seq 0）→ 1 回合 → rounds:[1]。
- `execute_dream_first_run_initializes_meta_no_rotate`（:639-655）：加 `let events = read_all(...)`；调用改签名（删 b=1，加 &events）；day_extracts 改 `{"rounds":[1],"title":"七月新事","detail":"d","subject":"用户"}`；cfg tail=0；assert 不变（meta=2026-07、含"七月新事"）。
- `execute_dream_same_month_upserts_today_line`（:657-677）：加 events；签名改；day_extracts 改 `{"rounds":[1],"title":"第一件",...}\n{"rounds":[2],"title":"第二件",...}`；seed 2 user（seq 0,1）a=1 → 段 seq 1（1 回合）→ 实际只 1 回合，content 应 1 组 `{"rounds":[1],...}` 或 seed 更多。**调整为**：seed 2 user 后 a=1 排除 seq 0 → 1 回合；改 content 为单组 `{"rounds":[1],"title":"第一件",...}`，assert 月节今日行含"第一件"。
- `execute_dream_cross_month_runs_theme_rotates_appends_year`（:679-713）+ `execute_dream_cross_month_theme_fail_no_rotate_no_marker`（:715-739）：加 events；签名改；day_extracts 改 `{"rounds":[1],"title":"七月新事",...}`；其余跨月 assert 不变。

- [ ] **Step 6: 移除 Task 4/5 的 `#[allow(dead_code)]`**

Task 6 接线后，`split_rounds`/`mechanical_extract`/`tail_aware_b`/`parse_groups`/`reconcile_groups`/`merge_event`/`Round`/`FinalEvent`/`Group`/`ReconcileStats`/辅助函数 全部被 execute 调用 → 删除之前加的 `#[allow(dead_code)]`（若加了）。

- [ ] **Step 7: 跑测试确认通过 + 零 warning**

```bash
cargo check --tests --manifest-path src-tauri/Cargo.toml
cargo test --lib --manifest-path src-tauri/Cargo.toml dream::tests
```
Expected: PASS（7 迁移测试 + Task 4/5 纯逻辑测试 + trigger 测试）+ 零 warning（dead_code 已移除，无未使用）。

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/dream.rs
git commit -m "feat(dream): execute_dream 重写——回合制+机械兜底+LLM增强+留尾（接入 Task4/5 纯逻辑）" -m "prepare 只算 a（b 移入 execute）；execute 接 events 透传返回 Result<u64,String>（Ok=b 推进 frontier）；新 DREAM_SYS/build_dream_messages（F13 去机械底座行）；F2 distinct-date upsert / F4 append propagate / F7 tail 同源 / F10 evt_no 同源 / F12 空段推进 marker；删旧 dream_prompt/parse_extracts/DreamExtract；迁移现有 7 测试（shape/签名/cfg tail=0）。" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 7: dream.rs 新集成测试 — 留尾端到端 / F4 / F12 / F2 / 全机械

**背景**：覆盖 spec §8.3 的新行为。**留尾端到端测试**是 F1 P0 的关键集成验证（dream 留尾 + build_messages 认 until_seq → tail 下轮可见），跨 dream.rs + context.rs。

**Files:**
- Modify: `src-tauri/src/dream.rs` `mod tests`（紧跟 Task 6 迁移后的测试块末尾加）
- 跨模块：留尾测试 import `crate::context::{build_messages, PinnedBlocks}`

**Interfaces:**
- Consumes: Task 6 的 `execute_dream`/`run_dream` 新签名 + Task 2 的 `build_messages`（认 until_seq）。

- [ ] **Step 1: 写测试**

在 `src-tauri/src/dream.rs` `mod tests` 末尾（现有 `execute_dream_cross_month_theme_fail_*` 之后）加：

```rust
    // ─── Task 7: 新行为集成测试（spec §8.3）───

    /// F1 P0 端到端：dream 留尾（tail>0）→ 最近 N 回合不进 day 文件，但 build_messages 下轮仍可见。
    #[tokio::test]
    async fn tail_rounds_kept_out_of_dream_but_visible_in_build_messages() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        for i in 0..5 { h.append(HistoryEvent::user(1000 + i * 1000, "main", &format!("m{i}"), &[])); }
        flush_dream().await;
        let mut t = DreamTrigger::new(); t.armed = true;
        // seed seq 0..4；a=1 → 段 seq 1..4（4 回合）；tail=2 → b=2（跳 seq3,4 两 user）
        let round = Arc::new(ScriptedDream { content:
            "{\"rounds\":[1,2],\"title\":\"前两回合\",\"detail\":\"d\",\"subject\":\"用户\"}".into(), fail: false });
        let cfg = Config { dream_tail_rounds: 2, ..Config::default() };
        let r = run_dream(round, &cfg, &h, &dir.path().join("history"), dir.path(), &mut t, &Noemit).await;
        assert!(r.is_ok(), "{r:?}");
        flush_dream().await;
        let evs = read_all(&dir.path().join("history"));
        let m = evs.iter().find(|e| e.kind == "marker").unwrap();
        assert_eq!(m.data["until_seq"], serde_json::json!(2), "b = cur(4) − tail(2) = 2");
        // tail 回合（m3/m4）不在 day 文件
        let mem = std::fs::read_to_string(dir.path().join("memory/1970/01/1970-01-01.md")).unwrap();
        assert!(!mem.contains("m3") && !mem.contains("m4"), "tail 回合不进 day 文件: {mem}");
        // F1 核心：build_messages 下轮仍可见 tail（m3/m4）
        let pinned = crate::context::PinnedBlocks {
            system_prompt: "S".into(), soul: None, agent: None, memory: None };
        let msgs = crate::context::build_messages(&evs, &pinned, 50);
        let contents: Vec<&str> = msgs.iter()
            .filter(|x| x["role"] == "user").filter_map(|x| x["content"].as_str()).collect();
        assert!(contents.iter().any(|c| c.contains("m3")), "tail m3 应在 build_messages 可见（F1 until_seq）: {contents:?}");
        assert!(contents.iter().any(|c| c.contains("m4")), "tail m4 应可见");
    }

    /// F4：append_day_event 失败 → propagate Err、不写 marker、frontier 不推进。
    #[tokio::test]
    async fn execute_dream_append_fail_propagates_no_marker() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        h.append(HistoryEvent::user(1000, "main", "m1", &[]));
        h.append(HistoryEvent::user(2000, "main", "m2", &[]));
        flush_dream().await;
        // 占位：cache/memory/1970 是文件 → create_dir_all(day_file_path 的 1970/01) 失败
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        std::fs::write(dir.path().join("memory").join("1970"), "占位文件").unwrap();
        let events = read_all(&dir.path().join("history"));
        let round = Arc::new(ScriptedDream { content:
            "{\"rounds\":[1],\"title\":\"x\",\"detail\":\"d\",\"subject\":\"用户\"}".into(), fail: false });
        let cfg = Config { dream_tail_rounds: 0, ..Config::default() };
        let r = execute_dream(round, &cfg, &events, &h, &dir.path().join("history"), dir.path(), 1, &Noemit).await;
        assert!(r.is_err(), "append 失败应 propagate Err（F4）: {r:?}");
        flush_dream().await;
        let evs = read_all(&dir.path().join("history"));
        assert!(evs.iter().all(|e| e.kind != "marker"), "IO 失败不写 marker（段保留重试）");
    }

    /// F12：user 数 ≤ tail → 空段 → 写 marker(until_seq=cur) 推进、不写 day 文件、无 tight-loop。
    #[tokio::test]
    async fn execute_dream_empty_segment_advances_marker_to_cur() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        h.append(HistoryEvent::user(1000, "main", "m1", &[]));
        h.append(HistoryEvent::user(2000, "main", "m2", &[]));
        flush_dream().await;
        let events = read_all(&dir.path().join("history"));
        let round = Arc::new(ScriptedDream { content: String::new(), fail: false }); // 不该被调（空段早退）
        let cfg = Config { dream_tail_rounds: 5, ..Config::default() }; // tail=5 > 2 user → 空段
        let r = execute_dream(round, &cfg, &events, &h, &dir.path().join("history"), dir.path(), 1, &Noemit).await;
        assert!(r.is_ok());
        flush_dream().await;
        let evs = read_all(&dir.path().join("history"));
        let m = evs.iter().find(|e| e.kind == "marker").unwrap();
        assert_eq!(m.data["until_seq"], serde_json::json!(2), "空段推进 marker until_seq=cur=2");
        assert!(!dir.path().join("memory/1970/01/1970-01-01.md").exists(), "空段不写 day 文件");
    }

    /// F2：段跨午夜（两天同月）→ 两天 MEMORY.md 月节今日行都刷新。
    #[tokio::test]
    async fn execute_dream_cross_day_upserts_both_today_lines() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        // TS_JUL = 2026-07-26 12:00 UTC；+86400000 = 2026-07-27（同月，offset 微调仍相邻日）
        let ts1 = TS_JUL;
        let ts2 = TS_JUL + 86_400_000;
        h.append(HistoryEvent::user(ts1, "main", "第一天的事", &[]));
        h.append(HistoryEvent::user(ts2, "main", "第二天的事", &[]));
        flush_dream().await;
        crate::memory::write_dream_meta(dir.path(), "2026-07").unwrap();
        let events = read_all(&dir.path().join("history"));
        let round = Arc::new(DualDream { theme: "不该被调用".into(),
            day_extracts: "{\"rounds\":[1],\"title\":\"第一天的事\",\"detail\":\"d\",\"subject\":\"用户\"}\n{\"rounds\":[2],\"title\":\"第二天的事\",\"detail\":\"d\",\"subject\":\"用户\"}".into(),
            fail_theme: false });
        let cfg = Config { dream_tail_rounds: 0, ..Config::default() };
        let r = execute_dream(round, &cfg, &events, &h, &dir.path().join("history"), dir.path(), 1, &Noemit).await;
        assert!(r.is_ok(), "{r:?}");
        flush_dream().await;
        let body = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        let off = crate::history::local_offset_secs();
        let d1 = crate::history::date_from_ts_local(ts1, off);
        let d2 = crate::history::date_from_ts_local(ts2, off);
        assert!(d1 != d2, "测试前提：两 ts 落不同日（否则 F2 无意义）");
        let cur = body.find("### 当月").unwrap();
        assert!(body[cur..].contains(&format!("- {d1}:")), "第一天今日行应刷新: {}", &body[cur..]);
        assert!(body[cur..].contains(&format!("- {d2}:")), "第二天今日行也应刷新（F2 distinct-date）: {}", &body[cur..]);
    }

    /// LLM 失败 → 全回合一机械条（G1 完整性 + G4 不阻塞）。
    #[tokio::test]
    async fn execute_dream_llm_fail_full_mechanical_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        for i in 0..3 { h.append(HistoryEvent::user(1000 + i * 1000, "main", &format!("msg{i}"), &[])); }
        h.append(HistoryEvent::assistant(1500, "main", "回复0", "", vec![]));
        flush_dream().await;
        let events = read_all(&dir.path().join("history"));
        let round = Arc::new(ScriptedDream { content: String::new(), fail: true });
        let cfg = Config { dream_tail_rounds: 0, ..Config::default() };
        let r = execute_dream(round, &cfg, &events, &h, &dir.path().join("history"), dir.path(), 1, &Noemit).await;
        assert!(r.is_ok(), "LLM 失败应兜底成功（G4）: {r:?}");
        flush_dream().await;
        // a=1 → 段 seq 1,2（2 user 回合）→ 全机械 → 2 段
        let mem = std::fs::read_to_string(dir.path().join("memory/1970/01/1970-01-01.md")).unwrap();
        let seg_count = mem.lines().filter(|l| l.starts_with("## ")).count();
        assert_eq!(seg_count, 2, "LLM 失败 → 每回合一机械条（2 回合 → 2 段）: {mem}");
        let evs = read_all(&dir.path().join("history"));
        assert!(evs.iter().any(|e| e.kind == "marker"), "兜底成功仍写 marker");
    }
```

- [ ] **Step 2: 跑测试确认通过**

```bash
cargo check --tests --manifest-path src-tauri/Cargo.toml
cargo test --lib --manifest-path src-tauri/Cargo.toml dream::tests
```
Expected: PASS（5 个新测试）。若 `tail_rounds_kept_out_of_dream_but_visible_in_build_messages` FAIL → F1（Task 2）未生效或 execute b 计算错——回头查。

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/dream.rs
git commit -m "test(dream): 新行为集成测——留尾端到端(F1 P0)/F4 propagate/F12 空段/F2 跨天/全机械兜底" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 8: agent.rs dispatch_dream F8 — events 单次 read_all 透传

**背景**：dispatch_dream 决策阶段 `read_all` 一次，events 全程透传给 execute（删 execute 内部 read_all，spec F8）；prepare 改返回 `Option<u64>`（删 history 参数）；execute 返回 `Result<u64,String>` → spawn 内 `Ok(b)` 推进 frontier。

**Files:**
- Modify: `src-tauri/src/agent.rs:301-346`（dispatch_dream）
- Test: 靠 `cargo check` + 现有 driver 集成测试不回归（dispatch_dream 是 private async fn，spawn tauri runtime，单测代价高；验证靠编译 + Task 6/7 的 execute/run_dream 测试已覆盖核心逻辑 + 手测）。

**Interfaces:**
- Consumes: Task 6 的 `prepare_dream(&mut DreamTrigger) -> Option<u64>` + `execute_dream(..., events: &[HistoryEvent], ..., a) -> Result<u64, String>`。

- [ ] **Step 1: 重写 dispatch_dream**

`src-tauri/src/agent.rs:301-346`，整个 `dispatch_dream` 替换为：

```rust
async fn dispatch_dream(
    trigger: &std::sync::Arc<std::sync::Mutex<crate::dream::DreamTrigger>>,
    history: &crate::history::HistoryWriterHandle,
    history_dir: &std::sync::Arc<std::path::PathBuf>,
    cache: &std::sync::Arc<std::path::PathBuf>,
    cfg: &std::sync::Arc<Config>,
) {
    // 1. 决策（持锁）：check 自带 armed/in_flight 门。read_all 不需锁（read-only）。F8：events 全程透传。
    let now = now_ms();
    let evs = crate::history::read_all(history_dir);
    let decision = {
        let t = trigger.lock().unwrap();
        t.check(&evs, now, cfg.dream_idle_secs, cfg.dream_cap_turns)
    };
    if decision.is_none() {
        return;
    }
    // 2. 占位（持锁）：set in_flight + 捕获 a，**不推进 frontier**、不算 b（F7：b 在 execute 内算）。
    let prepared = {
        let mut t = trigger.lock().unwrap();
        crate::dream::prepare_dream(&mut t)
    };
    let Some(a) = prepared else { return; };
    // 3. spawn 执行（不持锁跨 await）；Ok(b) 才推进 frontier，再 finish。
    let trigger2 = trigger.clone();
    let history2 = history.clone();
    let hd2 = history_dir.as_ref().to_path_buf();
    let cache2 = cache.as_ref().to_path_buf();
    let cfg2 = (**cfg).clone();
    tauri::async_runtime::spawn(async move {
        let round: std::sync::Arc<dyn llm::LlmRound> = std::sync::Arc::new(crate::llm::HttpRound);
        let emit = DreamSilentEmitter; // §7.7 静默
        let r = crate::dream::execute_dream(round, &cfg2, &evs, &history2, &hd2, &cache2, a, &emit).await;
        {
            let mut t = trigger2.lock().unwrap();
            if let Ok(b) = r {
                t.last_dream_marker_seq = b; // 成功才推进（失败段保留待重试，§13.1）
            }
            t.dream_finished();
        }
        if let Err(e) = r {
            eprintln!("[dream] 失败（不推进 marker，下次 idle/cap 自动重试）: {e}");
        }
    });
}
```

> 关键变化：①`evs` 提到持锁块外（单次 read_all，move 进 spawn）；②`prepare_dream(&mut t)` 删 history 参数；③解构 `let Some(a) = prepared`（删 b）；④`execute_dream(..., &evs, ..., a, ...)` 接 events + 删 b 位置参数；⑤`if let Ok(b) = r` 用返回的 b 推进 frontier。

- [ ] **Step 2: 跑编译 + 全测试不回归**

```bash
cargo check --tests --manifest-path src-tauri/Cargo.toml
cargo test --lib --manifest-path src-tauri/Cargo.toml
```
Expected: PASS（编译过；dream/context/memory/config 测试全绿；agent 现有测试不回归）。零 warning（删 execute 内部 read_all 后，dream.rs 顶部的 `use crate::history::read_all` 若仅 run_dream 用则保留，确认无 unused import）。

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/agent.rs src-tauri/src/dream.rs
git commit -m "feat(agent): dispatch_dream F8——events 单次 read_all 全程透传 + Ok(b) 推进 frontier" -m "决策阶段 read_all 一次，evs move 进 spawn 透传 execute（删 execute 内部 read_all）；prepare 删 history 参数；execute 返回 Result<u64,String>，spawn 用 Ok(b) 推进 frontier（失败段保留重试）。" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 9: 文档同步 — dream-flow.md + AGENT.md

**背景**：`docs/dream-flow.md` 是逐字抄源码（标 `file:line`）的 dream 流程参考文档（commit `bf37d6a` 进 master，基于 `cec02c7`）。Task 1-8 改了 DREAM_SYS/dream_prompt→build_dream_messages/execute_dream 全流程，dream-flow.md 必须同步否则误导。AGENT.md 补一句记忆边界（让 LLM 理解"刚说的话 MEMORY.md 可能还没 = 留尾"）。文档无 TDD。

**Files:**
- Modify: `docs/dream-flow.md`（流程图段 + DREAM_SYS 原文段 + dream_prompt→build_dream_messages 段 + 重锚 file:line）
- Modify: `src-tauri/defaults/AGENT.md:75-78`（"记忆深思（mem）"段补记忆边界一句）

- [ ] **Step 1: 更新 docs/dream-flow.md**

读 `docs/dream-flow.md` 全文，按 Task 1-8 后的源码更新：
- **流程图段**：在原"段→LLM→落盘"基础上补「①tail_aware_b 留尾 ②split_rounds 回合切 ③mechanical_extract 机械底座（每回合必出）④LLM 分组（受 max_rounds + 连续约束）⑤reconcile（漏补机械 + 非连续拆 + 超限拆）⑥per-event 多 ts 落盘 + distinct-date upsert ⑦marker(until_seq=b)」。补"空段（F12）→ marker(until_seq=cur) 推进"分支。
- **DREAM_SYS 原文段**：换成 Task 6 新 DREAM_SYS（含 `{MAX_ROUNDS}` 占位 + ③ 连续要求 + 全覆盖要求），重锚 `dream.rs:<新行号>`。
- **dream_prompt 段**：改成 `build_dream_messages` 描述（system + 渲染回合的 user prompt，F13 去机械底座行），重锚行号。
- **所有 `file:line` 重锚**：Task 1-8 改动后行号漂移；以函数名为准重新核对（prepare_dream/execute_dream/run_dream/tail_aware_b/split_rounds/mechanical_extract/parse_groups/reconcile_groups/build_dream_messages）。
- 顶部"基于 master cec02c7"改成本 plan 完成后的 commit（或"基于 feat/dream-completeness HEAD"）。

- [ ] **Step 2: 更新 AGENT.md 记忆边界**

`src-tauri/defaults/AGENT.md:75-78` "记忆深思（mem）" 段，在 MEMORY.md 描述后补一句（让 LLM 理解记忆边界，避免困惑"刚说的话 MEMORY.md 怎么没有"）：

```markdown
**记忆边界**：dream 后台每段覆盖段内**每个回合**（每回合至少 1 条记忆，程序化兜底不丢）；但**最近 3 个回合留尾**（dream_tail_rounds）不进记忆——这是为了让当前进行中的对话在下轮仍以原文可见。所以"刚说的话 MEMORY.md 可能还没"是正常的（还在留尾窗口内），不是记忆丢失。
```

- [ ] **Step 3: 编译确认（文档改不影响编译，跑一次防误碰代码）**

```bash
cargo check --tests --manifest-path src-tauri/Cargo.toml
```
Expected: 零 warning（纯文档改动不应影响编译）。

- [ ] **Step 4: Commit**

```bash
git add docs/dream-flow.md src-tauri/defaults/AGENT.md
git commit -m "docs: dream-flow 同步新流程（回合制+机械兜底+reconcile+留尾）+ AGENT.md 记忆边界" -m "Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Self-Review（plan 作者自检）

### 1. Spec coverage（13 findings → task 映射）

| Finding | 严重 | 覆盖 task | 状态 |
|---|---|---|---|
| F1 build_messages 认 until_seq | P0 | Task 2（改 + 3 回归测试）+ Task 7（留尾端到端） | ✓ |
| F2 distinct-date upsert | P1 | Task 6（execute 内）+ Task 7（跨天测） | ✓ |
| F3 段跨月 rotate 顺序 | P1 | — | **deferred**（见下） |
| F4 append 错误 propagate | P1 | Task 6（execute `?`）+ Task 7（测） | ✓ |
| F5 测试迁移 | P1 | Task 6（ScriptedDream shape + 调用点 + assert + cfg tail=0） | ✓ |
| F6 非连续拆子组 | P1 | Task 5（reconcile）+ Task 5 测 | ✓ |
| F7 tail 移入 execute | P1 | Task 4（tail_aware_b）+ Task 6（execute 内调） | ✓ |
| F8 events 透传 | P2 | Task 6（execute 接 events）+ Task 8（dispatch 透传） | ✓ |
| F9 struct 温和合并 | P2 | Task 4（FinalEvent round_idx） | ✓ |
| F10 evt_no 同源 | P2 | Task 3（day_event_count_pub）+ Task 6（用） | ✓ |
| F11 超限强制拆 | P2 | Task 5（reconcile）+ Task 5 测 | ✓ |
| F12 空段推进 marker | P2 | Task 6（execute）+ Task 7（测） | ✓ |
| F13 prompt 瘦身 | P2 | Task 6（新 DREAM_SYS/build_dream_messages 去机械底座行） | ✓ |

**F3 deferred 说明**：spec §11 R2 明确允许「plan 先加跨月检测 + 日志，实测频次后再决定是否分组判」。本 plan Task 6 execute 保持现状 rotate 顺序（基于段末月，在 append 前），段跨月 edge case（上月 ts 事件 append 到已冻结上月）未单独实现。**理由**：cap=50 + 单次 dream 几秒，段跨月极罕见（需 dream 跨午夜且段内事件横跨两月）。plan 不为罕见 edge case 增复杂度，留作实测后增强。**这是 plan 对 spec §11 R2 推荐的遵循，非覆盖缺口。**

### 2. Placeholder scan

- 无 TBD/TODO 占位。Task 4 的 `#[allow(dead_code)]` 是临时手段（Task 6 接线后删），已明确说明。
- Task 6 测试迁移用「规则 + 代表性完整代码 + 其余 bullet」——bullet 给了每个测试的具体改动（content/assert/cfg），非模糊指示。
- Task 9 文档更新用「读源码 + 重锚行号」指导（dream-flow.md 逐字抄源码性质决定，无法在 plan 里预抄未来行号）。

### 3. Type consistency（跨 task 签名一致）

- `prepare_dream`: Task 2 定义 `Option<u64>` → Task 6 run_dream 用 → Task 8 dispatch 用 ✓
- `execute_dream`: Task 6 定义 `Result<u64,String>` + `events: &[HistoryEvent]` 参数 → Task 7 测试用 → Task 8 dispatch 用 ✓
- `FinalEvent`: Task 4 定义（`round_idx: Option<usize>`）→ Task 5 reconcile 用 → Task 6 execute 用 ✓
- `tail_aware_b`/`split_rounds`/`mechanical_extract`: Task 4 定义 → Task 6 execute 调用 ✓
- `parse_groups`/`reconcile_groups`/`merge_event`: Task 5 定义 → Task 6 execute 调用 ✓
- `day_event_count_pub`: Task 3 定义 `(cache, ts, offset) -> u32` → Task 6 execute 调用 ✓
- `Config.dream_tail_rounds`/`dream_merge_max_rounds`: Task 1 定义 → Task 4（build_dream_messages 用 max）/Task 6（execute 用 tail + max）✓

### 4. 关键风险点（implementer 注意）

- **Task 6 是最大风险**：execute 重写 + 迁移 7 测试，签名变化连锁（prepare/execute/run/dispatch）。务必按 Step 1-8 顺序，每步 `cargo check`。
- **Task 6 Step 5 测试迁移**：`run_dream_empty_segment_skips` 行为反转（旧=空段不写 marker；新=空段写 marker 推进 F12）。按 Step 5 该测试的迁移决策改。
- **Task 7 留尾测试**：跨 context.rs + dream.rs，是 F1 P0 端到端验证。若 FAIL 说明 F1（Task 2）或 execute b 计算（Task 6）有误。
- **dev server 锁 exe**：全程只用 `cargo check --tests` + `cargo test --lib`，不 build/run。

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-30-dream-completeness.md`.

**用户已选 Subagent-Driven 执行** → REQUIRED SUB-SKILL: superpowers:subagent-driven-development。Fresh subagent per task（Task 1→9 串行）+ task review（spec compliance + code quality）+ final whole-branch review。

**Task 依赖顺序**：1（config）→ 2（context F1）→ 3（memory pub）→ 4（纯逻辑 A）→ 5（纯逻辑 B）→ 6（execute 重写，最大）→ 7（新测试）→ 8（dispatch 接线）→ 9（文档）。Task 1/2/3 互相独立但串行执行避免冲突。

**模型选择建议**（subagent-driven-development）：
- Task 1/3（机械小改，complete code 在 plan）：cheap model
- Task 2（F1 P0 + 测试，主路径）：standard model
- Task 4/5（纯逻辑，complete code 在 plan，但要核对 Unicode/边界）：standard model
- Task 6（execute 重写 + 迁移 7 测试，集成核心）：standard-capable model
- Task 7（新测试，逻辑清晰）：standard model
- Task 8（dispatch 接线，锁模式）：standard model
- Task 9（文档）：cheap model
- Final whole-branch review：most capable model
