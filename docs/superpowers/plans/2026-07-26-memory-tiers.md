# 记忆分层修订（日 / 月 / 年，无周）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把现有「日」层 dream 扩成日 / 月 / 年三层记忆：dream 一次 run 内，日提取后 upsert 月层当月今日行（机械汇总当天事件段 title），写 marker 前做跨月检查（跨则 LLM 综合刚完成月全部日概括成月主题 → 追加年层 + 月层轮转）；MEMORY.md 骨架四层 → 三层（去周）；新增只读 `mem index [层]`。

**Architecture:** 全代码规则、无 chrono。dream 仍是 `memory/` 唯一写者。LLM 只在两处产语义内容：日提取（已有 `execute_dream`）+ 月主题（新）。跨月判定 / 月层轮转 / 年层装配 / 当月今日行汇总——全确定性代码。状态记在 `cache/memory/.dream-meta.json` 的 `current_month`（dream 读写、用户不碰）。MEMORY.md 月节用 `### 当月` / `### 上月` 子标题承载 ≤ 2 月滚动窗口；当月今日行 per-day 汇总（区别于「日」节的 per-event 细粒度）。

**Tech Stack:** Rust（async/await + tokio + serde_json），Tauri 2，无 chrono（本地时间走现有 `history::date_from_ts_local(ts, offset_secs)`，offset 源 `windows-sys GetTimeZoneInformation`）。

## Global Constraints

- **无 chrono**：跨月 = `YYYY-MM` 字符串字典序比对（`seg_ym > cur`）；日期 / 月份由现有 `crate::history::date_from_ts_local(ts, offset_secs)`（返回 `YYYY-MM-DD`，补零）派生。绝不引入 chrono 依赖。
- **dream 是 `memory/` 唯一写者**：mem CLI 只读；`MEMORY.md` / `memory/{Y}/{M}/{date}.md` / `memory/.dream-meta.json` 全由 dream 写。无并发写者，零竞态。
- **结构规则全代码，LLM 只产内容**：跨月判定 / 月层轮转 / 年层装配 / 当月今日行汇总 = 确定性代码；LLM 只在日提取（已有）+ 月主题（新）产语义文本。
- **P1 铁律延续**（本次不改语义，不得破坏）：①单写不变量（dream 单飞，已有 `DreamTrigger.in_flight`）；③seq 代码盖戳（`append_day_event` 不动）；④cap 只数 `kind=user`；marker(dream|reset) 重建边界（`dream` marker 仍 `until_seq=b`，跨月不改 marker 语义）。
- **不新增 agent retrieval 工具**：`mem index` 是 CLI 命令（agent 经 bash 调），不是 agent 工具 → `llm.rs` `tools.len()` 断言 + `tools.rs` schemas 名字表**不动**（[[ovoice-tool-count-cascade]] 不触发）。
- **cache_dir 内运作**：`MEMORY.md` / `memory/` / `.dream-meta.json` 都在 cache_dir（`618b20c` 已分离 workspace/cache）；不动 workspace。
- **dev server 占 exe**：验证用 `cargo check --tests --manifest-path src-tauri/Cargo.toml` / `cargo test --lib --manifest-path src-tauri/Cargo.toml`，不要 `cargo build`；改 mem 源码后单独 `cargo build --bin mem --manifest-path src-tauri/Cargo.toml`（[[ovoice-dev-server-cargo-lock]]）。
- **绿色 / 便携**：无 installer / PATH / registry；config + 二进制 next-to-exe。
- **测试范式**：后端纯逻辑用 tempfile + Scripted/Dual LlmRound + Noemit Emitter（参照 `dream.rs` 现有 tests、`memory.rs` tests）；前端无 JS 测试框架（本次不动前端）。
- **commit message 末尾必加** `Co-Authored-By: Claude <noreply@anthropic.com>`。

## File Structure

- **`src-tauri/src/memory.rs`**（改 + 扩）：改 `ensure_memory_skeleton`（三层 + 旧四层迁移）；新增 `read_dream_meta` / `write_dream_meta` / `ym_from_ts` / `day_titles` / `upsert_current_month_today` / `rotate_month` / `append_year_theme` / `collect_month_segments` + 私有解析助手。单一职责：MEMORY.md + day 文件 + dream meta 数据访问。
- **`src-tauri/src/dream.rs`**（改 + 扩）：新增 `MONTH_THEME_SYS` / `month_theme_prompt` / `build_month_theme` / `check_and_rotate_month`；改 `execute_dream`（接入跨月检查 + 月层今日行 upsert）。dream 编排逻辑。
- **`src-tauri/src/mem_cli.rs`**（扩）：新增 `index(workspace, tier)` + 私有 `parse_sections`。只读 CLI。
- **`src-tauri/src/bin/mem.rs`**（扩）：dispatch 加 `"index"` 臂 + usage 加一行。

**任务依赖**：Task 1（骨架 + meta 基础设施）→ Task 2（月层当月今日行）+ Task 3（轮转 + 年层）均依赖 Task 1；Task 4（月主题 LLM）依赖 Task 3 的 `collect_month_segments`；Task 5（execute_dream 集成）依赖 1-4；Task 6（mem index）独立，只读 MEMORY.md，可任意时刻做。

---

### Task 1: MEMORY.md 三层骨架（去周 + 旧四层迁移）+ `.dream-meta.json` current_month 读写

**Files:**
- Modify: `src-tauri/src/memory.rs`（`ensure_memory_skeleton` 改写 + 新增 `migrate_to_three_tiers` / `read_dream_meta` / `write_dream_meta` / `ym_from_ts`）
- Test: `src-tauri/src/memory.rs` 的 `mod tests`（改现有 `ensure_memory_skeleton_creates_four_tiers` → 三层；新增 meta + 迁移 + ym 测试）

**Interfaces:**
- Consumes: `crate::history::date_from_ts_local(ts, offset_secs) -> String`（现有，返回 `YYYY-MM-DD`）。
- Produces:
  - `pub fn ensure_memory_skeleton(cache: &Path) -> std::io::Result<()>`（签名不变；行为变：建三层 + 旧四层迁移）
  - `pub fn read_dream_meta(cache: &Path) -> Option<String>`（读 `memory/.dream-meta.json` 的 `current_month`）
  - `pub fn write_dream_meta(cache: &Path, ym: &str) -> Result<(), String>`
  - `pub fn ym_from_ts(ts: u64, offset_secs: i64) -> String`（返回 `YYYY-MM`）

- [ ] **Step 1: 写失败测试**（追加到 `memory.rs` 的 `mod tests`；并改现有 `ensure_memory_skeleton_creates_four_tiers`）

把现有 `ensure_memory_skeleton_creates_four_tiers` 改名为 `ensure_memory_skeleton_creates_three_tiers_no_week`：

```rust
    #[test]
    fn ensure_memory_skeleton_creates_three_tiers_no_week() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert!(body.contains("## 日"), "含日节: {body}");
        assert!(body.contains("## 月"), "含月节: {body}");
        assert!(body.contains("## 年"), "含年节: {body}");
        assert!(!body.contains("## 周"), "去周: {body}");
        assert!(body.contains("### 当月"), "月节带当月子标题: {body}");
        assert!(body.contains("### 上月"), "月节带上月子标题: {body}");
    }

    #[test]
    fn ensure_memory_skeleton_migrates_old_four_tier() {
        // 旧四层 MEMORY.md（含 ## 周，月节无子标题）→ 迁移：删周 + 补当月/上月子标题
        let w = ws();
        let old = "# 记忆索引\n\n## 日\n- 旧日行\n\n## 周\n- 旧周行\n\n## 月\n- 旧月行\n\n## 年\n";
        std::fs::write(w.path().join("MEMORY.md"), old).unwrap();
        ensure_memory_skeleton(w.path()).unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert!(!body.contains("## 周"), "迁移后无周节: {body}");
        assert!(!body.contains("旧周行"), "周节内容被删: {body}");
        assert!(body.contains("### 当月"), "补上当月子标题: {body}");
        assert!(body.contains("### 上月"), "补上上月子标题: {body}");
        assert!(body.contains("旧月行"), "月节原内容保留: {body}");
    }

    #[test]
    fn ensure_memory_skeleton_idempotent_three_tier() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        let once = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        ensure_memory_skeleton(w.path()).unwrap(); // 再调不动
        let twice = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert_eq!(once, twice, "已是三层应幂等不动");
    }

    #[test]
    fn read_dream_meta_none_when_absent() {
        let w = ws();
        assert_eq!(read_dream_meta(w.path()), None);
    }

    #[test]
    fn write_then_read_dream_meta_roundtrip() {
        let w = ws();
        write_dream_meta(w.path(), "2026-07").unwrap();
        assert_eq!(read_dream_meta(w.path()), Some("2026-07".to_string()));
        // 文件落在 memory/.dream-meta.json
        assert!(w.path().join("memory").join(".dream-meta.json").exists());
    }

    #[test]
    fn ym_from_ts_returns_yyyy_mm() {
        // ts_on(2026-07-26) + offset=0 → date 2026-07-26 → ym 2026-07
        assert_eq!(ym_from_ts(ts_on("2026-07-26"), 0), "2026-07");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml memory::`
Expected: FAIL —— `ensure_memory_skeleton_creates_four_tiers` 已改名编译失败（找不到旧测试），新增的 `read_dream_meta` / `write_dream_meta` / `ym_from_ts` / 迁移测试因函数未定义而失败。

- [ ] **Step 3: 实现**（追加 / 改写到 `memory.rs`）

改 `ensure_memory_skeleton`（替换现有四层版本）+ 新增函数：

```rust
pub fn ensure_memory_skeleton(cache: &Path) -> std::io::Result<()> {
    let path = cache.join("MEMORY.md");
    if !path.exists() {
        let skeleton = r#"# 记忆索引（MEMORY.md）

由 dream 自动维护。三层：日（今天 per-event）/月（当月逐日 + 上月冻结）/年（每月一段月主题）。

## 日

## 月
### 当月

### 上月

## 年
"#;
        return std::fs::write(&path, skeleton);
    }
    // 迁移：旧四层（含 ## 周）或月节缺子标题 → 升级为三层
    let text = std::fs::read_to_string(&path)?;
    if text.contains("\n## 周") || !text.contains("### 当月") {
        let migrated = migrate_to_three_tiers(&text);
        std::fs::write(&path, migrated)?;
    }
    Ok(())
}

fn migrate_to_three_tiers(text: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    // 删 ## 周 节（从 ## 周 到下一个 ## 之前）
    if let Some(start) = lines.iter().position(|l| l.trim_start() == "## 周") {
        let mut end = start + 1;
        while end < lines.len() && !lines[end].trim_start().starts_with("## ") { end += 1; }
        lines.drain(start..end);
    }
    // 确保月节有 ### 当月 / ### 上月 子标题
    if let Some(mstart) = lines.iter().position(|l| l.trim_start() == "## 月") {
        let mut mend = mstart + 1;
        while mend < lines.len() && !lines[mend].trim_start().starts_with("## ") { mend += 1; }
        let has_current = (mstart + 1..mend).any(|i| lines[i].trim_start().starts_with("### 当月"));
        if !has_current {
            lines.insert(mstart + 1, "### 上月".to_string());
            lines.insert(mstart + 1, "### 当月".to_string());
        }
    }
    lines.join("\n")
}

pub fn read_dream_meta(cache: &Path) -> Option<String> {
    let text = std::fs::read_to_string(cache.join("memory").join(".dream-meta.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("current_month").and_then(|x| x.as_str()).map(|s| s.to_string())
}

pub fn write_dream_meta(cache: &Path, ym: &str) -> Result<(), String> {
    let dir = cache.join("memory");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let body = format!(r#"{{"current_month":"{}"}}"#, ym);
    std::fs::write(dir.join(".dream-meta.json"), body).map_err(|e| e.to_string())
}

pub fn ym_from_ts(ts: u64, offset_secs: i64) -> String {
    date_from_ts_local(ts, offset_secs)[..7].to_string()
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml memory::`
Expected: PASS（含改名后的三层测试 + 迁移 + meta + ym 全绿）。

- [ ] **Step 5: 全量 check**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml`
Expected: 零 error / 零 warning。`lib.rs:403` 的 `ensure_memory_skeleton(cache)` 调用点签名未变 → 不破。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/memory.rs
git commit -m "feat(memory): 三层骨架（去周 + 旧四层迁移）+ .dream-meta.json current_month 读写

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: 月层当月「今日行」upsert（机械汇总当天事件段 title）

**Files:**
- Modify: `src-tauri/src/memory.rs`（新增 `day_titles` / `upsert_current_month_today` / 私有 `upsert_line_in_subsection`）
- Test: `src-tauri/src/memory.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `ensure_memory_skeleton`（提供 `### 当月` 子标题锚点）；现有 `day_file_path` / `date_from_ts_local` / `insert_in_section`。
- Produces: `pub fn upsert_current_month_today(cache: &Path, ts: u64, offset_secs: i64) -> Result<(), String>`

**说明**：「日」节是 per-event（每个事件段一行，由现有 `append_memory_day` 写）；「月」节当月今日行是 per-day 汇总（当天所有事件段 title 用「；」连成一行），随当天 dream 累积 upsert（同一天多次 dream → 替换今日行，不重复）。

- [ ] **Step 1: 写失败测试**（追加到 `memory.rs` 的 `mod tests`）

```rust
    fn write_day_with_segments(ws_dir: &std::path::Path, date: &str, titles: &[&str]) {
        // date="2026-07-26" → memory/2026/07/2026-07-26.md，每标题写一段
        let (y, m) = (date.split('-').next().unwrap(), date.split('-').nth(1).unwrap());
        let p = ws_dir.join("memory").join(y).join(m).join(format!("{date}.md"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut body = String::new();
        for (i, t) in titles.iter().enumerate() {
            body.push_str(&format!("## 12:0{i} evt-{}-{i:03} {t}\n**详情**: d\n**主语**: 用户\n", date.replace('-', "")));
        }
        std::fs::write(&p, body).unwrap();
    }

    #[test]
    fn day_titles_reads_all_segment_titles() {
        let w = ws();
        write_day_with_segments(w.path(), "2026-07-26", &["第一件", "第二件"]);
        let titles = day_titles(w.path(), ts_on("2026-07-26"), 0);
        assert_eq!(titles, vec!["第一件".to_string(), "第二件".to_string()]);
    }

    #[test]
    fn upsert_creates_today_line_when_absent() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        write_day_with_segments(w.path(), "2026-07-26", &["重构完成", "聊了三件事"]);
        upsert_current_month_today(w.path(), ts_on("2026-07-26"), 0).unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        let cur_start = body.find("### 当月").unwrap();
        let cur_section = &body[cur_start..];
        assert!(cur_section.contains("- 2026-07-26: 重构完成；聊了三件事"),
            "当月今日行应汇总当天 title: {cur_section}");
    }

    #[test]
    fn upsert_replaces_today_line_on_second_dream_same_day() {
        // 同一天 dream 两次（第二次多了事件段）→ 今日行更新，不重复
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        write_day_with_segments(w.path(), "2026-07-26", &["A"]);
        upsert_current_month_today(w.path(), ts_on("2026-07-26"), 0).unwrap();
        // 第二次 dream 又写了一段
        write_day_with_segments(w.path(), "2026-07-26", &["A", "B"]);
        upsert_current_month_today(w.path(), ts_on("2026-07-26"), 0).unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        let count = body.matches("- 2026-07-26:").count();
        assert_eq!(count, 1, "今日行只一行（upsert 替换不重复）: {body}");
        assert!(body.contains("- 2026-07-26: A；B"), "今日行含最新汇总: {body}");
    }

    #[test]
    fn upsert_does_not_touch_other_sections() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        // 先在日节 / 年节放内容
        let mut text = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        text = text.replace("## 日\n\n## 月", "## 日\n- 不该被动\n\n## 月");
        text = text.replace("## 年\n", "## 年\n- 2026-01: 旧年主题\n");
        std::fs::write(w.path().join("MEMORY.md"), text).unwrap();
        write_day_with_segments(w.path(), "2026-07-26", &["X"]);
        upsert_current_month_today(w.path(), ts_on("2026-07-26"), 0).unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert!(body.contains("- 不该被动"), "日节不动: {body}");
        assert!(body.contains("- 2026-01: 旧年主题"), "年节不动: {body}");
        assert!(body.contains("- 2026-07-26: X"), "当月今日行写入: {body}");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml memory::tests::day_titles memory::tests::upsert`
Expected: FAIL —— `day_titles` / `upsert_current_month_today` 未定义。

- [ ] **Step 3: 实现**（追加到 `memory.rs`）

```rust
/// 读当天 day 文件所有事件段的 title（去 `## HH:MM evt-NNN ` 前缀）。
pub fn day_titles(cache: &Path, ts: u64, offset_secs: i64) -> Vec<String> {
    let path = day_file_path(cache, ts, offset_secs);
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    body.lines().filter_map(|l| {
        let rest = l.strip_prefix("## ")?;
        // "HH:MM evt-NNN title..." → splitn 3 取第三段（title，可含空格）
        let mut it = rest.splitn(3, ' ');
        it.next();
        it.next();
        it.next().map(|t| t.to_string())
    }).collect()
}

/// 重算当月「今日行」（汇总当天所有事件段 title），在 MEMORY.md 月节「### 当月」子节里 upsert。
pub fn upsert_current_month_today(cache: &Path, ts: u64, offset_secs: i64) -> Result<(), String> {
    let path = cache.join("MEMORY.md");
    if !path.exists() { ensure_memory_skeleton(cache).map_err(|e| e.to_string())?; }
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let today = date_from_ts_local(ts, offset_secs); // YYYY-MM-DD
    let titles = day_titles(cache, ts, offset_secs);
    let summary = if titles.is_empty() { String::from("（无事件）") } else { titles.join("；") };
    let new_line = format!("- {today}: {summary}");
    let new_text = upsert_line_in_subsection(&text, "月", "当月", &today, &new_line);
    std::fs::write(&path, new_text).map_err(|e| e.to_string())
}

/// 在 `## {section}` 节的 `### {subsection}` 子节里，按 key（行前缀 `- {key}`）upsert 一行。
fn upsert_line_in_subsection(text: &str, section: &str, subsection: &str, key: &str, new_line: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let sec_header = format!("## {section}");
    let sec_start = match lines.iter().position(|l| l.trim_start() == sec_header) {
        Some(i) => i + 1,
        None => return lines.join("\n"),
    };
    let mut sec_end = sec_start;
    while sec_end < lines.len() && !lines[sec_end].trim_start().starts_with("## ") { sec_end += 1; }
    let sub_prefix = format!("### {subsection}");
    let sub_start = match (sec_start..sec_end).find(|&i| lines[i].trim_start().starts_with(&sub_prefix)) {
        Some(i) => i + 1,
        None => return lines.join("\n"),
    };
    let mut sub_end = sub_start;
    while sub_end < sec_end && !lines[sub_end].trim_start().starts_with("### ") { sub_end += 1; }
    let key_prefix = format!("- {key}");
    if let Some(i) = (sub_start..sub_end).find(|&i| lines[i].trim_start().starts_with(&key_prefix)) {
        lines[i] = new_line.to_string(); // 替换
    } else {
        let mut insert_at = sub_end;
        while insert_at > sub_start && lines[insert_at - 1].trim().is_empty() { insert_at -= 1; }
        lines.insert(insert_at, new_line.to_string()); // 追加
    }
    lines.join("\n")
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml memory::tests::day_titles memory::tests::upsert`
Expected: PASS。

- [ ] **Step 5: 全量 check**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml`
Expected: 零 error / 零 warning。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/memory.rs
git commit -m "feat(memory): 月层当月今日行 upsert（机械汇总当天事件段 title）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

### Task 3: 月层轮转（当月→上月冻结、旧上月退出、新月→空当月）+ 年层月主题装配

**Files:**
- Modify: `src-tauri/src/memory.rs`（新增 `rotate_month` / `append_year_theme` + 私有 `rotate_month_text` / `extract_subsection_body`）
- Test: `src-tauri/src/memory.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `ensure_memory_skeleton` / `insert_in_section`（现有）。
- Produces:
  - `pub fn rotate_month(cache: &Path, completed_ym: &str, new_ym: &str) -> Result<(), String>`
  - `pub fn append_year_theme(cache: &Path, ym: &str, theme: &str) -> Result<(), String>`

**轮转语义**：跨月时 `completed_ym`（刚完成的旧当月）→ 移到 `### 上月 {completed_ym}（冻结）`（连带其逐日行），旧 `### 上月` 整段删除（退出 2 月窗口），新建空 `### 当月 {new_ym}`。

- [ ] **Step 1: 写失败测试**（追加到 `memory.rs` 的 `mod tests`）

```rust
    fn three_tier_with_current(ws_dir: &std::path::Path, current_ym: &str, current_lines: &[&str]) -> String {
        let mut body = format!("# 记忆索引\n\n## 日\n\n## 月\n### 当月 {current_ym}\n");
        for l in current_lines { body.push_str(&format!("{l}\n")); }
        body.push_str("\n### 上月\n\n## 年\n");
        std::fs::write(ws_dir.join("MEMORY.md"), body).unwrap();
        body
    }

    #[test]
    fn rotate_moves_current_to_previous_and_creates_empty_current() {
        let w = ws();
        three_tier_with_current(w.path(), "2026-07", &["- 2026-07-01: 七月A", "- 2026-07-15: 七月B"]);
        rotate_month(w.path(), "2026-07", "2026-08").unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert!(body.contains("### 上月 2026-07（冻结）"), "旧当月→上月冻结: {body}");
        assert!(body.contains("七月A") && body.contains("七月B"), "旧当月内容带到上月: {body}");
        assert!(body.contains("### 当月 2026-08"), "新建空当月: {body}");
    }

    #[test]
    fn rotate_drops_old_previous() {
        // 已有旧上月（2026-05）+ 当月（2026-06）→ 轮转到 2026-07 时，旧上月 2026-05 应退出
        let w = ws();
        let body = "# 记忆索引\n\n## 日\n\n## 月\n### 当月 2026-06\n- 2026-06-01: 六月\n\n### 上月 2026-05（冻结）\n- 2026-05-01: 五月旧\n\n## 年\n";
        std::fs::write(w.path().join("MEMORY.md"), body).unwrap();
        rotate_month(w.path(), "2026-06", "2026-07").unwrap();
        let after = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert!(!after.contains("五月旧"), "旧上月内容退出 2 月窗口: {after}");
        assert!(!after.contains("2026-05"), "旧上月标识退出: {after}");
        assert!(after.contains("六月"), "刚完成月（六月）→ 上月: {after}");
    }

    #[test]
    fn append_year_theme_adds_line_under_year() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        append_year_theme(w.path(), "2026-06", "六月主要做了X和Y").unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        let year_start = body.find("## 年").unwrap();
        let year_section = &body[year_start..];
        assert!(year_section.contains("- 2026-06: 六月主要做了X和Y"), "年节追加月主题: {year_section}");
    }

    #[test]
    fn append_year_theme_multiple_months_accumulate() {
        let w = ws();
        ensure_memory_skeleton(w.path()).unwrap();
        append_year_theme(w.path(), "2026-05", "五月主题").unwrap();
        append_year_theme(w.path(), "2026-06", "六月主题").unwrap();
        let body = std::fs::read_to_string(w.path().join("MEMORY.md")).unwrap();
        assert!(body.contains("五月主题") && body.contains("六月主题"), "多月主题累加: {body}");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml memory::tests::rotate memory::tests::append_year_theme`
Expected: FAIL —— `rotate_month` / `append_year_theme` 未定义。

- [ ] **Step 3: 实现**（追加到 `memory.rs`）

```rust
/// 跨月轮转：当月 {completed_ym} → 上月（冻结），旧上月退出，新建空当月 {new_ym}。
pub fn rotate_month(cache: &Path, completed_ym: &str, new_ym: &str) -> Result<(), String> {
    let path = cache.join("MEMORY.md");
    if !path.exists() { ensure_memory_skeleton(cache).map_err(|e| e.to_string())?; }
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let new_text = rotate_month_text(&text, completed_ym, new_ym);
    std::fs::write(&path, new_text).map_err(|e| e.to_string())
}

fn rotate_month_text(text: &str, completed_ym: &str, new_ym: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let sec_start = match lines.iter().position(|l| l.trim_start() == "## 月") {
        Some(i) => i,
        None => return lines.join("\n"),
    };
    let mut sec_end = sec_start + 1;
    while sec_end < lines.len() && !lines[sec_end].trim_start().starts_with("## ") { sec_end += 1; }
    // 取当月内容（连带其逐日行），删整个旧月节（含旧上月），重建为「上月冻结 + 空当月」
    let cur_body = extract_subsection_body(&lines, sec_start + 1, sec_end, "当月");
    let mut new_section: Vec<String> = vec![String::from("## 月")];
    new_section.push(format!("### 上月 {completed_ym}（冻结）"));
    new_section.extend(cur_body.into_iter().filter(|l| !l.is_empty()));
    new_section.push(String::new());
    new_section.push(format!("### 当月 {new_ym}"));
    let mut out: Vec<String> = lines[..sec_start].to_vec();
    out.extend(new_section);
    out.extend(lines[sec_end..].to_vec());
    out.join("\n")
}

fn extract_subsection_body(lines: &[String], from: usize, to: usize, subsection: &str) -> Vec<String> {
    let prefix = format!("### {subsection}");
    let start = match (from..to).find(|&i| lines[i].trim_start().starts_with(&prefix)) {
        Some(i) => i + 1,
        None => return Vec::new(),
    };
    let mut end = start;
    while end < to && !lines[end].trim_start().starts_with("### ") { end += 1; }
    lines[start..end].to_vec()
}

/// 年节追加一条月主题行（`- {ym}: {theme}`）。
pub fn append_year_theme(cache: &Path, ym: &str, theme: &str) -> Result<(), String> {
    let path = cache.join("MEMORY.md");
    if !path.exists() { ensure_memory_skeleton(cache).map_err(|e| e.to_string())?; }
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    let line = format!("- {ym}: {theme}");
    text = insert_in_section(&text, "年", &line);
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(())
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml memory::tests::rotate memory::tests::append_year_theme`
Expected: PASS。

- [ ] **Step 5: 全量 check**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml`
Expected: 零 error / 零 warning。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/memory.rs
git commit -m "feat(memory): 月层跨月轮转（当月→上月冻结、旧上月退出）+ 年层月主题装配

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: 月主题 LLM（综合刚完成月全部日概括成一段话）

**Files:**
- Modify: `src-tauri/src/memory.rs`（新增 `collect_month_segments`）
- Modify: `src-tauri/src/dream.rs`（新增 `MONTH_THEME_SYS` / `month_theme_prompt` / `build_month_theme`）
- Test: 两处 `mod tests`

**Interfaces:**
- Consumes: Task 3 的 `collect_month_segments`（本任务先在 memory.rs 建）；现有 `LlmRound` / `Emitter` / `Config`。
- Produces: `pub async fn build_month_theme<E: Emitter>(round: Arc<dyn LlmRound>, cfg: &Config, cache: &Path, ym: &str, emit: &E) -> Result<String, String>`

**说明**：月主题原料 = 该月所有 `memory/{Y}/{M}/*.md` 的全部事件段（机械收集，含 `## 标题` + 详情）。LLM 综合成一段话。空月（无事件段）→ `Err`（不调 LLM，避免无原料空跑）。

- [ ] **Step 1: 写失败测试**

memory.rs `mod tests` 追加：

```rust
    #[test]
    fn collect_month_segments_reads_all_days_sorted() {
        let w = ws();
        // 2026-06 月两天，各含事件段
        for (d, title) in [("2026-06-15", "六月A"), ("2026-06-01", "六月B")] {
            let p = w.path().join("memory/2026/06").join(format!("{d}.md"));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!("## 10:00 evt-{}-001 {title}\n**详情**: d\n**主语**: 用户\n", d.replace('-', ""))).unwrap();
        }
        let segs = collect_month_segments(w.path(), "2026-06");
        assert!(segs.contains("六月A") && segs.contains("六月B"), "含两天事件段: {segs}");
        // 按文件名排序：2026-06-01 在 2026-06-15 前
        assert!(segs.find("六月B").unwrap() < segs.find("六月A").unwrap(), "按日期排序: {segs}");
    }

    #[test]
    fn collect_month_segments_empty_when_no_dir() {
        let w = ws();
        assert_eq!(collect_month_segments(w.path(), "2099-01"), "");
    }
```

dream.rs `mod tests` 追加（复用现有 `ScriptedDream` / `Noemit` / `Config::default`，见 `dream.rs:379-395`）：

```rust
    #[tokio::test]
    async fn build_month_theme_collects_and_returns_theme() {
        let dir = tempfile::tempdir().unwrap();
        // 该月有事件段（原料）
        let p = dir.path().join("memory/2026/06/2026-06-15.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "## 10:00 evt-20260615-001 六月事件\n**详情**: d\n**主语**: 用户\n").unwrap();
        let round = Arc::new(ScriptedDream {
            content: "六月主要在重构记忆系统，完成了日/月/年三层。".into(), fail: false,
        });
        let theme = build_month_theme(round, &Config::default(), dir.path(), "2026-06", &Noemit).await.unwrap();
        assert!(theme.contains("重构记忆系统"), "返回月主题: {theme}");
    }

    #[tokio::test]
    async fn build_month_theme_empty_month_err_no_llm() {
        let dir = tempfile::tempdir().unwrap();
        // 无事件段 → Err，不调 LLM
        let round = Arc::new(ScriptedDream { content: "不该被调用".into(), fail: false });
        let r = build_month_theme(round, &Config::default(), dir.path(), "2099-01", &Noemit).await;
        assert!(r.is_err(), "空月应 Err: {r:?}");
    }

    #[tokio::test]
    async fn build_month_theme_llm_fail_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("memory/2026/06/2026-06-15.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "## 10:00 evt-20260615-001 X\n**详情**: d\n**主语**: 用户\n").unwrap();
        let round = Arc::new(ScriptedDream { content: String::new(), fail: true });
        let r = build_month_theme(round, &Config::default(), dir.path(), "2026-06", &Noemit).await;
        assert!(r.is_err(), "LLM 失败应传播: {r:?}");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml memory::tests::collect_month_segments dream::tests::build_month_theme`
Expected: FAIL —— `collect_month_segments` / `build_month_theme` 未定义。

- [ ] **Step 3: 实现**

memory.rs 追加：

```rust
/// 收集某月（YYYY-MM）所有 day 文件的全部事件段文本，按文件名（日期）排序，作月主题 LLM 原料。
pub fn collect_month_segments(cache: &Path, ym: &str) -> String {
    let (y, m) = match ym.split_once('-') { Some(x) => x, None => return String::new() };
    let dir = cache.join("memory").join(y).join(m);
    let mut files: Vec<(String, String)> = std::fs::read_dir(&dir).map(|rd| {
        rd.filter_map(|e| e.ok()).filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_string_lossy().into_owned();
            if !name.ends_with(".md") { return None; }
            let body = std::fs::read_to_string(&p).ok()?;
            Some((name, body))
        }).collect()
    }).unwrap_or_default();
    files.sort();
    let mut out = String::new();
    for (name, body) in &files {
        out.push_str(&format!("## {name}\n"));
        out.push_str(body);
        out.push('\n');
    }
    out
}
```

dream.rs 追加（放在 `parse_extracts` 之后、`mod tests` 之前）：

```rust
// ─── 月主题 LLM（综合当月全部日概括成一段话） ───

const MONTH_THEME_SYS: &str = "你是 dream，负责把一个月的「日」层记忆综合成一段「月主题」。给你这个月每天的事件段，请输出一段话（150-300字）概括这个月的主要活动、主题和关键人物。只输出月主题正文，不要标题、不要 JSON、不要 markdown、不要任何前后缀。";

fn month_theme_prompt(ym: &str, segments_text: &str) -> String {
    format!("把 {ym} 这个月的日层记忆综合成一段月主题：\n\n{segments_text}\n\n现在输出月主题正文（一段话）。")
}

/// 读 {ym} 月全部事件段 → LLM 综合成一段月主题。空月 → Err（不调 LLM）；LLM 失败 → Err 传播。
pub async fn build_month_theme<E: Emitter>(
    round: Arc<dyn LlmRound>, cfg: &Config, cache: &Path, ym: &str, emit: &E,
) -> Result<String, String> {
    let segments = memory::collect_month_segments(cache, ym);
    if segments.trim().is_empty() { return Err(format!("{ym} 无日层记忆，无法综合月主题")); }
    let prompt = month_theme_prompt(ym, &segments);
    let messages = vec![
        serde_json::json!({"role":"system","content": MONTH_THEME_SYS}),
        serde_json::json!({"role":"user","content": prompt}),
    ];
    let resp = round.round(&messages, cfg, emit).await?;
    let theme = resp.content.trim().to_string();
    if theme.is_empty() { return Err(format!("{ym} 月主题 LLM 返回空")); }
    Ok(theme)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml memory::tests::collect_month_segments dream::tests::build_month_theme`
Expected: PASS。

- [ ] **Step 5: 全量 check**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml`
Expected: 零 error / 零 warning。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/memory.rs src-tauri/src/dream.rs
git commit -m "feat(dream): 月主题 LLM（综合当月全部日概括成一段话）+ collect_month_segments

Co-Authored-By: Claude <noreply@anthropic.com>"
```

### Task 5: `execute_dream` 接入跨月检查 + 月层今日行 upsert

**Files:**
- Modify: `src-tauri/src/dream.rs`（新增 `check_and_rotate_month`；改 `execute_dream` 主体）
- Test: `src-tauri/src/dream.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 1 `read_dream_meta` / `write_dream_meta` / `ym_from_ts`；Task 2 `upsert_current_month_today`；Task 3 `append_year_theme` / `rotate_month`；Task 4 `build_month_theme`。现有 `execute_dream` 的全部入参。
- Produces: 改写后的 `execute_dream`（签名不变）；私有 `async fn check_and_rotate_month<E: Emitter>(round, cfg, cache, seg_ym, emit) -> Result<(), String>`。

**新 execute_dream 顺序**（segment 非空确认后）：
1. `seg_ym = ym_from_ts(now, offset)`（段最后事件本地月）
2. `check_and_rotate_month(...)`（跨则 LLM 月主题 + 年层 + 轮转 + 更新 meta；失败传播 → 不写 marker，段保留重试；首次无 meta → 初始化，不跨）
3. 现有日提取循环（`append_day_event` + `append_memory_day`）
4. `upsert_current_month_today`（机械汇总当天事件段 → 月节当月今日行）
5. 现有 dream marker（`until_seq=b`，语义不变）

- [ ] **Step 1: 写失败测试**（追加到 `dream.rs` 的 `mod tests`）

复用现有 `ScriptedDream` / `Noemit` / `Config::default` / `spawn_writer` / `flush_dream`（`dream.rs:376-399`）。新增一个能区分「月主题 round」与「日提取 round」的 fake：

```rust
    /// 双响应 round：按 system prompt 含「月主题」分发不同内容（月主题 round vs 日提取 round）。
    struct DualDream { theme: String, day_extracts: String, fail_theme: bool }
    #[async_trait]
    impl LlmRound for DualDream {
        async fn round(&self, messages: &[serde_json::Value], _: &Config, _: &dyn Emitter) -> Result<RoundResult, String> {
            let sys = messages.first().and_then(|m| m.get("content")).and_then(|c| c.as_str()).unwrap_or("");
            if sys.contains("月主题") {
                if self.fail_theme { return Err("月主题 LLM 500".into()); }
                return Ok(RoundResult { content: self.theme.clone(), reasoning: String::new(),
                    tool_calls: vec![], finish: FinishReason::Stop, usage: None,
                    assistant_message: serde_json::json!({"role":"assistant","content": self.theme}) });
            }
            Ok(RoundResult { content: self.day_extracts.clone(), reasoning: String::new(),
                tool_calls: vec![], finish: FinishReason::Stop, usage: None,
                assistant_message: serde_json::json!({"role":"assistant","content": self.day_extracts}) })
        }
    }

    const TS_JUL: u64 = 1_785_024_000_000 + 12 * 3600_000; // 2026-07-26 12:00 UTC（offset=0 → date 2026-07-26）

    #[tokio::test]
    async fn execute_dream_first_run_initializes_meta_no_rotate() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        h.append(HistoryEvent::user(TS_JUL, "main", "七月消息", &[]));
        flush_dream().await;
        let mut t = DreamTrigger::new(); t.armed = true;
        let round = Arc::new(DualDream { theme: "不该被调用".into(),
            day_extracts: r#"{"title":"七月新事","detail":"d","subject":"用户"}"#.into(), fail_theme: false });
        let r = execute_dream(round, &Config::default(), &h, &dir.path().join("history"), dir.path(), 0, 1, &Noemit).await;
        assert!(r.is_ok(), "{r:?}");
        flush_dream().await;
        // 首次：初始化 meta = 段月（2026-07），不跨月、不写年层
        assert_eq!(crate::memory::read_dream_meta(dir.path()), Some("2026-07".into()));
        let body = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(!body.contains("### 上月"), "首次不轮转，无上月: {body}");
        assert!(body.contains("七月新事"), "日提取仍写入日节: {body}");
    }

    #[tokio::test]
    async fn execute_dream_same_month_upserts_today_line() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        h.append(HistoryEvent::user(TS_JUL, "main", "七月A", &[]));
        h.append(HistoryEvent::user(TS_JUL + 1000, "main", "七月B", &[]));
        flush_dream().await;
        crate::memory::write_dream_meta(dir.path(), "2026-07").unwrap(); // meta 已是同月
        let mut t = DreamTrigger::new(); t.armed = true;
        let round = Arc::new(DualDream { theme: "不该被调用".into(),
            day_extracts: r#"{"title":"第一件","detail":"d","subject":"用户"}
{"title":"第二件","detail":"d","subject":"agent"}"#.into(), fail_theme: false });
        let r = execute_dream(round, &Config::default(), &h, &dir.path().join("history"), dir.path(), 0, 2, &Noemit).await;
        assert!(r.is_ok(), "{r:?}");
        flush_dream().await;
        let body = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        let cur = body.find("### 当月").unwrap();
        assert!(body[cur..].contains("- 2026-07-26: 第一件；第二件"), "同月 → 月节当月今日行汇总: {}", &body[cur..]);
    }

    #[tokio::test]
    async fn execute_dream_cross_month_runs_theme_rotates_appends_year() {
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        h.append(HistoryEvent::user(TS_JUL, "main", "七月消息", &[])); // seg_ym = 2026-07
        flush_dream().await;
        // 旧月（2026-06）有事件段（月主题原料）
        let jun = dir.path().join("memory/2026/06/2026-06-15.md");
        std::fs::create_dir_all(jun.parent().unwrap()).unwrap();
        std::fs::write(&jun, "## 10:00 evt-20260615-001 六月事件A\n**详情**: d\n**主语**: 用户\n").unwrap();
        // MEMORY.md 三层，当月 = 2026-06（含今日行）
        std::fs::write(dir.path().join("MEMORY.md"),
            "# 记忆索引\n\n## 日\n\n## 月\n### 当月 2026-06\n- 2026-06-15: 六月事件A\n\n### 上月\n\n## 年\n").unwrap();
        // meta = 2026-06（旧月）→ 段月 2026-07 > 2026-06 → 跨月
        crate::memory::write_dream_meta(dir.path(), "2026-06").unwrap();
        let mut t = DreamTrigger::new(); t.armed = true;
        let round = Arc::new(DualDream { theme: "六月主要做了X和Y".into(),
            day_extracts: r#"{"title":"七月新事","detail":"d","subject":"用户"}"#.into(), fail_theme: false });
        let r = execute_dream(round, &Config::default(), &h, &dir.path().join("history"), dir.path(), 0, 1, &Noemit).await;
        assert!(r.is_ok(), "{r:?}");
        flush_dream().await;
        let body = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        // 年层追加了 2026-06 月主题
        let year = body.find("## 年").unwrap();
        assert!(body[year..].contains("- 2026-06: 六月主要做了X和Y"), "年层追加月主题: {}", &body[year..]);
        // 月层轮转：旧当月 2026-06 → 上月冻结；新当月 2026-07
        assert!(body.contains("### 上月 2026-06（冻结）"), "旧当月→上月冻结: {body}");
        assert!(body.contains("### 当月 2026-07"), "新当月 2026-07: {body}");
        assert!(body[body.find("### 当月 2026-07").unwrap()..].contains("- 2026-07-26: 七月新事"),
            "新当月今日行（日提取后 upsert）: {body}");
        // meta 推进到 2026-07
        assert_eq!(crate::memory::read_dream_meta(dir.path()), Some("2026-07".into()));
        // dream marker 仍写入（until_seq = b = 1）
        let evs = read_all(&dir.path().join("history"));
        assert!(evs.iter().any(|e| e.kind == "marker"), "跨月后仍写 dream marker");
    }

    #[tokio::test]
    async fn execute_dream_cross_month_theme_fail_no_rotate_no_marker() {
        // 跨月但月主题 LLM 失败 → 不轮转、不写 marker、meta 不动、propagate Err（段保留重试）
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_writer(dir.path().join("history"), 0);
        h.append(HistoryEvent::user(TS_JUL, "main", "七月消息", &[]));
        flush_dream().await;
        let jun = dir.path().join("memory/2026/06/2026-06-15.md");
        std::fs::create_dir_all(jun.parent().unwrap()).unwrap();
        std::fs::write(&jun, "## 10:00 evt-20260615-001 六月事件A\n**详情**: d\n**主语**: 用户\n").unwrap();
        std::fs::write(dir.path().join("MEMORY.md"),
            "# 记忆索引\n\n## 日\n\n## 月\n### 当月 2026-06\n- 2026-06-15: 六月事件A\n\n### 上月\n\n## 年\n").unwrap();
        crate::memory::write_dream_meta(dir.path(), "2026-06").unwrap();
        let mut t = DreamTrigger::new(); t.armed = true;
        let round = Arc::new(DualDream { theme: String::new(),
            day_extracts: r#"{"title":"七月新事","detail":"d","subject":"用户"}"#.into(), fail_theme: true });
        let r = execute_dream(round, &Config::default(), &h, &dir.path().join("history"), dir.path(), 0, 1, &Noemit).await;
        assert!(r.is_err(), "月主题失败应 propagate Err: {r:?}");
        flush_dream().await;
        let body = std::fs::read_to_string(dir.path().join("MEMORY.md")).unwrap();
        assert!(!body.contains("### 上月 2026-06（冻结）"), "失败不轮转: {body}");
        assert!(!body.contains("六月主要做了"), "失败不写年层: {body}");
        assert_eq!(crate::memory::read_dream_meta(dir.path()), Some("2026-06".into()), "失败 meta 不动");
        let evs = read_all(&dir.path().join("history"));
        assert!(evs.iter().all(|e| e.kind != "marker"), "失败不写 dream marker（段保留重试）");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml dream::tests::execute_dream_first_run dream::tests::execute_dream_same_month dream::tests::execute_dream_cross_month`
Expected: FAIL —— `execute_dream` 还未接入跨月 / upsert（同月今日行断言、跨月年层 / 轮转断言、首次 meta 初始化断言都不过）。

- [ ] **Step 3: 实现**（改 `dream.rs` 的 `execute_dream` + 新增 `check_and_rotate_month`）

替换现有 `execute_dream` 函数体（从 `pub async fn execute_dream` 到其结尾 `}`）为：

```rust
pub async fn execute_dream<E: Emitter>(
    round: Arc<dyn LlmRound>,
    cfg: &Config,
    history: &HistoryWriterHandle,
    history_dir: &Path,
    cache: &Path,
    a: u64,
    b: u64,
    emit: &E,
) -> Result<(), String> {
    let events = read_all(history_dir);
    let segment: Vec<&HistoryEvent> = events
        .iter()
        .filter(|e| e.seq >= a && e.seq <= b && e.thread == "main" && e.kind != "marker")
        .collect();
    if segment.is_empty() {
        return Ok(()); // 空跳过
    }
    let now = segment.last().expect("non-empty").ts;
    let offset = crate::history::local_offset_secs();
    let seg_ym = memory::ym_from_ts(now, offset);
    // 跨月检查：跨则 LLM 月主题 + 年层 + 轮转（失败传播 → 不写 marker，段保留重试 §13.1）
    check_and_rotate_month(round.clone(), cfg, cache, &seg_ym, emit).await?;
    let prompt = dream_prompt(a, b, &segment);
    let messages = vec![
        serde_json::json!({"role":"system","content": DREAM_SYS}),
        serde_json::json!({"role":"user","content": prompt}),
    ];
    let resp = round.round(&messages, cfg, emit).await?; // 失败传播（不写 marker）
    let extracts = parse_extracts(&resp.content);
    for (i, ext) in extracts.iter().enumerate() {
        // P1 铁律③：seq 指针代码盖戳 [a,b]，LLM 只写了 title/detail/subject。
        let _ = memory::append_day_event(cache, now, offset, &ext.title, &ext.detail, &ext.subject, (a, b), None);
        let _ = memory::append_memory_day(cache, now, offset, (i + 1) as u32, &ext.title);
    }
    // 月层当月今日行 upsert（机械汇总当天所有事件段 title → 月节「### 当月」per-day 行）
    let _ = memory::upsert_current_month_today(cache, now, offset);
    // 写 dream marker（until_seq=b，dream-start；ts 同段最后事件）。
    history.append(HistoryEvent::marker(now, "dream", b));
    Ok(())
}

/// 跨月检查 + 月层轮转。基于段月 `seg_ym` 与 `.dream-meta.json` 的 `current_month`：
/// - 无 meta（首次）→ 初始化 `current_month = seg_ym`，不跨。
/// - `seg_ym > current_month`（跨月）→ LLM 综合旧月成月主题 → 追加年层 → 轮转（旧当月→上月、旧上月退出、新月→空当月）→ 更新 meta。
/// - 否则（同月 / 未来月）→ 不动。
/// 跨月时任一步失败 → propagate Err（不轮转、不写 meta，下次 dream 仍检测到跨月重试）。
async fn check_and_rotate_month<E: Emitter>(
    round: Arc<dyn LlmRound>,
    cfg: &Config,
    cache: &Path,
    seg_ym: &str,
    emit: &E,
) -> Result<(), String> {
    match memory::read_dream_meta(cache) {
        None => {
            memory::write_dream_meta(cache, seg_ym)?;
        }
        Some(cur) if seg_ym > cur.as_str() => {
            let theme = build_month_theme(round.clone(), cfg, cache, &cur, emit).await?;
            memory::append_year_theme(cache, &cur, &theme)?;
            memory::rotate_month(cache, &cur, seg_ym)?;
            memory::write_dream_meta(cache, seg_ym)?;
        }
        Some(_) => {}
    }
    Ok(())
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --lib --manifest-path src-tauri/Cargo.toml dream::`
Expected: PASS（含本任务 4 个新测试 + 现有 `run_dream_*` 全绿；`run_dream_*` 走 `run_dream`→`execute_dream`，首次无 meta → 初始化，行为兼容）。

- [ ] **Step 5: 全量 check + 跑全部后端测试**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml && cargo test --lib --manifest-path src-tauri/Cargo.toml`
Expected: 零 error / 零 warning；全部测试通过（含 `run_dream_*` 回归）。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/dream.rs
git commit -m "feat(dream): execute_dream 接入跨月轮转 + 月层当月今日行 upsert

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 6: `mem index [层]` 只读命令（读 MEMORY.md 日 / 月 / 年节）

**Files:**
- Modify: `src-tauri/src/mem_cli.rs`（新增 `index` + 私有 `parse_sections`）
- Modify: `src-tauri/src/bin/mem.rs`（dispatch 加 `"index"` 臂 + usage 加一行）
- Test: 两处 `mod tests`

**Interfaces:**
- Consumes: `MEMORY.md`（Task 1 三层骨架产物）。mem_cli 函数形参沿用 `workspace: &Path`（实参是 cache，与现有 ls/read/history/search 一致，不改名 —— YAGNI）。
- Produces: `pub fn index(workspace: &Path, tier: Option<&str>) -> String`

**说明**：agent 脱离 pinned 窗口时（经 bash 调 `mem index`）取 MEMORY.md 摘要。`tier` 支持 `日`/`day`、`月`/`month`、`年`/`year`，不传 = 全部三层。不读 day 文件 / history（那些走 `ls`/`read`/`history`）。

- [ ] **Step 1: 写失败测试**

mem_cli.rs `mod tests` 追加：

```rust
    fn write_memory_md(ws_dir: &std::path::Path, body: &str) {
        std::fs::write(ws_dir.join("MEMORY.md"), body).unwrap();
    }
    const SAMPLE: &str = "# 记忆索引\n\n## 日\n- 12:30 evt-001 今日A\n\n## 月\n### 当月\n- 2026-07-26: 今日A\n\n### 上月\n\n## 年\n- 2026-06: 六月主题\n";

    #[test]
    fn index_all_returns_full_text() {
        let w = ws();
        write_memory_md(w.path(), SAMPLE);
        let out = index(w.path(), None);
        assert!(out.contains("## 日") && out.contains("## 月") && out.contains("## 年"), "不传=全部: {out}");
    }

    #[test]
    fn index_day_returns_only_day_section() {
        let w = ws();
        write_memory_md(w.path(), SAMPLE);
        let out = index(w.path(), Some("日"));
        assert!(out.contains("今日A"), "日节内容: {out}");
        assert!(!out.contains("六月主题"), "不含年节: {out}");
        assert!(!out.contains("### 当月"), "不含月节子标题: {out}");
    }

    #[test]
    fn index_month_returns_only_month_section() {
        let w = ws();
        write_memory_md(w.path(), SAMPLE);
        let out = index(w.path(), Some("月"));
        assert!(out.contains("### 当月") && out.contains("2026-07-26"), "月节内容: {out}");
        assert!(!out.contains("六月主题"), "不含年节: {out}");
    }

    #[test]
    fn index_year_returns_only_year_section() {
        let w = ws();
        write_memory_md(w.path(), SAMPLE);
        let out = index(w.path(), Some("年"));
        assert!(out.contains("六月主题"), "年节内容: {out}");
        assert!(!out.contains("今日A"), "不含日节: {out}");
    }

    #[test]
    fn index_english_alias_works() {
        let w = ws();
        write_memory_md(w.path(), SAMPLE);
        assert_eq!(index(w.path(), Some("day")), index(w.path(), Some("日")));
        assert_eq!(index(w.path(), Some("month")), index(w.path(), Some("月")));
        assert_eq!(index(w.path(), Some("year")), index(w.path(), Some("年")));
    }

    #[test]
    fn index_missing_memory_md_returns_hint() {
        let w = ws();
        let out = index(w.path(), None);
        assert!(out.contains("无 MEMORY.md"), "缺文件给提示: {out}");
    }
```

bin/mem.rs `mod tests` 追加：

```rust
    #[test]
    fn dispatch_routes_index() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "# 记忆\n\n## 日\n- x\n").unwrap();
        let out = dispatch(&["mem".into(), "index".into(), "日".into()], dir.path());
        assert!(out.contains("x"), "index 日 路由成功: {out}");
    }
    #[test]
    fn dispatch_index_no_tier_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("MEMORY.md"), "# 记忆\n\n## 日\n- x\n\n## 年\n- y\n").unwrap();
        let out = dispatch(&["mem".into(), "index".into()], dir.path());
        assert!(out.contains("x") && out.contains("y"), "不传 tier=全部: {out}");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --bin mem mem_cli::tests::index bin_mem::tests::dispatch_routes_index bin_mem::tests::dispatch_index_no_tier`
Expected: FAIL —— `index` 未定义（mem_cli），dispatch 无 `"index"` 臂。

- [ ] **Step 3: 实现**

mem_cli.rs 追加（放在 `search` 之后、`mod tests` 之前）：

```rust
/// mem index [日|月|年]：读 MEMORY.md 某层（不传 = 全部三层）。
pub fn index(workspace: &Path, tier: Option<&str>) -> String {
    let path = workspace.join("MEMORY.md");
    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return String::from("（无 MEMORY.md；dream 尚未整理记忆）"),
    };
    let want = match tier {
        None => return text,
        Some("日") | Some("day") => "日",
        Some("月") | Some("month") => "月",
        Some("年") | Some("year") => "年",
        Some(other) => return format!("未知层: {other}（可选: 日/月/年）"),
    };
    let sections = parse_sections(&text);
    match sections.iter().find(|(n, _)| n == want) {
        Some((_, body)) => format!("## {want}\n{body}"),
        None => format!("（MEMORY.md 无「{want}」节）"),
    }
}

fn parse_sections(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur_name: Option<String> = None;
    let mut cur_body = String::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("## ").map(str::trim) {
            if let Some(n) = cur_name.take() { out.push((n, std::mem::take(&mut cur_body))); }
            cur_name = Some(name.to_string());
        } else if cur_name.is_some() {
            cur_body.push_str(line);
            cur_body.push('\n');
        }
    }
    if let Some(n) = cur_name { out.push((n, cur_body)); }
    out
}
```

bin/mem.rs 的 `dispatch` 加一个臂（在 `Some("search")` 之后、`Some("--help")` 之前）：

```rust
        Some("index") => mem_cli::index(cache, args.get(2).map(|s| s.as_str())),
```

bin/mem.rs 的 `usage` 在 `search` 行之后加一行：

```rust
"  mem index [日|月|年]               打印 MEMORY.md 某层索引（不传=全部）\n"
```

完整 `usage` 改为：

```rust
fn usage() -> String {
    "mem —— 记忆深思（零 LLM 只读 drill）\n\n用法:\n  mem ls [year [month]]              列年/月/天（天带标题）\n  mem read <date>                    打印当天事件段（含对话索引指针）\n  mem history <date> [--seq a..b]    读原始对话（可按 seq 过滤）\n  mem search <query> [--raw]         关键字 grep（默认 memory/，--raw 扩到 history）\n  mem index [日|月|年]               打印 MEMORY.md 某层索引（不传=全部）\n\ncache 定位: --cache > $OVOICE_CACHE > (--workspace/$OVOICE_WORKSPACE 别名) > next-to-exe config > %APPDATA% config > %APPDATA%/cache".into()
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --bin mem mem_cli::tests::index bin_mem::tests::dispatch`
Expected: PASS。

- [ ] **Step 5: 全量 check + 验 mem 能独立编译**

Run: `cargo check --tests --manifest-path src-tauri/Cargo.toml`
Expected: 零 error / 零 warning。

（dev server 关闭时可选验独立二进制：`cargo build --bin mem --manifest-path src-tauri/Cargo.toml`。dev server 占 exe 时**不要** `cargo build` 主 bin，但 `--bin mem` 不碰 ovoice.exe，可跑。）

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/mem_cli.rs src-tauri/src/bin/mem.rs
git commit -m "feat(mem): index [层] 读 MEMORY.md 日/月/年节（脱离 pinned 窗口取摘要）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## 端到端手测清单（spec §13.2；dev server + 真 LLM，Task 1-6 落地后）

1. **首次 dream（同月）**：和 agent 聊几句 → 触发 dream（idle 600s 或 cap 50）→ 查 MEMORY.md：`## 日` 有 per-event 行；`## 月 ### 当月 YYYY-MM` 有今日汇总行；`memory/.dream-meta.json` 的 `current_month` = 当前月；`## 年` 空。
2. **同日二次 dream**：再聊 + 触发 → MEMORY.md 当月今日行**更新不重复**（一行，含最新汇总）。
3. **跨月 dream**（需构造：手动改 `.dream-meta.json` 的 `current_month` 为上月 + 在 `memory/{旧Y}/{旧M}/` 放 day 文件 + 把 MEMORY.md 当月改成旧月）→ 触发 dream → 查：`## 年` 追加 `- 旧月: <月主题>`；`## 月` 上月 = 旧月（冻结，含旧当月逐日行）、当月 = 新月（空，再填今日行）；`current_month` = 新月。
4. **`mem index`**：agent bash 调 `mem index` / `mem index 月` / `mem index 年` → 返回对应节。
5. **回归**：`mem ls/read/history/search` 不变；context 重建（marker 切边）不变；重启后对话正常。

## Self-Review（writing-plans 自检结论）

**1. Spec coverage**（对照 `2026-07-26-memory-tiers-design.md`）：
- 三层（日/月/年）+ 砍周 → Task 1（骨架去周 + 旧四层迁移）。✓
- 月层 2 月滚动窗口（上月冻结 + 当月逐日）→ Task 1（`### 当月`/`### 上月` 子标题）+ Task 2（当月今日行 upsert）+ Task 3（轮转）。✓
- 月主题 = 跨月 LLM 综合 → Task 4（`build_month_theme`）+ Task 5（`check_and_rotate_month` 跨月触发）。✓
- 年 = 月主题集合 → Task 3（`append_year_theme`）+ Task 5（跨月装配）。✓
- 全代码规则、LLM 只产日提取 + 月主题 → 跨月判定/轮转/年层/今日行汇总全确定性代码；LLM 仅 Task 4 月主题（+现有日提取）。✓
- 无 chrono → 全用 `date_from_ts_local` + `YYYY-MM` 字符串比对。✓
- dream 一次 run（日提取→跨月检查→marker）→ Task 5 改 `execute_dream`。✓
- `ensure_memory_skeleton` 四层→三层 → Task 1。✓
- `.dream-meta.json` 状态 → Task 1（读写）+ Task 5（dream 读写）。✓
- `mem index [层]` → Task 6。✓
- day 文件 append-only 永不删 → 本次不改 `append_day_event`，只读 `collect_month_segments`/`day_titles`。✓
- cache_dir 内运作 → 所有路径基于 `cache`/`workspace`（mem_cli 形参，实参 cache）。✓

**2. Placeholder scan**：无 TBD/TODO；每步含完整测试代码 + 实现代码 + 命令 + 期望。✓

**3. Type consistency**：
- `ensure_memory_skeleton(cache) -> io::Result<()>`（Task 1 改内容，签名不变，lib.rs:403 不破）。✓
- `read_dream_meta(cache) -> Option<String>` / `write_dream_meta(cache, &str) -> Result<(), String>`（Task 1 定，Task 5 用）。✓
- `ym_from_ts(ts, offset) -> String`（Task 1 定，Task 5 用）。✓
- `upsert_current_month_today(cache, ts, offset) -> Result<(), String>`（Task 2 定，Task 5 用）。✓
- `rotate_month(cache, &str, &str)` / `append_year_theme(cache, &str, &str)`（Task 3 定，Task 5 用）。✓
- `collect_month_segments(cache, &str) -> String`（Task 4 定，build_month_theme 用）。✓
- `build_month_theme(round, cfg, cache, ym, emit) -> Result<String, String>`（Task 4 定，Task 5 用）。✓
- `index(workspace, tier: Option<&str>) -> String`（Task 6 定，dispatch 用 `args.get(2).map(|s| s.as_str())` 即 `Option<&str>`）。✓

**P1 铁律落地**：①单写（dream 单飞，已有，Task 5 不破）；③seq 盖戳（`append_day_event` 不动，Task 5 仍传 `(a,b)`）；④cap 只数 user（`DreamTrigger.check` 不动）；marker 重建边界（`dream` marker 仍 `until_seq=b`，Task 5 仅在其前插入跨月检查，不改 marker）。Task 5 测试 `execute_dream_cross_month_theme_fail_no_rotate_no_marker` 守住"失败不写 marker"。

**已知取舍（用户审计划时可改）**：
- MEMORY.md 月节内部用 `### 当月 YYYY-MM` / `### 上月 YYYY-MM（冻结）` 子标题（spec 未明确内部结构，此为实现选择）。
- 当月今日行 = 当天事件段 title 用「；」连接（机械汇总，无 LLM）；月主题 LLM 输入 = 当月所有 day 文件全部事件段。
- 旧四层 MEMORY.md 轻量迁移（删 `## 周` + 补子标题），Task 1 `ensure_memory_skeleton` 内做，幂等。
- `mem_cli` 函数形参仍名 `workspace`（实参 cache），不改名（YAGNI；与现有四命令一致）。

## 执行方式

**Subagent-Driven（推荐）** —— 用 `superpowers:subagent-driven-development`：每任务 fresh implementer subagent + task reviewer（spec 合规 + 代码质量）+ 末尾 whole-branch review。任务有依赖（5 依赖 1-4），按序派发。

**Inline Execution** —— 用 `superpowers:executing-plans`：本会话批量执行 + checkpoint。

（计划已保存到 `docs/superpowers/plans/2026-07-26-memory-tiers.md`，待用户审批后进 SDD。）
