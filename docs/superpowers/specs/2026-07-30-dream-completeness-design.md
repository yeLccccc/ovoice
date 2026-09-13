# dream 完整性改造设计（回合制 + 程序化兜底 + LLM 增强 + 留尾）

- **日期**：2026-07-30
- **关联**：P-2026-008 / #010（dream 摘要事实失真与批次错配）、`docs/dream-flow.md`、`docs/mem-cli.md`、`docs/superpowers/specs/2026-07-30-dream-completeness-review.md`（eng review，13 findings 已吸收）
- **状态**：待 user 确认（review 修订版 v2）
- **目标分支**：`feat/dream-completeness`（基于 master）

> **v2 修订说明**：经 gstack plan-eng-review + outside voice（独立 subagent）review，发现 1 个 P0（F1：tail retention 因 `build_messages` 认 `marker.seq` 而非 `until_seq` 而**根本不工作**）+ 6 P1 + 6 P2。本版按 review 决策（F1=A 改 build_messages、F9=A 温和合并 struct、F11=A 强制拆）修订。详见 review 报告。

## 1. 背景与问题

当前 dream 提取（`dream.rs:139-179 execute_dream`）的完整性靠 LLM 主观判断，存在五类丢失：

1. **LLM 自由裁剪**：`DREAM_SYS`（`dream.rs:112`）要求"对每个**值得记住的**事件输出一行"——"值得记住"是主观判断，LLM 会主动跳过它认为不重要的回合（如补登 issues、短确认等），**无程序化兜底**。
2. **坏行静默跳过**：`parse_extracts`（`dream.rs:252-269`）对非 `{` 开头、JSON 解析失败、title 空的行 `continue` 静默丢弃，**无日志、无计数**。
3. **段粒度同 ts**：一次 dream 的所有 extract 共用 `now = segment.last().ts`（`dream.rs:157`）和同一 `seq[a,b]`（`memory.rs:41`），原始时间分布信息丢失。
4. **当前进行中对话失忆（设计目标，但 v1 留尾方案有 P0 bug——见下）**：dream 切段 `[a, current_seq]`，marker 推进 `last_dream_marker_seq = current_seq`。下轮 `build_messages`（`context.rs:75`）按 marker 过滤 → 刚发生的回合被 marker 窗口排除。
5. **[P0 F1] `build_messages` 的 marker 过混淆**（review 发现）：`context.rs:70-71` `last_marker_seq` 取 `marker.seq`（marker 事件在 history 的位置序号），**不是 `data["until_seq"]`**。marker 在 `execute_dream` 末尾 append → `marker.seq > until_seq`。现状因 `b = current_seq`、无并发时 `marker.seq == until_seq == b` 巧合相等没暴露；v1 留尾方案 `b = current_seq − tail_rounds` 后 `marker.seq > until_seq`，filter `e.seq > marker.seq` 排除全部 tail → **留尾静默失效**。同时现状还有个潜在 bug：dream 期间用户发的事件（seq ∈ (b, marker.seq)）也被 marker.seq 切掉永久失忆。

## 2. 目标

- **G1 完整性（硬保证）**：dream 段内每一个回合都必出至少一条记忆。不靠 LLM "值得记住"判断；落盘失败必须 propagate 不写 marker（F4）。
- **G2 留尾防失忆**：dream 不吃最近 N 个回合，让当前进行中的对话在下轮 `build_messages` rebuild 仍以原始形态可见。**前提**：`build_messages` 改认 `until_seq`（F1 修复）。
- **G3 允许合并（受约束）**：相邻同主题回合可合并成一条记忆，但合并组内回合 **seq 必须连续**（F6）、组大小 ≤ `dream_merge_max_rounds`（F11 强制拆）、seq 指针 = 并集。
- **G4 失败不阻塞（但不写半残）**：LLM 失败/格式坏 → 程序化机械兜底，dream 仍完成落盘；但**落盘 IO 失败必须 propagate**（F4），不写 marker、段重试。
- **G5 联想向标题**：机械兜底保完整，LLM 增强保"可联想"质量；质量让位于完整性。

## 3. 非目标

- **N1**：history 脱敏层不做（#008 关闭，产品决策）。
- **N2**：超长段分批喂 LLM 本次不做（YAGNI；留尾 + cap=50 + prompt 瘦身已显著缩短段长度，分批留待按实测决定）。
- **N3**：不改 dream 触发策略（idle 2h / cap 50 不变）。
- **N4**：不改 mem CLI（读端契约不变；新 dream 产出的 day 文件段格式兼容 mem 现有解析）。
- **N5**：不回填历史数据（旧 day 文件 / MEMORY.md 原样保留；新逻辑仅对后续 dream 生效）。

## 4. 架构总览

### 4.1 数据流

```
dream 触发（idle 2h / cap 50 user）
  │
  ▼
① prepare_dream（持锁同步）           ◀── 只置 in_flight + 捕获 a = last_dream_marker_seq+1
  │                                      不再此处算 b（F7：tail 边界移入 execute 用同一份 events）
  │                                      只接 tail_rounds 参数（透传 cfg）
  ▼
② execute_dream（async，不持锁跨 await）
  │  read_all(history) → events           ◀── 单次 read_all（F8：透传自 dispatch_dream 决策阶段，
  │  filter seq∈[a, cur] ∧ main ∧ ≠marker     不再 execute 内重复读）
  │
  ├ ②a tail_aware_b(events, cur, tail_rounds) → b   ◀── F7：在 execute 内算，
  │       b = cur 往回跳过 tail_rounds 个 user          用 split 同源 events
  │       空段（b < a）→ 推进 marker 到 cur（F12：tail 永不整，符合"停说 2h 就该整"）
  │
  ├ ②b filter seq∈[a, b] → segment
  │       空段（user ≤ tail）→ 写 marker(until_seq=cur) 推进 frontier，返回 Ok（F12）
  │
  ├ ②c split_rounds(segment) → Vec<Round>           ◀── 按 kind=user 边界切回合
  ├ ②d mechanical_extract(每回合) → Vec<FinalEvent>  ◀── F9：MechanicalEvent 并入 FinalEvent(round_idx=Some)
  │
  ├ ②e 跨月检查 check_and_rotate_month               ◀── F3：移到 append 之前仍看段末月判定，
  │       （段跨月见 §11 R2 处理）                        但段跨月时按事件 ts 分组判（见 §5.8）
  │
  ├ ②f LLM 增强（失败 catch → groups=[]）
  │       parse_groups → Vec<Group>
  │
  ├ ②g reconcile_groups(groups, mechanical, max_rounds)
  │       非连续拆子组（F6）+ 超限强制拆（F11）+ 漏补机械
  │       → Vec<FinalEvent>（每个含 seq_a/b/last_ts/title/detail/subject）
  │
  └ ②h 落盘（每个 FinalEvent）
        • append_day_event(cache, ev.last_ts, ..., (ev.seq_a,ev.seq_b), None)
          —— F4：任一 Err → propagate，不写 marker，段重试
        • evt_no 用 day_event_count 起算（F10：与 day 文件 evt-NNN 同源）
        • 收集所有 ev.last_ts 的 distinct 日期 → 每个日期 upsert_current_month_today（F2）
        • check_and_rotate_month 若 ②e 未跑则此处在 append 后跑（F3：rotate 在 append 后）
        • 写 marker(until_seq = b)；F12 空段用 until_seq = cur
  │
  ▼
③ 下轮 build_messages（F1 改造）
   last_marker_seq 读 marker.data["until_seq"]（非 marker.seq）
   filter e.seq > until_seq → tail（seq ∈ (b, cur)）保留可见 = G2 达成
   dream 期间新事件（seq > cur）也 > b → 可见（顺带修现状 bug）
```

### 4.2 关键不变量

1. **完整性**：段内每个回合必出至少一条记忆（LLM 覆盖优先，漏的机械兜底）。
2. **留尾防失忆（依赖 F1）**：`b = cur − tail_rounds` 在 execute 内算（F7），写 `marker(until_seq=b)`；`build_messages` 改认 `until_seq`（§5.2）后，tail 事件 seq ∈ (b, marker.seq) 满足 `seq > until_seq` → 保留可见。
3. **合并组 seq 连续 + 并集**（F6）：`reconcile_groups` 强制组内回合 seq 连续（非连续拆子组）；seq 指针 = `[min_a, max_b]`，`mem history --seq min_a..max_b` 还原整组原始对话且不含其它组回合。
4. **LLM 失败不阻塞，但 IO 失败 propagate**（F4）：LLM 失败 → 全机械兜底仍落盘；`append_day_event` 任一 Err → 不写 marker、不推进 frontier、段重试（§13.1）。
5. **空段推进 marker**（F12）：user 数 ≤ tail → 段为空 → 写 `marker(until_seq=cur)` 推进 frontier（tail 永不被 dream，符合"停说 2h 就该整"），避免 idle tight-loop。

## 5. 详细设计

### 5.1 prepare_dream（改：只捕获 a + 接 tail_rounds）

`dream.rs:126-135` 现状：
```rust
let b = history.current_seq();
let a = trigger.last_dream_marker_seq + 1;
```

改为（F7：b 移入 execute）：
```rust
pub fn prepare_dream(trigger: &mut DreamTrigger, history: &HistoryWriterHandle) -> Option<(u64, ())> {
    if trigger.in_flight { return None; }
    let a = trigger.last_dream_marker_seq + 1;
    trigger.in_flight = true; // 单飞锁；不动 frontier；不在此算 b
    // cur 在 execute 内取（与 read_all 同源，避免 prepare↔execute 快照 race —— F7/F11-review）
    Some((a, ()))
}
```

> b 不再在 prepare 算。prepare↔execute 之间若有 await，prepare 的 events 快照与 execute 的 current_seq 会不同步（review F7 race）。把 tail 计算移入 execute，用 execute 内 `read_all` 的同一份 events + 该次 `current_seq`，彻底同源。

### 5.2 build_messages 改造（F1 P0 修复 — 新增节）

**问题**（review F1）：`context.rs:70-71` `last_marker_seq` 取 `marker.seq`，filter `e.seq > marker.seq`。marker 在 dream 末尾 append → `marker.seq > until_seq` → tail 被排除。

**修复**（决策 A）：改 `last_marker_seq` 读 `data["until_seq"]`：

```rust
/// 取最后一个 marker 的 until_seq（dream/reset 都带）；无 marker → None（从头重建）。
fn last_marker_seq(events: &[HistoryEvent]) -> Option<u64> {
    events.iter()
        .filter(|e| e.kind == "marker")
        .filter_map(|e| e.data.get("until_seq").and_then(|v| v.as_u64()))
        .max()
}
```

`build_messages`（`context.rs:75-88`）的 filter 不变（仍是 `e.seq > cut`），但 `cut` 现在是 `until_seq`。

**向后兼容性**：现状 `b = current_seq`、无并发时 `marker.seq == until_seq == b`，改前改后 filter 结果一致。dream 期间有新事件时（`marker.seq > until_seq`），改前 dream 期间事件被切掉（**现状潜在 bug**），改后可见——顺带修复。

**回归测试**（§8 新增）：`tail_events_survive_marker_in_next_build_messages`——dream 跑后构造 tail 事件 + marker，断言 `build_messages` 输出含 tail。

> 所有 marker 都带 until_seq：dream marker（`dream.rs:177` `marker(now,"dream",b)`）、reset marker（`dream.rs:317` `marker(ts,"reset",seq)`）。`HistoryEvent::marker` 第三参数即 until_seq。

### 5.3 split_rounds（新增，原 §5.2 不变）

按 `kind=user` 边界切。输入：段内事件（`seq∈[a,b]`，main，非 marker），按 seq 升序。输出：`Vec<Round>`。

```rust
struct Round<'a> {
    seq_a: u64, seq_b: u64, last_ts: u64,
    user_text: &'a str,
    assistant: Vec<&'a HistoryEvent>,
    tools: Vec<&'a HistoryEvent>,
}

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
            "assistant" => { if let Some(r) = cur.as_mut() { r.assistant.push(e); r.seq_b = e.seq; r.last_ts = e.ts; } }
            "tool_result" | "external" | "subagent_result" => { if let Some(r) = cur.as_mut() { r.tools.push(e); r.seq_b = e.seq; r.last_ts = e.ts; } }
            _ => {}
        }
    }
    if let Some(r) = cur { rounds.push(r); }
    rounds
}
```

> 段首非 user 的前导事件：挂到下一个 user 回合（实现取此）。

### 5.4 机械底座（新增 `mechanical_extract`，F9 合并 struct）

**F9 决策 A（温和合并）**：`MechanicalEvent` 与 `FinalEvent` 合并为一个 struct，`round_idx: Option<usize>` 区分（机械底座 `Some`，reconcile 输出 `None`）。

```rust
struct FinalEvent {
    round_idx: Option<usize>,   // Some(i) = 机械底座（reconcile 用）；None = 已落盘的最终事件
    seq_a: u64, seq_b: u64, last_ts: u64,
    title: String, detail: String, subject: String,
}

fn mechanical_extract(idx: usize, r: &Round) -> FinalEvent {
    let user_clean = collapse_ws(r.user_text);
    let title = truncate_title(&user_clean, 30);
    let asst_first = r.assistant.first()
        .and_then(|a| a.data.get("content").and_then(|v| v.as_str()))
        .map(truncate_first_sentence)
        .unwrap_or_else(|| if has_tool_calls(r) { "(调用工具)".into() } else { String::new() });
    let detail = truncate(&format!("{} → {}", truncate(&user_clean, 100), asst_first), 200);
    FinalEvent {
        round_idx: Some(idx), seq_a: r.seq_a, seq_b: r.seq_b, last_ts: r.last_ts,
        title: if title.is_empty() { "（无文本）".into() } else { title },
        detail, subject: "用户".into(),
    }
}
```

辅助（同 v1）：`collapse_ws` / `truncate_title`（去末尾标点 `。，！？、,.!?`）/ `truncate_first_sentence`（首句号/换行或前 80 字）/ `truncate(s,n)`（Unicode 安全，按 char_indices）。

### 5.5 LLM 增强 prompt（改 `DREAM_SYS` + `dream_prompt`，F13 去机械底座行）

#### 新 `DREAM_SYS`（替换 `dream.rs:112`）

```
你是 dream，负责把一段对话整理成「日」层记忆。输入按回合编号给出。请把属于同一事件的回合合并成一个分组，对每个分组输出一行 JSON：{"rounds":[回合编号...],"title":"联想向简短标题","detail":"一句话详情","subject":"主语(用户/agent/子代理#N)"}。硬性要求：①每个回合编号必须恰好被一个分组覆盖，不许漏、不许重复（回合编号 1..N 全覆盖）；②单个分组的回合数不超过 {MAX_ROUNDS}；③一个分组内的回合编号必须连续（如 [3,4,5]，不允许 [3,5]）；④标题要适合联想回忆（看到标题能想起这段对话）；⑤只输出 JSON 行，不要任何其他文字、不要 markdown 代码块。
```

> v2 加 ③ 连续要求（F6）+ 明确"连续"语义。`{MAX_ROUNDS}` = `cfg.dream_merge_max_rounds`（默认 5）。

#### 新 `dream_prompt`（替换 `dream.rs:234-249`，F13 去机械底座行）

```
把下面 {N} 个回合（history seq[{a},{b}]）整理成事件分组：

回合1 [seq{a1}-{b1}] 用户：{user 截 100 字}
  助手：{assistant 首句 或 (调用工具)}
回合2 [seq{a2}-{b2}] 用户：{...}
  ...

输出 JSON 行（每行一个分组，必须覆盖所有回合编号 1..{N}，单组回合数 ≤ {MAX_ROUNDS}，组内编号连续）。
```

> **F13**：去掉 v1 的"机械底座：{title} / {detail}"行——LLM 不需要它（user+assistant 已是信号），机械底座是给兜底用的不是给 LLM。减 prompt 体积、降 cap=50 时的 parse 失败率。

### 5.6 parse_groups + reconcile_groups（F6 连续拆 + F11 超限拆）

#### parse_groups（原 §5.5 不变，坏行计数）

```rust
struct Group { rounds: Vec<usize>, title: String, detail: String, subject: String }

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
                groups.push(Group { rounds, title,
                    detail: v.get("detail").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    subject: v.get("subject").and_then(|x| x.as_str()).unwrap_or("用户").to_string() });
            }
            Err(_) => { bad_lines += 1; }
        }
    }
    (groups, bad_lines)
}
```

#### reconcile_groups（F6 + F11 重写）

```rust
struct ReconcileStats { filled_by_mechanical: u32, duplicate: u32, out_of_range: u32, over_limit: u32, non_contiguous_split: u32 }

fn reconcile_groups(
    groups: Vec<Group>, mechanical: &[FinalEvent], max_rounds: u64,
) -> (Vec<FinalEvent>, ReconcileStats) {
    let n = mechanical.len();
    let mut covered = vec![false; n];
    let mut out = Vec::new();
    let mut stats = ReconcileStats { filled_by_mechanical: 0, duplicate: 0, out_of_range: 0, over_limit: 0, non_contiguous_split: 0 };

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

        // F6：非连续拆子组（按 seq 升序后检查相邻 idx 是否连续）
        idxs.sort();
        let mut chunks: Vec<Vec<usize>> = vec![vec![idxs[0]]];
        for &i in &idxs[1..] {
            let last = *chunks.last().unwrap().last().unwrap();
            // 连续 = round idx 相邻（i == last+1）。注：idx 是 0-based round 序号。
            if i == last + 1 { chunks.last_mut().unwrap().push(i); }
            else { stats.non_contiguous_split += 1; chunks.push(vec![i]); }
        }

        for chunk in chunks {
            // F11：超限强制拆成 max_rounds 块（保留 LLM title/detail，加 (续K) 后缀）
            if chunk.len() as u64 > max_rounds {
                stats.over_limit += 1;
                for (k, sub) in chunk.chunks(max_rounds as usize).enumerate() {
                    let suffix = if chunk.len() > max_rounds as usize { format!(" (续{})", k + 1) } else { String::new() };
                    out.push(merge_event(sub, mechanical, &g.title, &format!("{}{}", g.detail, suffix), &g.subject));
                }
            } else {
                out.push(merge_event(&chunk, mechanical, &g.title, &g.detail, &g.subject));
            }
        }
    }

    // 漏的回合 → 机械兜底
    for (i, &c) in covered.iter().enumerate() {
        if !c {
            stats.filled_by_mechanical += 1;
            let m = &mechanical[i];
            out.push(FinalEvent { round_idx: None, seq_a: m.seq_a, seq_b: m.seq_b, last_ts: m.last_ts,
                title: m.title.clone(), detail: m.detail.clone(), subject: m.subject.clone() });
        }
    }
    out.sort_by_key(|e| e.seq_a);
    (out, stats)
}
```

`merge_event(idxs, mechanical, title, detail, subject)` → `FinalEvent`：
- `seq_a = mechanical[idxs.first()].seq_a`（idxs 升序且连续，首元素 min）。
- `seq_b = mechanical[idxs.last()].seq_b`（末元素 max）。
- `last_ts = mechanical[idxs.last()].last_ts`。
- `round_idx = None`（最终事件）。
- `title/detail/subject` 用 LLM 的（覆盖机械底座）。

> **F6**：组内 round idx 必须连续；非连续拆成连续子组（各带 LLM title），保证 seq 并集指针 `[min_a, max_b]` drill 不含其它组回合。
> **F11 决策 A**：超 `max_rounds` 强制拆成 `max_rounds` 块，保留 LLM title/detail + `(续K)` 后缀。不丢回合、守上限、保语义。

### 5.7 tail_aware_b（新增，F7 移入 execute）

```rust
/// 留尾：从 cur 往回跳过 tail_rounds 个 user，返回应整理到的上界 b。
/// tail_rounds=0 → b=cur（不留尾）。user 数 ≤ tail → b<a（空段，调用方推进 marker 到 cur —— F12）。
fn tail_aware_b(events: &[HistoryEvent], cur: u64, tail_rounds: u64) -> u64 {
    if tail_rounds == 0 { return cur; }
    let mut seen = 0u64;
    let mut b = cur;
    for e in events.iter().rev() {
        if e.seq > cur { continue; }
        if e.kind == "user" {
            seen += 1;
            if seen > tail_rounds { break; }
        }
        b = e.seq.saturating_sub(1);
    }
    b
}
```

> 在 execute_dream 内调用，用 execute 的 read_all events + 该次 cur（F7 同源）。

### 5.8 execute_dream 重写（F2/F3/F4/F7/F8/F10/F12）

```rust
pub async fn execute_dream<E: Emitter>(
    round: Arc<dyn LlmRound>, cfg: &Config,
    events: &[HistoryEvent],            // F8：透传自 dispatch_dream（不再内部 read_all）
    history: &HistoryWriterHandle, history_dir: &Path, cache: &Path,
    a: u64, emit: &E,
) -> Result<(), String> {
    let cur = history.current_seq();
    let b = tail_aware_b(events, cur, cfg.dream_tail_rounds);   // F7：execute 内算

    // F12：空段（user ≤ tail 或无新事件）→ 推进 marker 到 cur，避免 idle tight-loop
    if b < a {
        let now = events.iter().filter(|e| e.seq <= cur).map(|e| e.ts).max().unwrap_or(cur);
        history.append(HistoryEvent::marker(now, "dream", cur));  // until_seq = cur（tail 永不整）
        return Ok(());
    }

    let segment: Vec<&HistoryEvent> = events.iter()
        .filter(|e| e.seq >= a && e.seq <= b && e.thread == "main" && e.kind != "marker")
        .collect();
    if segment.is_empty() { return Ok(()); }

    let last_ts = segment.last().expect("non-empty").ts;
    let offset = crate::history::local_offset_secs();

    // ②c/②d 回合切 + 机械底座
    let rounds = split_rounds(&segment);
    if rounds.is_empty() { return Ok(()); }
    let mechanical: Vec<FinalEvent> = rounds.iter().enumerate()
        .map(|(i, r)| mechanical_extract(i, r)).collect();

    // ②e 跨月检查（段末月；段跨月见 R2，按事件 ts 分组判）
    let seg_ym = memory::ym_from_ts(last_ts, offset);
    check_and_rotate_month(round.clone(), cfg, cache, &seg_ym, emit).await?;

    // ②f LLM 增强（失败 catch → 全机械）
    let (groups, bad_lines) = match round.round(&build_dream_messages(&rounds, a, b, cfg), cfg, emit).await {
        Ok(resp) => parse_groups(&resp.content),
        Err(e) => { eprintln!("[dream] LLM 失败，全机械兜底: {e}"); (vec![], 0) }
    };

    // ②g reconcile（F6 连续拆 + F11 超限拆）
    let (events_out, stats) = reconcile_groups(groups, &mechanical, cfg.dream_merge_max_rounds);
    eprintln!("[dream] 回合{} 组{} 坏行{} 漏补{} 重复{} 越界{} 超限{} 非连续拆{}",
        mechanical.len(), events_out.len(), bad_lines,
        stats.filled_by_mechanical, stats.duplicate, stats.out_of_range, stats.over_limit, stats.non_contiguous_split);

    // ②h 落盘：每个 FinalEvent 用自己的 last_ts（多 ts，修复段粒度同 ts）
    // F4：append_day_event 任一 Err → propagate（不写 marker，段重试 §13.1）
    for ev in &events_out {
        memory::append_day_event(cache, ev.last_ts, offset, &ev.title, &ev.detail,
            &ev.subject, (ev.seq_a, ev.seq_b), None)?;   // ← 不再 let _ =
    }

    // F10：evt_no 用 day_event_count 起算（与 day 文件 evt-NNN 同源）
    // MEMORY.md 日节：每个 FinalEvent 一行，evt_no = 该事件 last_ts 对应 day 文件已有段数 + 序号
    for (i, ev) in events_out.iter().enumerate() {
        let base = memory::day_event_count_pub(cache, ev.last_ts, offset); // 暴露 day_event_count
        let _ = memory::append_memory_day(cache, ev.last_ts, offset, base + i as u32 + 1, &ev.title);
    }

    // F2：收集所有 ev.last_ts 的 distinct 日期，每个日期 upsert（修复跨天漏刷）
    let mut seen_days: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for ev in &events_out {
        seen_days.insert(crate::history::date_from_ts_local(ev.last_ts, offset));
    }
    // 段末 ts 那天必须 upsert（即使 events_out 为空，也已 early return）
    seen_days.insert(crate::history::date_from_ts_local(last_ts, offset));
    for day_ts_raw in &seen_days {
        // upsert_current_month_today 需要 ts；用该日期首个事件的 ts 代表
        let rep_ts = events_out.iter()
            .find(|ev| crate::history::date_from_ts_local(ev.last_ts, offset) == *day_ts_raw)
            .map(|e| e.last_ts).unwrap_or(last_ts);
        let _ = memory::upsert_current_month_today(cache, rep_ts, offset);
    }

    // 写 dream marker（until_seq = b，留尾后；F1 build_messages 认 until_seq → tail 保留）
    history.append(HistoryEvent::marker(last_ts, "dream", b));
    Ok(())
}
```

> **F8**：execute_dream 接 `events: &[HistoryEvent]` 透传，删内部 `read_all`（原 `dream.rs:149`）。dispatch_dream 决策阶段 read_all 一次，全程透传。
> **F4**：`append_day_event` 不再 `let _ =`，Err 用 `?` propagate。`append_memory_day`/`upsert_current_month_today` 仍 `let _ =`（MEMORY.md 索引层失败不阻断 day 文件——day 文件是真相源）。
> **F10**：`day_event_count` 暴露为 `pub`（`day_event_count_pub`），evt_no 与 day 文件 evt-NNN 同源。
> **F2**：跨天时按 distinct 日期 upsert，避免 MEMORY.md 月节今日行漏刷。
> **F3**：`check_and_rotate_month` 仍在 extract 前（看段末月判定跨月）；段跨月 edge case 见 §11 R2。
> **F12**：`b < a`（user ≤ tail）空段，写 marker(until_seq=cur) 推进 frontier，避免 idle tight-loop。

### 5.9 dispatch_dream 改造（F8 锁模式 + 透传）

`agent.rs:301-346` dispatch_dream：决策阶段持锁 read_all → 把 events clone 或引用传入 prepare + execute。

**plan 必须明确**（review F8）：决策阶段 read_all 后，prepare（持锁同步）+ execute（spawn async）共享同一份 events。execute 是 async spawn，events 须 `clone()` 进 spawn（Vec\<HistoryEvent\>，cap=50 段约 200-400 event，clone 成本可接受）或 `Arc<Vec<HistoryEvent>>`。

### 5.10 配置项（改 `config.rs`，同 v1）

```rust
#[serde(default = "d_dream_tail_rounds")]
pub dream_tail_rounds: u64,
#[serde(default = "d_dream_merge_max_rounds")]
pub dream_merge_max_rounds: u64,
```
```rust
fn d_dream_tail_rounds() -> u64 { 3 }
fn d_dream_merge_max_rounds() -> u64 { 5 }
```

## 6. 数据格式变更

### 6.1 day 文件段格式（`memory/Y/M/date.md`）—— 不变
`append_day_event` 签名不变。**唯一语义变化**：`seq[a,b]` 对合并组是连续并集 `[min_a, max_b]`（F6 保证连续，drill 不含其它组）。

### 6.2 evt_no 同源（F10）
day 文件 evt-NNN（`day_event_count` 起）与 MEMORY.md 日节 evt_no（同源起算）对齐，多次 dream 同一天编号连续不重复。

### 6.3 多 ts（行为变化）
每个事件段用自己的组末 ts（`ev.last_ts`），不再共用段末 ts。

### 6.4 MEMORY.md 三层 —— 不变

## 7. 错误处理汇总

| 失败点 | 行为 | 不变量 |
|---|---|---|
| LLM 调用失败 | catch → `groups=[]` → 全机械兜底 | G1 G4 |
| LLM parse 空/全坏 | `groups=[]`（bad_lines 计数）→ 全机械 | G1 G4 |
| 部分回合未覆盖 | `reconcile_groups` 补漏 | G1 |
| 回合重复覆盖 | 去重记 `duplicate`，跳过 | G1 |
| `rounds` 越界 | `out_of_range` 跳过 | G1 |
| 单组超 `max_rounds`（F11） | **强制拆** max_rounds 块 + `(续K)` 后缀 | G3 |
| 组内非连续（F6） | **拆连续子组**，各带 LLM title | G3 |
| **`append_day_event` 失败（F4）** | **propagate Err**（`?`），不写 marker、不推进 frontier、段重试 | G1 G4 §13.1 |
| `append_memory_day`/`upsert` 失败 | `let _ =`（MEMORY.md 索引层失败不阻断 day 文件真相源） | 现状 |
| 跨月月主题 LLM 失败 | `?` propagate（不轮转、不写 marker、段保留重试） | §13.1 |
| **空段 user ≤ tail（F12）** | 写 marker(until_seq=cur) 推进 frontier，返回 Ok | 避免 idle tight-loop |

## 8. 测试矩阵

### 8.0 Test migration（F5 — review 强制新增子节）

现有 7 个 `run_dream_*`/`execute_dream_*` 测试（`dream.rs:457-739`）必须迁移：
- **`ScriptedDream.content` shape 改**：从旧 `{"title","detail","subject"}` → 新 `{"rounds":[1,2,...],"title","detail","subject"}`，否则 `parse_groups` 全判 `rounds.is_empty()` → 所有测试静默退化为只测 mechanical 分支。
- **`execute_dream` 签名改**：新增 `events: &[HistoryEvent]` 参数；`a`/`b` 参数合并为 `a`（b 内部算）。所有调用点（`dream.rs:647/668/696/730`）更新。
- **seq assert 改**：`dream.rs:476 assert!(mem.contains("seq[1,3]"))` 不再成立（seq 是 per-event 回合并集），按新逻辑改。
- **新增 LLM-success 断言**：集成测试里至少一个断言 `stats.filled_by_mechanical == 0`（证明 LLM 分支被覆盖，不是全退机械）。

### 8.1 纯逻辑单测（FakeRound / ScriptedDream）
- `split_rounds`：单/多回合/连续 user/tool_result 归属/段首非 user 前导。
- `mechanical_extract`：title 截断去标点 / detail 拼接 / assistant 空"(调用工具)" / user 空"（无文本）" / Unicode 安全。
- `tail_aware_b`：tail=0 不留尾 / tail=3 跳过 3 user / user≤tail 返回 <a / 未来事件不数。
- `parse_groups`：正常多行 / 坏行计数 / rounds 缺失跳过 / title 空。
- `reconcile_groups` 全覆盖 / 部分覆盖（机械补）/ 重复 / 越界。
- **`reconcile_groups` 超限（F11）**：单组 6 回合（max=5）→ 拆成 [1-5]+[6]，2 个 FinalEvent，title 含 `(续1)`。
- **`reconcile_groups` 非连续（F6）**：rounds=[1,5,9] → 拆 3 个连续子组，各带 LLM title，`non_contiguous_split=2`（2 次非连续跳）。
- seq 指针并集（连续）：[R1(seq1-2),R2(seq3-5)] → FinalEvent seq=[1,5]。

### 8.2 build_messages 单测（F1 P0 回归）
- **`tail_events_survive_marker_in_next_build_messages`**：dream 跑后构造 tail 事件（seq ∈ (b, marker.seq)）+ marker(until_seq=b)，断言 `build_messages` 输出含 tail。
- **`dream_during_events_survive`**：dream 期间新事件（seq > marker.seq 前）+ marker(until_seq=b)，断言可见（顺带修的现状 bug）。
- **回归**：旧 case（marker.seq == until_seq）行为不变。

### 8.3 execute_dream 集成测
- LLM 全覆盖（n 回合合法分组）→ day 段数 = 组数，`filled_by_mechanical == 0`。
- LLM 部分覆盖 → 漏回合机械底座落盘。
- LLM 失败兜底（`ScriptedDream{fail:true}`）→ 全回合一回一条机械，marker 写。
- LLM parse 空 → 全机械。
- **留尾**：tail_rounds=2 → marker until_seq = cur−2，最近 2 回合不进 day 文件，但 `build_messages` 下轮可见（配 8.2）。
- **合并组 drill（F6）**：连续合并 seq 并集 → drill 还原整组；非连续拆子组 drill 各自范围。
- **多 ts**：同次 dream 多段时间戳不同。
- **F4 propagate**：mock append_day_event 失败 → execute 返 Err、不写 marker、frontier 不推进。
- **F12 空段**：user ≤ tail → marker(until_seq=cur) 写、frontier 推进、day 文件不写、无 tight-loop。
- **F2 跨天 upsert**：段跨 23:50→00:10 → 两天的 MEMORY.md 月节今日行都刷新。
- 回归：跨月轮转（DualDream）/ 空段 / 失败不推进 frontier / dream marker until_seq=b。

### 8.4 配置测
- 默认 `dream_tail_rounds=3`、`dream_merge_max_rounds=5`。
- `tail_rounds=0` → 留尾关闭（b=cur）。

## 9. 改动清单

| 文件 | 改动 |
|---|---|
| `src-tauri/src/dream.rs` | `prepare_dream` 只接 a（不算 b）；新增 `tail_aware_b`（§5.7）/ `split_rounds`+`Round`（§5.3）/ `mechanical_extract`（§5.4，F9 合并 FinalEvent）/ `parse_groups`+`Group`（§5.6）/ `reconcile_groups`+`ReconcileStats`+`merge_event`（§5.6，F6+F11）/ `build_dream_messages`；改 `DREAM_SYS`+`dream_prompt`（§5.5，F13）；重写 `execute_dream`（§5.8，F2/F3/F4/F7/F8/F10/F12）；删旧 `dream_prompt`/`parse_extracts`。 |
| **`src-tauri/src/context.rs`** | **F1 P0**：`last_marker_seq` 改读 `data["until_seq"]`（§5.2）。 |
| `src-tauri/src/config.rs` | 加 `dream_tail_rounds`/`dream_merge_max_rounds` 字段 + 默认函数。 |
| `src-tauri/src/agent.rs` | `dispatch_dream`（agent.rs:301-346）：决策阶段 read_all 的 events 透传给 prepare+execute（F8 锁模式 + Arc/clone）；execute 签名接 events。 |
| `src-tauri/src/memory.rs` | `day_event_count` 暴露为 `pub`（`day_event_count_pub`，F10 evt_no 同源）。其余不改。 |
| `src-tauri/defaults/AGENT.md` | 更新 dream 说明：新行为"回合制 + 每回合必出 + 留尾 + build_messages 认 until_seq"。 |
| `docs/dream-flow.md` | 更新流程图 + 提示词原文（新 DREAM_SYS/dream_prompt）。 |
| `src-tauri/src/mem_cli.rs` | **不改**（读端契约不变，F6 连续保证 seq 并集 drill 不变）。 |

## 10. 兼容性

- **旧 day 文件 / MEMORY.md**：原样保留（N5）。
- **新 dream 产出**：day 文件段格式不变，mem 解析零改（F6 连续保证 seq 并集语义不变）。
- **F1 build_messages 改造向后兼容**：无 tail 时 `marker.seq == until_seq`，行为不变。
- **配置向后兼容**：新字段有 serde default，旧 config.json 无需迁移。
- **触发策略不变**：idle 2h / cap 50 / armed / in_flight 全不动。

## 11. 风险

- **R1 LLM 不听"全覆盖/连续"指令**：缓解——程序化覆盖校验 + 机械兜底是硬保证；非连续强制拆（F6）；最坏退化为全机械（仍完整）。
- **R2 段跨月（F3，review 扩展）**：段本身跨月（上月最后一天 → 本月）时，`seg_ym`（段末月）只判本月。**缓解**：①`check_and_rotate_month` 在 append 前跑，本月轮转后 per-event ts 在上月的 event append 到上月 day 文件——上月已轮转为"上月冻结"，今日行 stale、月主题漏见。**plan 决策**：段跨月时按事件 ts 分两组分别判跨月 + 分别 rotate；或加前置断言"段不跨月"（cap=50 + 单次 dream 几秒，跨月极罕见，断言 + 日志可接受）。**推荐**：plan 先加跨月检测 + 日志，实测频次后再决定是否分组判。
- **R3 空段（F12 重写）**：user ≤ tail → `tail_aware_b < a` → 写 marker(until_seq=cur) 推进 frontier，返回 Ok（tail 永不被 dream）。符合"停说 2h 就该整"的用户预期。避免 v1 的 idle tight-loop（空段不推进 → 每 tick 重跑）。
- **R4 单次 LLM 调用超长段**：cap=50 user ≈ 50 回合。**F13 缓解**：prompt 去机械底座行，每回合 ~150 字 → 50 回合约 7.5K 字，M3 context 够。本次不分批（N2）；若实测 parse 失败率 >10%，后续加 chunked 分批（留扩展点）。

## 12. 验证

1. **编译 + 单测**（dev server 锁 exe，[[ovoice-dev-server-cargo-lock]]）：
   ```bash
   cargo check --tests --manifest-path src-tauri/Cargo.toml
   cargo test --lib --manifest-path src-tauri/Cargo.toml dream::tests context::tests
   ```
2. **手测**（dev server，右键 Reload）：
   - 攒 ≥10 回合 → 触发 dream（或临时 `dream_cap_turns=5`）→ day 文件每回合都有段，合并组 seq 指针是连续并集。
   - 故意让 LLM 失败（断网/假 key）→ dream 落盘全机械，marker 写。
   - 留尾：最近 3 回合不在 day 文件，但下轮对话仍记得（build_messages 认 until_seq → tail 可见，**F1 核心**）。
   - `[dream]` 日志统计合理。
3. **F1 drill 验证**：`mem index` 覆盖所有回合；`mem history --seq <连续合并组 min_a>..<max_b>` 还原整组（不含其它组）。
4. **F4 IO 失败**：mock 磁盘错误 → dream 不写 marker、frontier 不推进、下次重试。

## 13. 约束

- dev server 锁 exe → 只 `cargo check --tests` / `cargo test --lib`。
- 分支 `feat/dream-completeness`（基于 master），严禁直接落 master。
- 数据保留（D2）：day 文件 / MEMORY.md append-only，新逻辑只增不删旧。
- 不改 mem 读端契约（[[ovoice-live-vs-history-render-paths]] 精神：写读分离）。
- F1 改 `build_messages`（主对话历史路径）—— 影响面大，必须配 P0 回归测试（§8.2）+ 充分手测。
- 提交信息以 `Co-Authored-By: Claude <noreply@anthropic.com>` 结尾。
