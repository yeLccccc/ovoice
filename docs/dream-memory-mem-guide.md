# dream 记忆系统 · MEMORY.md 更新 · mem CLI 完整指南

> **最详细版**（dream 记忆子系统唯一权威文档）。把 dream 的**完整提取流程**、`MEMORY.md` 三层的**逐字段更新过程**、以及 **mem CLI** 全部收进一个文件，含完整性改造（tail 留尾 / merge 上限 / 机械底座 / reconcile）、F10 evt_no 同源修复、月层轮转三步详解。
>
> 所有代码引用**逐字核对源码**并标 `file:line`，基于当前工作区（`feat/dream-completeness` + F10 修复，源码即本文锚点）。

---

## 目录

- [0. 全景：写端 / 读端 / 真相之源](#0-全景写端--读端--真相之源)
- [一、dream 完整流程](#一dream-完整流程)
  - [1.1 触发状态机 DreamTrigger](#11-触发状态机-dreamtrigger)
  - [1.2 驱动接线（agent.rs）](#12-驱动接线agentrs)
  - [1.3 execute_dream 十步](#13-execute_dream-十步逐行)
  - [1.4 配置项（4 个 dream_*）](#14-配置项4-个-dream_)
  - [1.5 LLM 提示词（4 段原文）](#15-llm-提示词4-段原文)
  - [1.6 月层轮转（check_and_rotate_month）](#16-月层轮转check_and_rotate_month)
  - [1.7 关键不变量（F1–F13 / G1/G4 / P1）](#17-关键不变量f1f13--g1g4--p1)
- [二、MEMORY.md 更新过程（逐字段）](#二memorymd-更新过程逐字段)
  - [2.1 三层骨架 ensure_memory_skeleton](#21-三层骨架-ensure_memory_skeleton)
  - [2.2 日层 day 文件 append_day_event](#22-日层-day-文件-append_day_event)
  - [2.3 MEMORY.md 日层 append_memory_day](#23-memorymd-日层-append_memory_day)
  - [2.4 月层今日行 upsert_current_month_today](#24-月层今日行-upsert_current_month_today)
  - [2.5 跨天 F2 distinct date](#25-跨天-f2-distinct-date)
  - [2.6 月层轮转 rotate_month](#26-月层轮转-rotate_month)
  - [2.7 年层 append_year_theme](#27-年层-append_year_theme)
  - [2.8 .dream-meta.json](#28-dream-metajson)
  - [2.9 完整跑前/跑后示例](#29-完整跑前跑后示例)
- [三、mem CLI](#三mem-cli)
  - [3.1 本质：文件树即索引](#31-本质文件树即索引)
  - [3.2 cache 定位（5 档）](#32-cache-定位5-档)
  - [3.3 调用方式](#33-调用方式)
  - [3.4 五个子命令详解](#34-五个子命令详解)
  - [3.5 drill 路径闭环](#35-drill-路径闭环)
- [四、数据流总图](#四数据流总图dream-写--mem-读--history-真相)
- [五、源码索引（全表）](#五源码索引全表)

---

## 0. 全景：写端 / 读端 / 真相之源

ovoice 的记忆子系统由**三个角色**组成，通过文件系统格式解耦、互不直接调用：

| 角色 | 是谁 | 职责 | 写？ |
|---|---|---|---|
| **真相之源** | `history/{date}.jsonl` | 每条对话事件 append-only 落盘（user / assistant / tool_result / external / subagent_result / marker） | agent.rs / llm.rs 落盘 |
| **写端（整理）** | `dream` 子系统 | 后台静默把一段 history 提炼成结构化记忆，写进 `memory/年/月/日.md` + `MEMORY.md` | dream 唯一写 memory/ |
| **读端（回忆）** | `mem` CLI | 零 LLM 纯只读，任何粒度把记忆读出来 drill | 不写任何状态 |

**文件布局**（`cache/` 根下）：

```
cache/                              ← 默认 %APPDATA%/com.ovoice.app/cache（可配）
├── MEMORY.md                       ← 三层记忆索引（dream 维护，mem 读）
├── memory/
│   ├── .dream-meta.json            ← { "current_month": "2026-07" } 跨月判定依据
│   └── {年}/
│       └── {月}/
│           └── {年}-{月}-{日}.md   ← 日层：每事件一段 + seq 指针（dream 写）
└── history/
    └── {年}-{月}-{日}.jsonl        ← 原始对话逐行 HistoryEvent（真相之源）
```

**核心铁律**：
- **P1 铁律③**：事件段的「对话索引」`seq[a,b]` 由**代码盖戳**，不让 LLM 决定边界（避免它乱标 seq）。
- **P1 铁律④**：dream 的 cap 触发只数 `kind=user`，external/subagent_result 不顶 cap。
- **dream 是 memory/ 唯一写者**（`memory.rs:2` 注释），mem 只读——零竞态，无需锁。
- **时间一律本地**：`offset_secs` 由 dream 传 `local_offset_secs()`，必须与 history writer 同源（否则 memory 文件日期与 history jsonl 文件名日期不一致，seq 指针会指向错日期文件而断裂）。

---

## 一、dream 完整流程

### 1.1 触发状态机 DreamTrigger

`DreamTrigger`（`dream.rs:20-25`）是 dream 的触发状态，owned by driver task，与 60s idle ticker 共享同一 `Arc<Mutex>`：

```rust
pub struct DreamTrigger {
    pub last_user_activity: u64,    // 距此算 idle（user/external 活动的最新 ts）
    pub last_dream_marker_seq: u64, // dream 提取前沿（只认 dream marker；reset 不动它）
    pub in_flight: bool,            // 单飞：dream 跑着时新触发 noop
    pub armed: bool,                // 首次用户活动后才 arm（修「开机触发」）
}
```

**4 个方法**（`dream.rs:33-107`）：

| 方法 | 行 | 作用 |
|---|---|---|
| `note_activity(ts)` | `dream.rs:44-49` | 记一次用户活动（user 消息 / external 拖入），置 `armed=true` + 更新 `last_user_activity` |
| `seed_from_history(events)` | `dream.rs:52-61` | 启动时播种提取前沿 = 最后 dream marker 的 seq；**不 armed**（仍需首次活动才触发） |
| `dream_started(marker_seq)` / `dream_finished()` | `dream.rs:63-71` | 单飞锁置位/清位（注：`dream_started` 推进 frontier 仅测试用，生产路径用 `prepare_dream` + 成功后手写 frontier） |
| `check(events, now, idle_secs, cap)` | `dream.rs:74-107` | 触发判定，返回 `Option<TriggerReason>` |

#### `check()` 判定顺序（cap 优先于 idle，`dream.rs:74-107`）

```
1. !armed || in_flight        → None（跳过）   ← armed 门防开机触发；in_flight 单飞
2. since 段为空               → None（跳过）   ← last_dream_marker_seq 之后无事件
3. user_count ≥ cap(50)       → Some(Cap)      ← cap 优先（P1 铁律④：只数 kind=user）
4. now - last_user_activity
   ≥ idle_secs(7200)*1000 ms  → Some(Idle)
5. 否则                       → None（跳过）
```

**关键边界**（`dream.rs:84-94`）：`last_dream_marker_seq == 0`（尚未 dream 过）时，`seq=0` 的首条事件也算「自上次 dream 后」——若用 `e.seq > 0` 过滤会错误排除第一条 user 事件，导致 idle/cap 都少算 1。故 `== 0` 时取全部事件，`> 0` 时才 `filter(e.seq > last_dream_marker_seq)`。

> **注意区分三个时间常量**：**60 秒** = ticker 检查间隔（`agent.rs:172`）；**7200 秒** = idle 触发阈值（`dream_idle_secs`）；**50** = cap 触发的 user 事件数（`dream_cap_turns`）。

### 1.2 驱动接线（agent.rs）

dream 不是独立进程，是 driver task 内的异步分支。整条驱动链路：

#### (a) spawn_session 启动（`agent.rs:135-221`）

```rust
// 1. dream 触发状态：driver 单持
let trigger = Arc::new(Mutex::new({
    let mut t = DreamTrigger::new();
    t.seed_from_history(&read_all(&history_dir_rc));  // 播种前沿，不 armed
    t
}));
// 2. idle ticker：每 60s 投 DreamCheck（agent.rs:166-176）
tauri::async_runtime::spawn(async move {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let _ = tx2.send(SessionEvent::DreamCheck).await;  // 不入 history、不跑 turn
    }
});
// 3. driver 主循环 select（agent.rs:208-218）
loop {
    tokio::select! {
        Some(o) = job_done_rx.recv() => run_one(&JobDone(o), .., &trigger).await,
        ev = rx.recv() => match ev {
            Some(e) => run_one(&e, .., &trigger).await,  // DreamCheck 也从这进
            None => break,
        }
    }
}
```

`SessionEvent::DreamCheck`（`agent.rs:16-17`）是 driver **内部信号**：60s ticker 投递，**不入 history、不跑 turn**，只触发 dream 判定。

#### (b) run_one 路由（`agent.rs:229-289`）

```rust
async fn run_one(e: &SessionEvent, .., trigger: &Arc<Mutex<DreamTrigger>>) {
    // 用户活动 → 记一笔（idle 时钟 + armed 门）
    if matches!(e, UserMessage{..} | ContextNote{..}) {
        trigger.lock().unwrap().note_activity(now_ms());
    }
    // DreamCheck → 直接 dispatch_dream，return（不进 handle_event）
    if matches!(e, DreamCheck) {
        dispatch_dream(trigger, history, history_dir, cache, cfg).await;
        return;
    }
    // 其它事件 → handle_event 落 history + run_turn（agent.rs:272）
    let _ = handle_event(e, ..).await;
    // 轮间 cap 触发：UserMessage / JobDone 在 turn 后再检查（dream 异步后台、不打断当前 turn）
    if matches!(e, UserMessage{..} | JobDone(_)) {
        dispatch_dream(trigger, history, history_dir, cache, cfg).await;
    }
}
```

**两个 dispatch 点**：① DreamCheck（idle 路径，60s 一查）；② UserMessage / JobDone turn 后（cap 路径，攒够就跑）。两者都串行经 driver select，不并发 dream。

#### (c) dispatch_dream 三阶段（`agent.rs:301-346`）

```rust
async fn dispatch_dream(trigger, history, history_dir, cache, cfg) {
    // ① 决策（持锁）：check 自带 armed/in_flight 门。read_all 不需锁（只读）。F8：events 全程透传。
    let evs = read_all(history_dir);                    // 单次读，hoisted out
    let decision = { let t = trigger.lock().unwrap();
        t.check(&evs, now, cfg.dream_idle_secs, cfg.dream_cap_turns) };
    if decision.is_none() { return; }

    // ② 占位（持锁）：set in_flight + 捕获 a，【不推进 frontier】（F7：b 移入 execute）
    let prepared = { let mut t = trigger.lock().unwrap(); prepare_dream(&mut t) };
    let Some(a) = prepared else { return; };

    // ③ spawn 异步执行（不持锁跨 await）；成功才推进 frontier，再 finish
    tauri::async_runtime::spawn(async move {
        let round = Arc::new(HttpRound);
        let emit = DreamSilentEmitter;                  // §静默，不推前端
        let r = execute_dream(round, &cfg2, &evs, &history2, &hd2, &cache2, a, &emit).await;
        { let mut t = trigger2.lock().unwrap();
          if let Ok(b) = r { t.last_dream_marker_seq = b; }  // 成功才推进（失败段保留重试）
          t.dream_finished(); }
        if let Err(e) = r { eprintln!("[dream] 失败（不推进 marker，下次 idle/cap 自动重试）: {e}"); }
    });
}
```

**3 个正确性要点**（`agent.rs:294-300` 注释）：
1. `check` 跑在 `prepare` **之前**（check 自身 guard on `in_flight`；若 prepare 先置 `in_flight`，check 永 None）。
2. `prepare_dream`（`dream.rs:116-121`）只置 `in_flight` + 捕获 `a`，**不调 `dream_started`**（dream_started 推进 frontier，会让失败段被跳过）。
3. `execute_dream` 不触碰 trigger；spawned task **成功后**才手写 `last_dream_marker_seq = b`，失败保留段待重试（§13.1）。

#### (d) DreamSilentEmitter（`agent.rs:349-359`）——完全静默

```rust
struct DreamSilentEmitter;
#[async_trait]
impl llm::Emitter for DreamSilentEmitter {
    async fn thinking(&self, _: &str) {}
    async fn content(&self, t: &str) { eprintln!("[dream] {t}"); }   // 只 stderr，不推前端
    async fn tool_call(&self, _: &str, _: &str) {}
    async fn tool_result(&self, _: &str, _: &str) {}
    async fn turn_start(&self) {}
    async fn turn_end(&self) {}
    async fn error(&self, t: &str) { eprintln!("[dream] err {t}"); }
}
```

dream marker（`kind="marker"`）也被前端 `buildHistoryBubbles` 跳过，不渲染气泡。**用户全程无感知**。

### 1.3 execute_dream 十步（逐行）

`execute_dream`（`dream.rs:126-204`）是核心。签名：

```rust
pub async fn execute_dream<E: Emitter>(
    round: Arc<dyn LlmRound>, cfg: &Config, events: &[HistoryEvent],
    history: &HistoryWriterHandle, _history_dir: &Path, cache: &Path,
    a: u64, emit: &E,
) -> Result<u64, String>   // Ok(b) = marker until_seq
```

> **F8**：`events` 由 dispatch 透传（`agent.rs:310` 单次 `read_all`），execute 内部**不再 read_all**——避免 await 漂移（dream 进行中新落盘事件不应被本次吞掉）。

| 步 | 代码 | 做什么 |
|---|---|---|
| **1** | `dream.rs:137-139` | `cur` = events 里 main 非 marker 的最大 seq（透传同源，不调 `current_seq()` 避免 await 漂移） |
| **2** | `dream.rs:140` | `b = tail_aware_b(events, cur, cfg.dream_tail_rounds)` —— 留尾，最近 N 个 user 回合不进记忆 |
| **3** | `dream.rs:143-147` | **F12 空段**：`b < a`（user ≤ tail 或无新事件）→ 写 `marker(now, "dream", cur)` 推进 frontier，**不写 day**，返回 `Ok(cur)`（避免 idle tight-loop） |
| **4** | `dream.rs:149-151` | 段过滤：`seq ∈ [a,b] ∧ thread="main" ∧ kind ≠ "marker"` |
| **5** | `dream.rs:158-161` | `split_rounds(&segment)` 按 user 边界切回合；空 → `Ok(b)` |
| **6** | `dream.rs:162-163` | `mechanical_extract` 每回合一机械条（G1 完整性兜底，覆盖 truth） |
| **7** | `dream.rs:166-167` | `check_and_rotate_month` 跨月检查（段末月 vs meta.current_month） |
| **8** | `dream.rs:170-173` | LLM `round(build_dream_messages)` → `parse_groups`；**失败 catch → 全机械兜底**（`(vec![], 0)`） |
| **9** | `dream.rs:175-178` | `reconcile_groups(groups, &mechanical, max_rounds)` → 最终事件 + stats；eprintln 统计 |
| **10** | `dream.rs:183-203` | **落盘**（F10 一轮循环 + F2 distinct date upsert）+ 写 `marker(last_ts, "dream", b)` → `Ok(b)` |

#### 步 2 详解：tail_aware_b（`dream.rs:364-382`）—— 留尾

```rust
fn tail_aware_b(events: &[HistoryEvent], cur: u64, tail_rounds: u64) -> u64 {
    if tail_rounds == 0 { return cur; }        // 不留尾
    let mut seen = 0u64; let mut b = cur;
    for e in events.iter().rev() {              // 从 cur 往回
        if e.seq > cur { continue; }            // 未来事件不数（dream 期间新事件归下一段）
        if e.kind == "user" {
            seen += 1;
            if seen > tail_rounds { break; }    // 跳过最近 tail_rounds 个 user
            b = e.seq.saturating_sub(1);        // b = 该 user 前一格
        }
    }
    if seen < tail_rounds { return 0; }         // user 数 ≤ tail → 空段信号（b<a）
    b
}
```

- `tail_rounds=0` → `b=cur`（不留尾，提取到 cur）。
- 正常：跳过最近 `tail_rounds` 个 user，`b` 落在第 `tail_rounds+1` 个 user 之前。
- `seen < tail_rounds` → 全部 user 都在留尾窗口内 → 返回 `0`（< 任何合理 `a`）→ 触发步 3 空段。

**为什么留尾**：让当前进行中的对话在下轮 `build_messages` 仍以原文可见（最近 3 个回合不进记忆，但 history 里原文还在）。详见 [F1](#17-关键不变量f1f13--g1g4--p1)。

#### 步 5 详解：split_rounds（`dream.rs:268-292`）—— 按回合切

一个回合 = 一个 `user` + 后续 `assistant` / `tool_result` / `external` / `subagent_result`（到下个 user 前）。`Round` 结构（`dream.rs:260-265`）：

```rust
struct Round<'a> {
    seq_a: u64, seq_b: u64, last_ts: u64,
    user_text: &'a str,
    assistant: Vec<&'a HistoryEvent>,
    tools: Vec<&'a HistoryEvent>,
}
```

切分规则（`dream.rs:271-289`）：
- 遇 `user` → 关闭当前回合（若有），开新回合（`seq_a = user.seq`）。
- 遇 `assistant` → 推入当前回合的 `assistant`，更新 `seq_b` / `last_ts`。
- 遇 `tool_result` / `external` / `subagent_result` → 推入 `tools`，更新 `seq_b` / `last_ts`。
- **段首非 user 前导事件**（理论边界）：挂到下一个 user 回合（`split_rounds_leading_non_user_attaches_to_next_user:722` 测）。

#### 步 6 详解：mechanical_extract（`dream.rs:341-360`）—— 机械底座

每回合产出一个程序化真相的 `FinalEvent`（G1 完整性兜底）。即使 LLM 全失败，机械底座保证每回合一记忆条：

```rust
fn mechanical_extract(idx: usize, r: &Round) -> FinalEvent {
    let user_clean = collapse_ws(r.user_text);                 // 折叠空白
    let title = truncate_title(&user_clean, 30);               // 截 30 char + 去末尾标点
    let asst_first = r.assistant.first()
        .and_then(|a| a.data.get("content").and_then(|v| v.as_str()))
        .map(truncate_first_sentence)                          // 首句或前 80 字
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| if has_tool_calls(r) { "(调用工具)".into() } else { String::new() });
    let detail = {
        let u = truncate_chars(&user_clean, 100);
        if asst_first.is_empty() { u } else { format!("{} → {}", u, truncate_chars(&asst_first, 80)) }
    };
    FinalEvent {
        round_idx: Some(idx),                                  // Some=机械底座（reconcile 引用）
        seq_a: r.seq_a, seq_b: r.seq_b, last_ts: r.last_ts,
        title: if title.is_empty() { "（无文本）".into() } else { title },
        detail: truncate_chars(&detail, 200),
        subject: "用户".into(),
    }
}
```

辅助函数：`collapse_ws`（`dream.rs:303-305` 折叠空白）、`truncate_chars`（`dream.rs:308-312` Unicode 安全截断，不切多字节字符中段）、`truncate_title`（`dream.rs:315-321` 截 + 去末尾 `。，！？、,.!?`）、`truncate_first_sentence`（`dream.rs:324-334` 首句或前 80 字）、`has_tool_calls`（`dream.rs:336-338`）。

#### 步 8 详解：LLM 调用 + parse_groups

`build_dream_messages`（`dream.rs:500-520`）构造 `[system, user]` 两条消息（提示词原文见 [1.5](#15-llm-提示词4-段原文)）。`round.round()` 真实调 MiniMax M3。

`parse_groups`（`dream.rs:399-421`）逐行解析 LLM 输出的 JSON：

```rust
fn parse_groups(content: &str) -> (Vec<Group>, u32) {
    let mut groups = Vec::new(); let mut bad_lines = 0u32;
    for line in content.lines() {
        let l = line.trim().trim_start_matches("```json").trim_start_matches("```").trim();  // 剥 code fence
        if l.is_empty() || !l.starts_with('{') { continue; }
        match serde_json::from_str::<Value>(l) {
            Ok(v) => {
                let rounds = v.get("rounds").and_then(|x| x.as_array())
                    .map(|a| a.iter().filter_map(|n| n.as_u64().map(|n| n as usize)).collect()).unwrap_or_default();
                let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if rounds.is_empty() || title.is_empty() { bad_lines += 1; continue; }       // 坏行计数
                groups.push(Group { rounds, title,
                    detail: v.get("detail").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    subject: v.get("subject").and_then(|x| x.as_str()).unwrap_or("用户").to_string() });
            }
            Err(_) => { bad_lines += 1; }    // 坏行计数（不静默丢）
        }
    }
    (groups, bad_lines)
}
```

**LLM 失败 catch**（`dream.rs:170-173`）：`round()` 返 `Err` → `(vec![], 0)`（空 groups + 0 坏行）→ reconcile 全机械兜底（每回合一机械条），**仍写 marker 推进 frontier**（G4 不阻塞）。

#### 步 9 详解：reconcile_groups（`dream.rs:440-497`）

把 LLM 分组 + 机械底座 reconcile 成最终事件。3 条硬规则：

```rust
fn reconcile_groups(groups: Vec<Group>, mechanical: &[FinalEvent], max_rounds: u64)
    -> (Vec<FinalEvent>, ReconcileStats)
{
    let n = mechanical.len();
    let mut covered = vec![false; n];   // G1 全覆盖追踪
    let mut out = Vec::new();
    for g in groups {
        // 1-based → 0-based；越界/重复标记
        let mut idxs: Vec<usize> = Vec::new();
        for r in &g.rounds {
            if *r == 0 || *r > n { stats.out_of_range += 1; continue; }  // 越界
            let i = r - 1;
            if covered[i] { stats.duplicate += 1; continue; }            // 重复（首次算）
            covered[i] = true; idxs.push(i);
        }
        if idxs.is_empty() { continue; }

        // F6：非连续拆子组（idxs 升序后检查相邻是否连续）
        idxs.sort();
        let mut chunks: Vec<Vec<usize>> = vec![vec![idxs[0]]];
        for &i in &idxs[1..] {
            let last = *chunks.last().unwrap().last().unwrap();
            if i == last + 1 { chunks.last_mut().unwrap().push(i); }
            else { stats.non_contiguous_split += 1; chunks.push(vec![i]); }  // 非连续 → 新子组
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
            out.push(FinalEvent { round_idx: None, seq_a: m.seq_a, seq_b: m.seq_b, last_ts: m.last_ts,
                title: m.title.clone(), detail: m.detail.clone(), subject: m.subject.clone() });
        }
    }
    out.sort_by_key(|e| e.seq_a);
    (out, stats)
}
```

`merge_event`（`dream.rs:425-437`）：把若干机械底座合并成一个 FinalEvent，用 LLM 的 title/detail/subject 覆盖；`seq_a = 首元素`，`seq_b = 末元素`（连续 → 并集 `[min,max]`）。

`ReconcileStats`（`dream.rs:390-396`）：`filled_by_mechanical` / `duplicate` / `out_of_range` / `over_limit` / `non_contiguous_split`，全部 eprintln 输出（`dream.rs:176-178`）便于排查。

#### 步 10 详解：落盘（F10 一轮循环 + F2 distinct date）

```rust
// F10：一轮循环——append_day_event 返回它算的 nn（写前 day 段数 + 1），append_memory_day 复用同一 nn。
//   旧两轮循环里 MEMORY evt_no（写后计数）比 day evt_no（写前）大 events_out.len() → 错位 bug，已修。
for ev in &events_out {
    let nn = memory::append_day_event(cache, ev.last_ts, offset, &ev.title, &ev.detail,
        &ev.subject, (ev.seq_a, ev.seq_b), None)?;        // F4：任一 Err → propagate（不写 marker）
    let _ = memory::append_memory_day(cache, ev.last_ts, offset, nn, &ev.title);
}
// F2：distinct 日期 upsert（修复跨天漏刷）
let mut seen_days: BTreeSet<String> = BTreeSet::new();
for ev in &events_out {
    seen_days.insert(date_from_ts_local(ev.last_ts, offset));
}
seen_days.insert(date_from_ts_local(last_ts, offset));
for day_str in &seen_days {
    let rep_ts = events_out.iter()
        .find(|ev| date_from_ts_local(ev.last_ts, offset) == *day_str)
        .map(|e| e.last_ts).unwrap_or(last_ts);
    let _ = memory::upsert_current_month_today(cache, rep_ts, offset);
}
// 写 dream marker（until_seq = b）；F1 build_messages 认 until_seq → tail 保留
history.append(HistoryEvent::marker(last_ts, "dream", b));
Ok(b)
```

**F10 evt_no 同源**（最近修复 `0ab465e`）：旧实现两轮循环——第一轮 `append_day_event`（写前计数，day 写 001/002），第二轮 `append_memory_day` 用写后计数（MEMORY 写 003/004），差 `events_out.len()`。修法：合并成一轮，`append_day_event` 返回它算的 `nn`，`append_memory_day` 复用同一 `nn`。回归测试 `execute_dream_day_and_memory_evt_no_aligned_f10`（`dream.rs:1362`）。

### 1.4 配置项（4 个 dream_*）

`config.rs:89-113`（字段 + 默认函数）：

| 字段 | 默认值 | 字段定义 | 默认函数 | 含义 |
|---|---|---|---|---|
| `dream_idle_secs` | **7200**（2 小时） | `config.rs:90-91` | `d_dream_idle()` `config.rs:108` | 距上次用户活动多久后 idle 触发 |
| `dream_cap_turns` | **50** | `config.rs:92-93` | `d_dream_cap()` `config.rs:109` | 自上次 dream marker 后攒多少条 `kind=user` 触发 cap（external / subagent_result **不**计数） |
| `dream_tail_rounds` | **3** | `config.rs:101-102` | `d_dream_tail_rounds()` `config.rs:112` | 最近 N 个 user 回合不进 dream（下轮 build_messages 仍可见） |
| `dream_merge_max_rounds` | **5** | `config.rs:104-105` | `d_dream_merge_max_rounds()` `config.rs:113` | 单个 LLM 分组最多覆盖多少回合；超出强制拆（F11） |

> **`dream_cap_turns` 双重身份**：既是 dream 的 cap 触发阈值（`dream.rs:100`），又作为 `build_messages` 的历史截断窗口（`agent.rs:109` `cfg.dream_cap_turns as usize`，限制发给主 LLM 的历史保留最近多少条 user 事件）。

> `display_window_size`（50）是普通对话历史显示窗口，与 dream 触发无直接关系。

### 1.5 LLM 提示词（4 段原文）

dream 共 **2 个 LLM 调用点**，每个 `[system, user]` 两条消息，共 **4 段提示词**。

#### 提示词 ① —— 日层提取 · system（`DREAM_SYS`，`dream.rs:112`）

```rust
const DREAM_SYS: &str = "你是 dream，负责把一段对话整理成「日」层记忆。输入按回合编号给出。请把属于同一事件的回合合并成一个分组，对每个分组输出一行 JSON：{\"rounds\":[回合编号...],\"title\":\"联想向简短标题\",\"detail\":\"一句话详情\",\"subject\":\"主语(用户/agent/子代理#N)\"}。硬性要求：①每个回合编号必须恰好被一个分组覆盖，不许漏、不许重复（回合编号 1..N 全覆盖）；②单个分组的回合数不超过 {MAX_ROUNDS}；③一个分组内的回合编号必须连续（如 [3,4,5]，不允许 [3,5]）；④标题要适合联想回忆（看到标题能想起这段对话）；⑤只输出 JSON 行，不要任何其他文字、不要 markdown 代码块。";
```

运行时 `{MAX_ROUNDS}` 替换为 `dream_merge_max_rounds`（默认 5）。5 条硬性要求：①全覆盖不漏不重 / ②单组 ≤ max / ③组内连续 / ④联想向标题 / ⑤纯 JSON 行。

#### 提示词 ② —— 日层提取 · user（`build_dream_messages`，`dream.rs:500-520`）

模板（`{a}` `{b}` `{回合序号}` `{seq_a}` `{seq_b}` `{user_text}` `{assistant}` 为运行时变量）：

```
把下面 N 个回合（history seq[{a},{b}]）整理成事件分组：

回合1 [seq{seq_a}-{seq_b}] 用户：{user_text 截100字}
  助手：{assistant 首句截80字 / "(调用工具)" / "-"}
回合2 [seq{seq_a}-{seq_b}] 用户：...
  助手：...
...

输出 JSON 行（每行一个分组，必须覆盖所有回合编号 1..N，单组回合数 ≤ {MAX_ROUNDS}，组内编号连续）。
```

> **F13**：`build_dream_messages` **不含机械底座行**——只把回合原文喂给 LLM，机械底座是 reconcile 内部的程序化兜底，不进 prompt。

#### 提示词 ③ —— 月主题 · system（`MONTH_THEME_SYS`，`dream.rs:524`）

仅在**跨月**时触发（`check_and_rotate_month` → `build_month_theme`）：

```rust
const MONTH_THEME_SYS: &str = "你是 dream，负责把一个月的「日」层记忆综合成一段「月主题」。给你这个月每天的事件段，请输出一段话（150-300字）概括这个月的主要活动、主题和关键人物。只输出月主题正文，不要标题、不要 JSON、不要 markdown、不要任何前后缀。";
```

#### 提示词 ④ —— 月主题 · user（`month_theme_prompt`，`dream.rs:526-528`）

```rust
fn month_theme_prompt(ym: &str, segments_text: &str) -> String {
    format!("把 {ym} 这个月的日层记忆综合成一段月主题：\n\n{segments_text}\n\n现在输出月主题正文（一段话）。")
}
```

`segments_text` 由 `memory::collect_month_segments(cache, ym)`（`memory.rs:271-291`）收集该月 `memory/{年}/{月}/*.md` 全部事件段按文件名排序拼接而成。

`build_month_theme`（`dream.rs:531-545`）的三条返回：空月 → `Err`（不调 LLM，`dream.rs:535`）；LLM 返空 → `Err`（`dream.rs:543`）；LLM `Err` → 传播。

### 1.6 月层轮转（check_and_rotate_month）

`check_and_rotate_month`（`dream.rs:211-231`）——基于段末月 `seg_ym` 与 `.dream-meta.json` 的 `current_month`：

```rust
async fn check_and_rotate_month<E: Emitter>(round, cfg, cache, seg_ym: &str, emit) -> Result<(), String> {
    match memory::read_dream_meta(cache) {
        None => {
            // 无 meta（首次）→ 初始化 current_month = seg_ym，不跨
            memory::write_dream_meta(cache, seg_ym)?;
        }
        Some(cur) if seg_ym > cur.as_str() => {
            // seg_ym > current_month（跨月）→ 四步
            let theme = build_month_theme(round.clone(), cfg, cache, &cur, emit).await?;  // ① 综合旧月
            memory::append_year_theme(cache, &cur, &theme)?;                              // ② 年层追加
            memory::rotate_month(cache, &cur, seg_ym)?;                                   // ③ 轮转
            memory::write_dream_meta(cache, seg_ym)?;                                     // ④ 更新 meta
        }
        Some(_) => {}  // 同月 / 未来月 → 不动
    }
    Ok(())
}
```

**跨月四步**（`dream.rs:222-226`）：
1. **`build_month_theme(cur)`** —— 读旧月全部 day 段 → LLM 综合成一段月主题（150-300 字）。
2. **`append_year_theme(cur, theme)`** —— 在 `MEMORY.md` `## 年` 节追加 `- {旧月}: {月主题}`。
3. **`rotate_month(cur, seg_ym)`** —— 旧当月 → 上月（冻结）、旧上月退出、新空当月。
4. **`write_dream_meta(seg_ym)`** —— 更新 `current_month = 新月`。

**失败语义**（`dream.rs:210` 注释）：跨月时任一步失败 → `?` propagate `Err`，**不轮转、不写 meta**，下次 dream 仍检测到 `seg_ym > current_month` 重试。整个 `execute_dream` 因 `check_and_rotate_month` 在 LLM 提取之前（步 7），失败时不写 day、不写 marker、不推进 frontier（段完整保留）。

> **同月 / 未来月**：`seg_ym == cur`（同月）或 `seg_ym < cur`（未来月，理论上不会发生，因 seq 单调）→ `Some(_) => {}` 不动。

### 1.7 关键不变量（F1–F13 / G1/G4 / P1）

dream 完整性改造的核心不变量，每条都有专门测试：

| 编号 | 不变量 | 源码 / 测试 |
|---|---|---|
| **F1** | 留尾回合（tail>0）最近 N 个 user 不进 day 文件，但 `build_messages` 下轮仍可见（认 dream marker `until_seq`） | `tail_rounds_kept_out_of_dream_but_visible_in_build_messages` `dream.rs:1234`；`context.rs` `build_messages` 认 `until_seq` |
| **F2** | 段跨午夜（两天同月）→ 两天 MEMORY.md 月节今日行都刷新（distinct date upsert） | `execute_dream_cross_day_upserts_both_today_lines` `dream.rs:1307` |
| **F4** | `append_day_event` 任一 `Err` → propagate，**不写 marker**、frontier 不推进（段保留重试） | `execute_dream_append_fail_propagates_no_marker` `dream.rs:1265` |
| **F6** | LLM 分组内回合非连续 → 拆成多个连续子组（各带 LLM title） | `reconcile_non_contiguous_splits_subgroups` `dream.rs:895` |
| **F7** | tail 边界 `b` 移入 execute 用同源 events（不在 prepare 算，避免漂移） | `dream.rs:115` 注释 |
| **F8** | events 全程透传（dispatch 单次 `read_all`，execute 内部不再 read） | `agent.rs:310` + `dream.rs:124` |
| **F9** | 温和合并：机械底座与 reconcile 输出共用 `FinalEvent` struct（`round_idx` 区分） | `FinalEvent` `dream.rs:295-301` |
| **F10** | day 文件 `evt-NNN` 与 MEMORY 日层 `evt-NNN` **同源对齐**（一轮循环，append_day_event 返 nn → append_memory_day 复用） | `execute_dream_day_and_memory_evt_no_aligned_f10` `dream.rs:1362` |
| **F11** | 单组超 `max_rounds` → 强制拆成 max_rounds 块（保留 LLM title + `(续K)` 后缀） | `reconcile_over_limit_force_split` `dream.rs:884` |
| **F12** | 空段（user ≤ tail 或无新事件）→ 写 `marker(until_seq=cur)` 推进、不写 day、无 tight-loop | `execute_dream_empty_segment_advances_marker_f12` `dream.rs:1026` |
| **F13** | `build_dream_messages` 不含机械底座行（只喂回合原文） | `dream.rs:499` 注释 |
| **G1** | 完整性硬保证：段内每个回合至少落 1 条记忆（LLM 漏的机械补） | `reconcile_partial_fills_mechanical` `dream.rs:851`；`execute_dream_llm_fail_full_mechanical_fallback` `dream.rs:1339` |
| **G4** | 不阻塞：LLM 失败 → 全机械兜底仍写 marker 推进 frontier | `run_dream_llm_fail_mechanical_fallback_writes_marker` `dream.rs:978` |
| **P1 铁律③** | seq 指针代码盖戳（不让 LLM 决定边界） | `memory.rs:48` `append_day_event` 写 `seq_range` |
| **P1 铁律④** | cap 只数 `kind=user`（external/subagent_result 不顶 cap） | `dream.rs:99`；`cap_counts_only_kind_user` `dream.rs:602` |

**其它正确性**（dream-flow 既有）：
- **marker 竞态防护**：`until_seq = b`（dream-start seq）在提取前捕获；dream 进行中新落盘事件 `seq > b` 归下一段，不被本次吞掉（`run_dream_until_seq_is_dream_start_not_concurrent_event` `dream.rs:999`）。
- **失败不前进**：`prepare_dream` 只置 `in_flight` + 捕获 `a`，不推进 frontier；只有成功后才写 `last_dream_marker_seq = b`（`agent.rs:337-338`）。
- **reset 不影响提取前沿**：`seed_from_history` 只认 `dream` marker，reset marker 不动 `last_dream_marker_seq`（`reset_does_not_reset_extraction_frontier` `dream.rs:665`）。

---

## 二、MEMORY.md 更新过程（逐字段）

dream 维护 `MEMORY.md` 三层结构 + `memory/年/月/日.md` 日层。本节逐字段讲每次 dream 跑时这些文件**怎么被更新**。

### 2.1 三层骨架 ensure_memory_skeleton

`ensure_memory_skeleton`（`memory.rs:149-174`）——建 `MEMORY.md` 三层骨架，已存在则不动；若是旧四层（含 `## 周`）或缺月节子标题 → 幂等迁移。

首次创建的骨架全文（`memory.rs:152-164`）：

```markdown
# 记忆索引（MEMORY.md）

由 dream 自动维护。三层：日（今天 per-event）/月（当月逐日 + 上月冻结）/年（每月一段月主题）。

## 日

## 月
### 当月

### 上月

## 年
```

**迁移逻辑**（`migrate_to_three_tiers`，`memory.rs:176-195`）：
1. **删 `## 周` 节**：从 `## 周` 到下一个 `## ` 之前整段移除（旧四层 → 三层）。
2. **补月节子标题**：月节（`## 月`）若无 `### 当月`，在月节开头插入 `### 当月` + `### 上月`。

判定条件（`memory.rs:169`）：`text.contains("\n## 周") || !text.contains("### 当月")` → 迁移。幂等（已是三层不动，测 `ensure_memory_skeleton_idempotent_three_tier:385`）。

### 2.2 日层 day 文件 append_day_event

`append_day_event`（`memory.rs:37-59`）——追加一条「日」事件段到 `memory/{Y}/{M}/{date}.md`。

```rust
pub fn append_day_event(
    cache: &Path, ts: u64, offset_secs: i64, title: &str, detail: &str, subject: &str,
    seq_range: (u64, u64), attachment: Option<&str>,
) -> Result<u32, String> {
    let path = day_file_path(cache, ts, offset_secs);
    if let Some(p) = path.parent() { create_dir_all(p)?; }
    let nn = day_event_count(&path) + 1;                        // 写前段数 + 1
    let hhmm = time_hhmm_from_ts_local(ts, offset_secs);
    let date = date_from_ts_local(ts, offset_secs);
    let date_c = date_compact(ts, offset_secs);                 // YYYYMMDD
    let mut seg = format!(
        "\n## {hhmm} evt-{date_c}-{nn:03} {title}\n**主语**: {subject}\n**详情**: {detail}\n**对话索引**: history/{date}.jsonl#seq[{a},{b}]\n",
        a = seq_range.0, b = seq_range.1);
    if let Some(att) = attachment { seg.push_str(&format!("**附件**: {att}\n")); }
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;  // append-only
    f.write_all(seg.as_bytes())?;
    Ok(nn)                                                       // 返回 evt_no（dream 复用）
}
```

#### 文件定位 day_file_path（`memory.rs:13-20`）

```rust
fn day_file_path(cache: &Path, ts: u64, offset_secs: i64) -> PathBuf {
    let d = date_from_ts_local(ts, offset_secs);   // YYYY-MM-DD（本地）
    let (y, m) = match d.split('-').collect::<Vec<_>>().as_slice() {
        [y, m, _] => (y.to_string(), m.to_string()),
        _ => ("1970".into(), "01".into()),
    };
    cache.join("memory").join(y).join(m).join(format!("{d}.md"))
}
```

→ `cache/memory/{年}/{月}/{年}-{月}-{日}.md`。月份用两位（`memory/2026/07/`）。

#### 段格式（逐字段）

```
## HH:MM evt-YYYYMMDD-NNN 标题
**主语**: {subject}
**详情**: {detail}
**对话索引**: history/YYYY-MM-DD.jsonl#seq[a,b]
**附件**: {attachment}            ← 仅当 attachment=Some 才有此行
```

| 字段 | 来源 | 示例 |
|---|---|---|
| `HH:MM` | `time_hhmm_from_ts_local(ts, offset)` 本地时间 | `12:30` |
| `evt-YYYYMMDD-NNN` | `date_compact` + `nn`（写前段数+1） | `evt-20260726-001` |
| `标题` | dream 的 `title`（LLM 或机械底座） | `重构 drag-drop` |
| `主语` | dream 的 `subject`（用户/agent/子代理#N） | `agent` |
| `详情` | dream 的 `detail` | `on_window_event + elementFromPoint` |
| `对话索引` | **代码盖戳** `seq_range`（P1 铁律③） | `history/2026-07-26.jsonl#seq[3,9]` |

#### nn 计数（F10 关键）

`nn = day_event_count(&path) + 1`（`memory.rs:43`）。`day_event_count`（`memory.rs:23-26`）数当天文件已有多少 `## ` 开头行：

```rust
fn day_event_count(path: &Path) -> u32 {
    read_to_string(path).unwrap_or_default()
        .lines().filter(|l| l.starts_with("## ")).count() as u32
}
```

公开包装 `day_event_count_pub`（`memory.rs:29-31`）按 `(cache, ts, offset)` 定位后数段数。

**写前计数**（不是写后）是 F10 同源的关键：`nn` 在 append 之前算，返回给调用方，`append_memory_day` 复用同一 `nn` → day 文件与 MEMORY 日层的 `evt-NNN` 永远一致。

**append-only**（`memory.rs:54-57`）：`OpenOptions::new().create(true).append(true)`，dream 是当天唯一写者，跨天冻结旧文件。

### 2.3 MEMORY.md 日层 append_memory_day

`append_memory_day`（`memory.rs:62-72`）——在 `MEMORY.md` 的 `## 日` 节追加一条索引行。

```rust
pub fn append_memory_day(cache: &Path, ts: u64, offset_secs: i64, evt_no: u32, title: &str) -> Result<String, String> {
    let path = cache.join("MEMORY.md");
    if !path.exists() { ensure_memory_skeleton(cache)?; }
    let mut text = read_to_string(&path).unwrap_or_default();
    let hhmm = time_hhmm_from_ts_local(ts, offset_secs);
    let date_c = date_compact(ts, offset_secs);
    let line = format!("- {hhmm} evt-{date_c}-{evt_no:03} {title}\n");
    text = insert_in_section(&text, "日", &line);               // 插到 ## 日 节末尾
    write(&path, text)?;
    Ok(...)
}
```

**evt_no 由调用方给**（dream 复用 `append_day_event` 返回的 `nn`，F10 同源）。

索引行格式：`- HH:MM evt-YYYYMMDD-NNN 标题`（比 day 文件段少了主语/详情/对话索引，是精简索引）。

#### insert_in_section（`memory.rs:130-145`）

在 `## {section}` 节（到下一个 `## ` 之前）末尾插入 line：

```rust
fn insert_in_section(text: &str, section: &str, line: &str) -> String {
    let header = format!("## {section}");
    let mut lines = text.lines().collect::<Vec<_>>();
    let start = match lines.iter().position(|l| l.trim_start() == header) {
        Some(i) => i + 1,
        None => { lines.push(""); lines.push(header.as_str()); lines.push(line); return lines.join("\n"); }  // 节不存在→新建
    };
    let mut end = start;
    while end < lines.len() && !lines[end].trim_start().starts_with("## ") { end += 1; }   // 找节末尾
    let mut insert_at = end;
    while insert_at > start && lines[insert_at - 1].trim().is_empty() { insert_at -= 1; }  // 跳过尾部空行
    lines.insert(insert_at, line.trim_end_matches('\n'));
    lines.join("\n")
}
```

### 2.4 月层今日行 upsert_current_month_today

`upsert_current_month_today`（`memory.rs:89-99`）——在 `MEMORY.md` 月节 `### 当月` 子节里，按日期 upsert 一行（汇总当天所有事件段 title）。

```rust
pub fn upsert_current_month_today(cache: &Path, ts: u64, offset_secs: i64) -> Result<(), String> {
    let path = cache.join("MEMORY.md");
    if !path.exists() { ensure_memory_skeleton(cache)?; }
    let text = read_to_string(&path).unwrap_or_default();
    let today = date_from_ts_local(ts, offset_secs);            // YYYY-MM-DD（upsert 的 key）
    let titles = day_titles(cache, ts, offset_secs);            // 读当天 day 文件所有 title
    let summary = if titles.is_empty() { "（无事件）".into() } else { titles.join("；") };
    let new_line = format!("- {today}: {summary}");
    let new_text = upsert_line_in_subsection(&text, "月", "当月", &today, &new_line);
    write(&path, new_text)
}
```

#### day_titles（`memory.rs:75-86`）

读当天 day 文件所有事件段 title（去 `## HH:MM evt-NNN ` 前缀）：

```rust
pub fn day_titles(cache: &Path, ts: u64, offset_secs: i64) -> Vec<String> {
    let path = day_file_path(cache, ts, offset_secs);
    let body = read_to_string(&path).unwrap_or_default();
    body.lines().filter_map(|l| {
        let rest = l.strip_prefix("## ")?;
        let mut it = rest.splitn(3, ' ');   // "HH:MM evt-NNN title..."
        it.next(); it.next(); it.next().map(|t| t.to_string())   // 取第三段（title，可含空格）
    }).collect()
}
```

今日行格式：`- YYYY-MM-DD: {title1}；{title2}；...`（当天所有段 title 用 `；` 连接）。无事件 → `（无事件）`。

#### upsert_line_in_subsection（`memory.rs:102-127`）

在 `## {section}` 节的 `### {subsection}` 子节里，按 key（行前缀 `- {key}`）upsert 一行：

```rust
fn upsert_line_in_subsection(text: &str, section: &str, subsection: &str, key: &str, new_line: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    // 1. 定位 ## {section} 节范围 [sec_start, sec_end)
    let sec_start = position(## {section}) + 1;
    // 2. 在节范围内定位 ### {subsection} 子节范围 [sub_start, sub_end)
    let sub_start = (sec_start..sec_end).find(starts_with "### {subsection}") + 1;
    // 3. 在子节范围内按 key_prefix = "- {key}" 查找
    if let Some(i) = (sub_start..sub_end).find(|l| l.starts_with("- {key}")) {
        lines[i] = new_line;          // 已存在 → 替换
    } else {
        lines.insert(sub_end 尾部, new_line);  // 不存在 → 追加
    }
    lines.join("\n")
}
```

**同日 dream 多次不重复**：同一 `today`（key）第二次写时找到已有行 → 替换（不新增）。测 `upsert_replaces_today_line_on_second_dream_same_day:465`。

### 2.5 跨天 F2 distinct date

`execute_dream` 步 10（`dream.rs:189-199`）：段内事件可能跨午夜（如深夜聊天到次日），需对**每个 distinct 日期**都 `upsert_current_month_today`：

```rust
let mut seen_days: BTreeSet<String> = BTreeSet::new();
for ev in &events_out {
    seen_days.insert(date_from_ts_local(ev.last_ts, offset));   // 每个 FinalEvent 的日期
}
seen_days.insert(date_from_ts_local(last_ts, offset));          // 兜底段末日期
for day_str in &seen_days {
    let rep_ts = events_out.iter()
        .find(|ev| date_from_ts_local(ev.last_ts, offset) == *day_str)
        .map(|e| e.last_ts).unwrap_or(last_ts);
    let _ = upsert_current_month_today(cache, rep_ts, offset);  // 该日期代表 ts
}
```

用 `BTreeSet` 去重 → 段跨几天就刷几个今日行。测 `execute_dream_cross_day_upserts_both_today_lines:1307`。

### 2.6 月层轮转 rotate_month

`rotate_month`（`memory.rs:218-224`）——跨月时把旧当月 → 上月（冻结）、旧上月退出、新空当月。

```rust
pub fn rotate_month(cache: &Path, completed_ym: &str, new_ym: &str) -> Result<(), String> {
    let path = cache.join("MEMORY.md");
    if !path.exists() { ensure_memory_skeleton(cache)?; }
    let text = read_to_string(&path).unwrap_or_default();
    let new_text = rotate_month_text(&text, completed_ym, new_ym);
    write(&path, new_text)
}
```

#### rotate_month_text（`memory.rs:226-246`）

```rust
fn rotate_month_text(text: &str, completed_ym: &str, new_ym: &str) -> String {
    let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    // 定位 ## 月 节范围 [sec_start, sec_end)
    let sec_start = position("## 月");
    let mut sec_end = sec_start + 1;
    while sec_end < lines.len() && !lines[sec_end].trim_start().starts_with("## ") { sec_end += 1; }
    // 取旧当月内容（连带其逐日行）
    let cur_body = extract_subsection_body(&lines, sec_start + 1, sec_end, "当月");
    // 删整个旧月节（含旧上月），重建为「上月冻结 + 空当月」
    let mut new_section = vec!["## 月".to_string()];
    new_section.push(format!("### 上月 {completed_ym}（冻结）"));
    new_section.extend(cur_body.into_iter().filter(|l| !l.is_empty()));
    new_section.push(String::new());                                    // 空行
    new_section.push(format!("### 当月 {new_ym}"));
    new_section.push(String::new());                                    // 空行分隔 ## 年
    // 拼回：## 月 之前 + 新月节 + ## 月 之后
    let mut out = lines[..sec_start].to_vec();
    out.extend(new_section);
    out.extend(lines[sec_end..].to_vec());
    out.join("\n")
}
```

**轮转效果**：
- 旧当月（`### 当月 {旧月}`）内容 → 移到 `### 上月 {旧月}（冻结）`。
- 旧上月（如有）→ **退出**（2 月窗口：只保留当月 + 上月）。
- 新建空当月 `### 当月 {新月}`。

测：`rotate_moves_current_to_previous_and_creates_empty_current:506`、`rotate_drops_old_previous:517`（旧上月退出）、`rotate_leaves_blank_line_before_next_section:530`（cosmetic 空行）。

### 2.7 年层 append_year_theme

`append_year_theme`（`memory.rs:260-268`）——跨月时在 `## 年` 节追加 `- {ym}: {theme}`。

```rust
pub fn append_year_theme(cache: &Path, ym: &str, theme: &str) -> Result<(), String> {
    let path = cache.join("MEMORY.md");
    if !path.exists() { ensure_memory_skeleton(cache)?; }
    let mut text = read_to_string(&path).unwrap_or_default();
    let line = format!("- {ym}: {theme}");
    text = insert_in_section(&text, "年", &line);       // 插到 ## 年 节末尾
    write(&path, text)
}
```

多月主题累加（测 `append_year_theme_multiple_months_accumulate:554`）。月主题由 `build_month_theme`（`dream.rs:531-545`）LLM 生成。

### 2.8 .dream-meta.json

记录 `current_month`（跨月判定依据）。

```rust
// read（memory.rs:198-202）
pub fn read_dream_meta(cache: &Path) -> Option<String> {
    let text = read_to_string(cache.join("memory").join(".dream-meta.json")).ok()?;
    let v: Value = from_str(&text).ok()?;
    v.get("current_month").and_then(|x| x.as_str()).map(|s| s.to_string())
}
// write（memory.rs:205-210）
pub fn write_dream_meta(cache: &Path, ym: &str) -> Result<(), String> {
    let dir = cache.join("memory");
    create_dir_all(&dir)?;
    let body = format!(r#"{{"current_month":"{}"}}"#, ym);
    write(dir.join(".dream-meta.json"), body)
}
```

文件落在 `cache/memory/.dream-meta.json`，内容 `{"current_month":"2026-07"}`。首次 dream → 初始化；跨月 → 更新为新月。

`ym_from_ts`（`memory.rs:213-215`）：由时间戳得 YYYY-MM（`date_from_ts_local` 前 7 字符），无 chrono。

### 2.9 完整跑前/跑后示例

假设 2026-07-26 有 2 个 user 回合（seq 1-2 + seq 3-5），dream 首次跑（meta 不存在）：

**跑前**：
```
cache/
├── memory/                          ← 空
└── history/2026-07-26.jsonl         ← 5 条事件（seq 0-5）
```

**跑后**（`a=1, b=5, tail=0`，LLM 返回 2 组）：
```
cache/
├── MEMORY.md                        ← 新建（ensure_memory_skeleton）
│   # 记忆索引（MEMORY.md）
│   ...
│   ## 日
│   - 12:30 evt-20260726-001 重构 drag-drop       ← append_memory_day（nn=1，复用 day）
│   - 14:00 evt-20260726-002 修重影               ← append_memory_day（nn=2）
│   ## 月
│   ### 当月                                        ← upsert_current_month_today（distinct date）
│   - 2026-07-26: 重构 drag-drop；修重影
│   ### 上月
│   ## 年
├── memory/
│   ├── .dream-meta.json             ← {"current_month":"2026-07"}（首次初始化）
│   └── 2026/07/2026-07-26.md        ← append_day_event ×2
│       ## 12:30 evt-20260726-001 重构 drag-drop
│       **主语**: agent
│       **详情**: on_window_event + elementFromPoint
│       **对话索引**: history/2026-07-26.jsonl#seq[1,2]
│       ## 14:00 evt-20260726-002 修重影
│       **主语**: agent
│       **详情**: buildHistoryBubbles
│       **对话索引**: history/2026-07-26.jsonl#seq[3,5]
└── history/2026-07-26.jsonl
    ... 原事件 + 新增 dream marker {"kind":"marker","data":{"marker":"dream","until_seq":5}}
```

**关键对齐**（F10）：day 文件 `evt-20260726-001/002` == MEMORY.md 日层 `evt-20260726-001/002`（同源 `nn`）。

---

## 三、mem CLI

> **mem = memory + history 的只读 drill CLI**。和 `ovoice.exe` 同 crate 的第二个 `[[bin]]`，共享 `ovoice_lib`。**零 LLM、纯只读**——只读文件 + 机械解析，不写状态、不烧 token。

### 3.1 本质：文件树即索引

核心哲学（`mem_cli.rs:1-2`，spec §10.6）：dream 不写额外索引文件，三样东西天然是索引：

| 索引维度 | 载体 | mem 怎么用 |
|---|---|---|
| 时间索引 | 路径 `memory/{年}/{月}/{日期}.md` | `ls` 列目录、`index YYYY-MM` 扫月 |
| 标题索引 | `{date}.md` 首行 `## HH:MM evt-NNN 标题` | `ls` 显示天标题、`first_title` 提取 |
| 对话指针 | 段内 `**对话索引**: history/{date}.jsonl#seq[a,b]` | `index` 紧凑行带 `#seq[a,b]` → `history --seq` 还原 |

mem 现场机械生成任何粒度索引，无冗余、无同步问题。

### 3.2 cache 定位（5 档）

`resolve_cache`（`bin/mem.rs:58-80`）——mem 只读 `memory/` + `history/`，定位 cache 是第一要务：

```
① --cache <path>                显式覆盖（--workspace <path> 作 legacy 别名）
                                    flag 可在 leading/trailing 任意位置
                                 ┃ 未传
                                 ▼
② $OVOICE_CACHE 环境变量        agent bash spawn 时由 tool_bash 注入（=ctx.cache）
   （$OVOICE_WORKSPACE 别名）    ┃ 未设
                                 ▼
③ next-to-exe/config.json       portable 优先：读 exe 同档 config 的原始 cache_dir 字段
                                 ┃ 字段空 / 无文件
                                 ▼
④ %APPDATA%/com.ovoice.app/     APPDATA config：读同档 config 的原始 cache_dir 字段
   config.json                   ┃ 字段空 / 无文件
                                 ▼
⑤ %APPDATA%/com.ovoice.app/     兜底默认（cache 不进 Documents，符合 D2 备份初衷）
   cache
```

各 OS 的 APPDATA 路径（`appdata_dir`，`bin/mem.rs:82-90`）：Windows `%APPDATA%/com.ovoice.app`；macOS `~/Library/Application Support/com.ovoice.app`；Linux `~/.local/share/com.ovoice.app`。

#### `load_raw` vs `load_from`（bug #3 根因，`bin/mem.rs:94-99`）

第③④档读 config 用 `config::load_raw` **而非** `load_from`：

```rust
fn resolve_from_config(dir: &Path) -> Option<PathBuf> {
    let raw = config::load_raw(dir).cache_dir;
    let trimmed = raw.trim();
    if trimmed.is_empty() { return None; }            // 空 → 跳下一档
    Some(config::resolve_cache_dir(trimmed, dir))
}
```

**原因**：`load_from` 把空 `cache_dir` 重写成默认值，mem 无法判断「用户没设过」（应跳档）vs「设了默认值」——portable 场景会错定位到 APPDATA。`load_raw` 保留原始字段，空→`None`→跳档。

#### `strip_loc_flag`（bug #3a，`bin/mem.rs:19-27`）

剥离 `--cache <path>` / `--workspace <path>` 这对 flag，让 dispatch 看到的 `args[1]` 始终是子命令：

```rust
fn strip_loc_flag(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--cache" || a == "--workspace" { it.next(); continue; }   // 跳过 flag + 其值
        out.push(a.clone());
    }
    out
}
```

**原因**：`mem --cache X ls` 不剥离 → `args[1]="--cache"` 被当「未知命令」。`resolve_cache` 用原 args 扫描，互不干扰。

### 3.3 调用方式

#### agent 经 tool_bash 调用（主路径）

agent 调 `bash` 工具跑 `mem ...` 时，`tool_bash` 做两件事：
- **PATH 注入**（绿色，不改系统环境）：运行时把 `current_exe()` 父目录前置进子进程 PATH → dev 下 `target/debug/mem.exe` 直接可调（详见 CLAUDE.md §10.3）。
- **环境变量注入**：`OVOICE_CACHE` = `ctx.cache`、`OVOICE_WORKSPACE` = `ctx.workspace`（mem 走第②档定位）。

→ agent bash 调 `mem ls` 自动找到当前会话记忆缓存，**无需知道 cache 在哪**。

#### dev / release 位置

- **dev**：`src-tauri/target/debug/mem.exe`（与 `ovoice.exe` 同目录），PATH 注入后 bash 直接可调。
- **release**：需先把 `mem.exe` 复制为 `src-tauri/binaries/mem-<host-triple>.exe`，在 `tauri.conf.json` 加 `"externalBin": ["binaries/mem"]`，再 `tauri build`（bundle 时去 triple 后缀落主 exe 同目录）。**externalBin 不能常驻 main 的 tauri.conf.json**（tauri-build 每次 check 都校验文件存在）。

### 3.4 五个子命令详解

顶层流程（`bin/mem.rs:8-16`）：`resolve_cache` → `strip_loc_flag` → `dispatch` 三步独立，cache 由调用方解析、dispatch 不碰 env（可纯逻辑单测）。

dispatch（`bin/mem.rs:29-39`）：

```rust
fn dispatch(args: &[String], cache: &Path) -> String {
    match args.get(1).map(|s| s.as_str()) {
        Some("ls") => mem_cli::ls(cache, args.get(2).map(|s| s.as_str()), args.get(3).map(|s| s.as_str())),
        Some("read") => mem_cli::read_day(cache, args.get(2).map(|s| s.as_str()).unwrap_or("")),
        Some("history") => mem_cli::history(cache, args.get(2).map(|s| s.as_str()).unwrap_or(""), parse_seq_flag(args)),
        Some("search") => mem_cli::search(cache, args.get(2).map(|s| s.as_str()).unwrap_or(""), args.iter().any(|a| a == "--raw")),
        Some("index") => mem_cli::index(cache, args.get(2).map(|s| s.as_str())),
        Some("--help") | Some("-h") | None => usage(),
        Some(other) => format!("未知命令: {other}\n\n{}", usage()),
    }
}
```

#### 3.4.1 `mem ls [year [month]]`（`mem_cli.rs:12-24`）—— 列年/月/天

```rust
pub fn ls(workspace: &Path, year: Option<&str>, month: Option<&str>) -> String {
    let mem = workspace.join("memory");
    match (year, month) {
        (None, None) => list_dirs(&mem),                       // 列年
        (Some(y), None) => list_dirs(&mem.join(y)),            // 列月
        (Some(y), Some(m)) => {
            let m_norm = if m.len() == 1 { format!("0{m}") } else { m.to_string() };  // 一位月补零
            list_days(&mem.join(y).join(&m_norm))              // 列天（带标题）
        }
        (None, Some(_)) => "请先指定年".into(),
    }
}
```

- `list_dirs`（`mem_cli.rs:26-36`）：`read_dir` 过滤纯数字目录名，排序；空→`（{dir} 下无数据）`。
- `list_days`（`mem_cli.rs:38-52`）：每个 `{date}.md` 配 `first_title` 提取的标题，格式 `{date}: {title}`。
- **一位月补零**（`mem_cli.rs:19`）：dream 写两位月目录（`memory/2026/07/`），用户传 `mem ls 2026 7` 自动补零命中 `07`。

`first_title`（`mem_cli.rs:54-60`）：从 `## 12:30 evt-20260726-001 重构 drag-drop` 提取 `重构 drag-drop`（去时间戳前缀）。

示例：
```
$ mem ls                $ mem ls 2026           $ mem ls 2026 07
2025                    07                      2026-07-26: 重构 drag-drop
2026                                            2026-07-29: attach 工具上线
```

#### 3.4.2 `mem read <date>`（`mem_cli.rs:63-70`）—— 打印当天事件段

```rust
pub fn read_day(workspace: &Path, date: &str) -> String {
    let (y, m) = match split_date(date) { Some(x) => x, None => return "日期格式应为 YYYY-MM-DD".into() };
    let path = workspace.join("memory").join(y).join(m).join(format!("{date}.md"));
    match read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => s,        // day 文件原文
        _ => format!("（{date} 无当天记忆）"),
    }
}
```

最简单的命令，读 day 文件原文（含 `**对话索引**` 指针）。示例见 [2.9](#29-完整跑前跑后示例) 的 day 文件输出。

#### 3.4.3 `mem history <date> [--seq a..b]`（`mem_cli.rs:73-85`）—— 读原始对话

```rust
pub fn history(workspace: &Path, date: &str, seq_range: Option<(u64, u64)>) -> String {
    let path = workspace.join("history").join(format!("{date}.jsonl"));
    let text = match read_to_string(&path) { Ok(s) => s, Err(_) => return format!("（{date} 无原始对话 history）") };
    let mut out = String::new();
    for line in text.lines() {
        if line.trim().is_empty() { continue; }
        let Ok(ev) = serde_json::from_str::<HistoryEvent>(line) else { continue };   // 坏行跳过
        if let Some((a, b)) = seq_range { if ev.seq < a || ev.seq > b { continue; } } // seq 过滤
        out.push_str(&humanize(&ev)); out.push('\n');
    }
    if out.is_empty() { format!("（{date} 的 seq 范围内无事件）") } else { out }
}
```

`--seq` 解析（`bin/mem.rs:45-56` `parse_seq_flag`）支持 `a..b` 和 `a,b` 两种分隔符。

`humanize`（`mem_cli.rs:87-107`）按 kind 渲染：

| kind | 输出格式 |
|---|---|
| `user` | `[seq{N} 用户] {text}` |
| `assistant`（有 tool_calls） | `[seq{N} 助手] (调用工具)` |
| `assistant`（无 tool_calls） | `[seq{N} 助手] {content}` |
| `tool_result` | `[seq{N} 工具结果:{name}] {result}` |
| `external` | `[seq{N} 外部] {what}` |
| `subagent_result` | `[seq{N} 子代理#{agent_id}完成] {summary}` |
| `marker` | `[seq{N} marker {marker} until_seq={N}] (隐藏)` |

> `tool_result` 读 `data.result`（不是 content/text）——gstack review 抓到的 fix（`humanize_tool_result_shows_result_content:511`）。

示例：
```
$ mem history 2026-07-26 --seq 3..9
[seq3 用户] 帮我把拖放改成 native DnD
[seq4 助手] (调用工具)
[seq5 工具结果:bash] git diff src/main.js
[seq9 子代理#1完成] 已验证 dragDropEnabled 互斥
```

#### 3.4.4 `mem search <query> [--raw]`（`mem_cli.rs:110-119`）—— 关键字 grep

```rust
pub fn search(workspace: &Path, query: &str, raw: bool) -> String {
    let mut hits = Vec::new();
    let mem = workspace.join("memory");
    grep_dir(&mem, query, &mut hits);                // 默认只搜 memory/
    if raw {
        let hist = workspace.join("history");
        grep_dir(&hist, query, &mut hits);           // --raw 扩到 history
    }
    if hits.is_empty() { format!("（未找到 {query:?}）") } else { hits.join("\n") }
}
```

`grep_dir`（`mem_cli.rs:121-134`）递归子目录，输出 `{path}:{line_no}: {line}`（ripgrep 风格）。

**默认不搜 history**：history 是原始对话（含工具输出、可能含密钥），dream 整理过的 memory 才是「该被回忆的」。`--raw` 是明确翻原始记录的逃生口。

示例：
```
$ mem search drag-drop
memory/2026/07/2026-07-26.md:1: ## 12:30 evt-20260726-001 重构 drag-drop
memory/2026/07/2026-07-26.md:3: **详情**: on_window_event + elementFromPoint
```

#### 3.4.5 `mem index [tier]`（`mem_cli.rs:142-175`）—— 索引统一入口（最复杂）

既能读 MEMORY.md 三层，又能从 day 文件机械生成任意时间粒度紧凑索引。

tier 分流（`mem_cli.rs:142-175`）：

```
mem index [tier]
    ├─ tier = None ─────────────► 读 MEMORY.md 全文（缺文件 → 提示）
    ├─ tier ∈ {日,月,年,day,month,year} ► 读 MEMORY.md 该层 section
    ├─ tier 匹配 YYYY ──────────► index_year：各月概览（月主题 or 段数+首条）
    ├─ tier 匹配 YYYY-MM ───────► index_month：该月事件段紧凑索引（按日期+时间排）
    ├─ tier 匹配 YYYY-MM-DD ────► index_day：该天事件段紧凑索引
    └─ 其它 ─────────────────────► "未知层/时间" + 用法提示
```

**紧凑索引行格式**（给 agent 判断 drill 的关键）：
```
- HH:MM [主语] 标题 — 详情 → YYYY-MM-DD#seq[a,b]
```

`parse_segments`（`mem_cli.rs:208-248`）把 day 文件解析成 `Segment`（`mem_cli.rs:197-205`：`hhmm/title/subject/detail/date/seq_a/seq_b`）。解析规则：
- `## ` 行开新段：`hhmm = rest[..5]`，`title` 跳过 `HH:MM evt-NNN ` 前缀取剩余。
- `**主语**: ` → `subject`；`**详情**: ` → `detail`。
- `**对话索引**: history/{date}.jsonl#seq[a,b]` → 解析 `date` + `seq_a/seq_b`。

`index_year`（`mem_cli.rs:333-358`）的月主题：`read_year_themes`（`mem_cli.rs:361-386`）从 MEMORY.md `## 年` 节解析 `{YM: 月主题}` HashMap；有月主题 → `- YYYY-MM: {月主题}（月主题 · N 段）`，无 → `- YYYY-MM: N 段（首条: {首条}）`（`count_segments_in_month:389`）。

格式判定（`mem_cli.rs:250-264`）：`is_year`（4 位纯数字）/ `is_year_month`（YYYY-MM 各两位）/ `is_year_month_day`（YYYY-MM-DD 各两位）。

示例：
```
$ mem index 2026               $ mem index 2026-07          $ mem index 2026-07-26
## 2026（各月概览）             ## 2026-07（事件段索引）     ## 2026-07-26（事件段索引）
- 2026-06: 六月主要做X（月主题） - 07-26 12:30 [agent] 重构…  - 12:30 [agent] 重构 drag-drop …
- 2026-07: 28 段（首条: 重构…）  - 07-26 14:00 [agent] 修重影… - 14:00 [agent] 修重影 …
```

### 3.5 drill 路径闭环

day 段记 `seq[a,b]` → `index` 紧凑行带 `#seq[a,b]` → `history --seq a..b` 还原原始对话。从「记忆标题」drill 到「原始对话」全程机械可走：

```
mem index 2026-07          ← 看这个月有什么（紧凑行带 #seq[a,b]）
  ↓ 选定一条
mem read 2026-07-26        ← 看当天事件段全文（含详情 + 对话索引）
  ↓ 想看原始逐字对话
mem history 2026-07-26 --seq 3..9    ← 按 seq 指针还原原文
  ↓ 找词
mem search drag-drop [--raw]         ← 关键字 grep（--raw 扩 history）
```

AGENT.md §「记忆深思」已 pinned MEMORY.md（日/月/年三层）在 agent 上下文，**日常不用主动查**——记忆已在眼前。只在查 MEMORY.md 窗口之外的远期细节时用 bash 调 mem。

---

## 四、数据流总图（dream 写 / mem 读 / history 真相）

```
┌─────────────────────────────────────────────────────────────────────────┐
│  history/{date}.jsonl  ← 真相之源（append-only，agent.rs/llm.rs 落盘）  │
│  每行一个 HistoryEvent（user/assistant/tool_result/external/            │
│  subagent_result/marker，含 dream marker {until_seq=b}）                │
└──────────────┬──────────────────────────────────────┬───────────────────┘
               │ dream 读（read_all 透传，F8）          │ mem history 读 + humanize
               ▼                                        │
┌──────────────────────────────────┐                   │
│  dream（写端，dream.rs）          │                   │
│  execute_dream 十步：             │                   │
│   tail → segment → split_rounds   │                   │
│   → mechanical_extract（G1）      │                   │
│   → check_and_rotate_month        │                   │
│   → LLM build_dream_messages      │                   │
│   → parse_groups                  │                   │
│   → reconcile_groups（F6/F11/G1） │                   │
│   → append_day_event + day_nn     │                   │
│   → append_memory_day（F10 同源） │                   │
│   → upsert_current_month_today    │                   │
│   → marker(until_seq=b)           │                   │
└──────────────┬───────────────────┘                   │
               │ dream 写                              │
               ▼                                       │
┌──────────────────────────────────────────────────────────────────────┐
│  memory/{年}/{月}/{date}.md   ← 日层（append_day_event）              │
│  ## HH:MM evt-NNN title + 主语/详情/对话索引 seq[a,b]（代码盖戳）    │
│                                                                       │
│  MEMORY.md                    ← 三层索引（dream 维护）                │
│  ## 日  - HH:MM evt-NNN title（append_memory_day，nn 同源）          │
│  ## 月 ### 当月  - YYYY-MM-DD: title1；title2（upsert，distinct date）│
│       ### 上月 {旧月}（冻结）（rotate_month 轮转）                   │
│  ## 年 - YYYY-MM: {月主题}（append_year_theme，跨月时）              │
│                                                                       │
│  memory/.dream-meta.json      ← {"current_month":"YYYY-MM"}           │
└──────────────┬────────────────────────────────────────────────────────┘
               │ mem 读（纯只读，零 LLM）                                ▲
               ▼                                                         │
┌──────────────────────────────────────────────────┐                    │
│  mem（读端，mem_cli.rs）                          │ ──────────────────┘
│  ls / read / history / search / index            │
│  文件树即索引，现场机械生成                       │
└──────────────────────────────────────────────────┘
```

**三者通过文件系统格式解耦，互不直接调用**：dream 决定格式（day 段结构 / MEMORY.md 三层 / seq 指针盖戳）；mem 机械读这些格式现场生成索引；history 是双方共同的真相之源。

---

## 五、源码索引（全表）

### dream（`src-tauri/src/dream.rs`）

| 关注点 | 位置 |
|---|---|
| DreamTrigger 状态机（4 字段） | `dream.rs:20-25` |
| check() 触发判定（cap 优先 idle） | `dream.rs:74-107` |
| note_activity / seed_from_history | `dream.rs:44-61` |
| prepare_dream 占位（set in_flight + a，不推进 frontier） | `dream.rs:116-121` |
| **execute_dream 十步** | `dream.rs:126-204` |
| F12 空段早退 | `dream.rs:143-147` |
| 落盘（F10 一轮循环 + F2 distinct date） | `dream.rs:183-199` |
| run_dream 可测入口 | `dream.rs:234-254` |
| check_and_rotate_month（跨月四步） | `dream.rs:211-231` |
| Round / FinalEvent 结构 | `dream.rs:260-265` / `dream.rs:295-301` |
| split_rounds 按回合切 | `dream.rs:268-292` |
| collapse_ws / truncate_chars / truncate_title / truncate_first_sentence | `dream.rs:303-334` |
| mechanical_extract（G1 机械底座） | `dream.rs:341-360` |
| tail_aware_b（留尾） | `dream.rs:364-382` |
| Group / ReconcileStats 结构 | `dream.rs:387` / `dream.rs:390-396` |
| parse_groups（LLM 分组解析，坏行计数） | `dream.rs:399-421` |
| merge_event（合并机械底座） | `dream.rs:425-437` |
| reconcile_groups（F6 非连续拆 / F11 超限拆 / G1 全覆盖） | `dream.rs:440-497` |
| build_dream_messages（日层 user prompt，F13 不含机械底座） | `dream.rs:500-520` |
| DREAM_SYS（日层 system prompt） | `dream.rs:112` |
| MONTH_THEME_SYS / month_theme_prompt | `dream.rs:524` / `dream.rs:526-528` |
| build_month_theme（月主题 LLM） | `dream.rs:531-545` |
| F10 evt_no 对齐回归测试 | `dream.rs:1362` |

### memory（`src-tauri/src/memory.rs`）

| 关注点 | 位置 |
|---|---|
| day_file_path（文件定位） | `memory.rs:13-20` |
| day_event_count / day_event_count_pub（F10 计数） | `memory.rs:23-31` |
| **append_day_event**（写 day 段，返 nn） | `memory.rs:37-59` |
| append_memory_day（MEMORY 日层，复用 nn） | `memory.rs:62-72` |
| day_titles（读当天所有 title） | `memory.rs:75-86` |
| upsert_current_month_today（月节今日行） | `memory.rs:89-99` |
| upsert_line_in_subsection（按 key upsert） | `memory.rs:102-127` |
| insert_in_section（节末尾插行） | `memory.rs:130-145` |
| ensure_memory_skeleton（三层骨架 + 迁移） | `memory.rs:149-174` |
| migrate_to_three_tiers（删周 + 补子标题） | `memory.rs:176-195` |
| read/write_dream_meta | `memory.rs:198-210` |
| ym_from_ts | `memory.rs:213-215` |
| rotate_month / rotate_month_text（月层轮转） | `memory.rs:218-246` |
| extract_subsection_body | `memory.rs:248-257` |
| append_year_theme（年层月主题） | `memory.rs:260-268` |
| collect_month_segments（月主题原料） | `memory.rs:271-291` |

### agent driver 接线（`src-tauri/src/agent.rs`）

| 关注点 | 位置 |
|---|---|
| SessionEvent（含 DreamCheck） | `agent.rs:11-21` |
| handle_event（落 history + run_turn） | `agent.rs:61-111` |
| spawn_session（driver + 60s ticker） | `agent.rs:135-221` |
| 60s idle ticker | `agent.rs:166-176` |
| driver select 主循环 | `agent.rs:208-218` |
| run_one 路由（DreamCheck → dispatch；turn 后 dispatch） | `agent.rs:229-289` |
| dispatch_dream 三阶段（check → prepare → spawn → 推进） | `agent.rs:301-346` |
| DreamSilentEmitter（静默） | `agent.rs:349-359` |

### mem CLI（`src-tauri/src/bin/mem.rs` + `src-tauri/src/mem_cli.rs`）

| 关注点 | 位置 |
|---|---|
| main（三步：resolve_cache / strip_loc_flag / dispatch） | `bin/mem.rs:8-16` |
| strip_loc_flag（bug #3a） | `bin/mem.rs:19-27` |
| dispatch 路由 | `bin/mem.rs:29-39` |
| usage（命令一览） | `bin/mem.rs:41-43` |
| parse_seq_flag（--seq a..b / a,b） | `bin/mem.rs:45-56` |
| resolve_cache（5 档定位） | `bin/mem.rs:58-80` |
| appdata_dir（各 OS 路径） | `bin/mem.rs:82-90` |
| resolve_from_config（load_raw 判空，bug #3） | `bin/mem.rs:94-99` |
| ls / list_dirs / list_days | `mem_cli.rs:12-52` |
| first_title（去时间戳前缀） | `mem_cli.rs:54-60` |
| read_day | `mem_cli.rs:63-70` |
| history + humanize | `mem_cli.rs:73-107` |
| search + grep_dir | `mem_cli.rs:110-134` |
| index（tier 分流） | `mem_cli.rs:142-175` |
| parse_sections（MEMORY.md 切片） | `mem_cli.rs:177-192` |
| Segment 结构 + parse_segments | `mem_cli.rs:197-248` |
| 格式判定 is_year/month/day | `mem_cli.rs:250-264` |
| index_day / index_month | `mem_cli.rs:275-330` |
| index_year + read_year_themes + count_segments_in_month | `mem_cli.rs:333-410` |

### config（`src-tauri/src/config.rs`）

| 关注点 | 位置 |
|---|---|
| dream_idle_secs（7200） | `config.rs:90-91` / `d_dream_idle` `config.rs:108` |
| dream_cap_turns（50） | `config.rs:92-93` / `d_dream_cap` `config.rs:109` |
| dream_tail_rounds（3） | `config.rs:101-102` / `d_dream_tail_rounds` `config.rs:112` |
| dream_merge_max_rounds（5） | `config.rs:104-105` / `d_dream_merge_max_rounds` `config.rs:113` |

---

> **相关文档**：[`paths.md`](paths.md)（cache/workspace 路径管理）。
