# dream-completeness spec · engineering review

- **日期**：2026-07-30
- **Review target**：`docs/superpowers/specs/2026-07-30-dream-completeness-design.md`
- **Reviewer**：Claude（gstack plan-eng-review）+ outside voice（独立 Claude subagent，cross-model）
- **Verdict**：**REVISE BEFORE PLAN** — 1 个 P0 否决 spec 核心前提（G2 留尾防失忆静默失效）；6 个 P1 必须 plan 覆盖。

---

## P0 — blocks plan（必须先决策修复方向）

### F1 · tail retention 不工作（G2 失效）

**[P0] (conf 10/10) `context.rs:70-71` + `dream.rs:177`**

spec §4.2 不变量 2 + §5.1 声称：`b = current_seq − tail_rounds`，写 `marker(now,"dream",b)`（until_seq=b），下轮 `build_messages` 按 `seq > b` 过滤 → tail 保留。**这是错的**。

`context.rs:70-71`：
```rust
fn last_marker_seq(events: &[HistoryEvent]) -> Option<u64> {
    events.iter().filter(|e| e.kind == "marker").map(|e| e.seq).max()  // 取 marker.seq
}
```
`context.rs:80`：
```rust
.filter(|e| e.thread == "main" && match cut { Some(s) => e.seq > s, None => true })
```
`cut` = marker 事件自己的 `.seq`（在 history 的位置），**不是 `data["until_seq"]`**。marker 在 `execute_dream` 末尾 append（`dream.rs:177`），所以 `marker.seq` = append 时的 current_seq，**大于**所有 tail 事件 seq（tail seq ∈ (b, marker.seq)）。filter `e.seq > marker.seq` → **tail 全被排除**。G2 静默失效。

**为何现状没暴露**：现状 `b = current_seq`（`dream.rs:131`），无并发时 `marker.seq == until_seq == b`，两者巧合相等。spec 引入 `b = current_seq − tail_rounds` 后 `marker.seq > until_seq`，混淆暴露。现有 7 个测试不断言 tail 在 build_messages 后可见，所以绿着通过。

**修复方向**（推荐 A）：
- **A（surgical，推荐）**：改 `last_marker_seq` 读 `e.data["until_seq"]`，filter `e.seq > until_seq`。**顺带修现状 bug**：dream 期间用户发的事件（seq ∈ (b, marker.seq)）现在也被 marker.seq 切掉永久失忆——改后可见。所有 marker（dream `dream.rs:317`/reset）都带 until_seq，向后兼容（无 tail 时两者相等）。
- B：重新设计留尾机制（不依赖 marker until_seq）。
- C：砍掉 G2（留尾），spec 回退到只做完整性 G1。
- D：加回归测试 `tail_events_survive_marker_in_next_build_messages` 锚定（与 A 配合，非替代）。

F1 否决：§4.2 不变量 2、§5.1 `tail_aware_b` 价值、§11 R3。

---

## P1 — 真 bug / 必须 plan 覆盖

### F2 · upsert 跨天/跨月漏刷
**[P1] (conf 9/10) `memory.rs:82-92` + `dream.rs:175`**
`upsert_current_month_today(cache, ts, offset)` 只刷 `ts` 那天的今日行（`memory.rs:86` `let today = date_from_ts_local(ts,...)`）。dream.rs:175 单次调用用段末 ts。spec §5.7 改 per-event `ev.last_ts` 后，合并组跨午夜（R2）或段跨 23:50→00:10 时，早一天的 final event 写了 day 文件但 MEMORY.md 那天今日行**不刷新**。现状 OK（共用 now），spec 改后 broken。**Fix**：收集所有 final event 的 distinct 日期，每个 upsert 一次。

### F3 · 段跨月 rotate 顺序错
**[P1] (conf 8/10) `dream.rs:161` + spec §5.7**
`seg_ym = ym_from_ts(now=段末ts)`，跨月检查只看段末月。若段跨月（上月最后一天 → 本月），`check_and_rotate_month`（`dream.rs:161`）在 extract/append **之前**跑 → 可能已把上月轮转为"上月冻结" + 跑月主题 LLM，**之后** per-event ts 在上月的事件才 append 到上月 day 文件 → 上月今日行 stale、月主题漏见这些事件、drill 指针可能断。spec R2 只覆盖"合并组跨午夜"。**Fix**：`rotate_month` 应在所有 event append 后；或段跨月时按事件 ts 分组分别判跨月。

### F4 · append_day_event 错误吞 → G1 静默失效
**[P1] (conf 9/10) spec §5.7 line 357 + §7 表**
spec 保留 `let _ = memory::append_day_event(...)`。新设计下每个 event 有独立 seq_range 覆盖不同回合——若 round k 的 append 失败（磁盘满/AV 扫描/权限），round k **零记忆**（无 day 段、无 MEMORY.md 行），随后 `upsert_current_month_today` 读 day_titles 写的 summary **漏掉 round k**，而 dream marker 仍写（成功路径）→ round k **永不重试**。**G1 完整性硬保证被静默破坏**。**Fix**：任一 append_day_event 返 Err → propagate，不写 marker、不推进 frontier，段重试（匹配 §13.1 失败语义）。"TODO"框架错——这是 correctness 不是 tech debt。

### F5 · 测试迁移：ScriptedDream shape + seq assert
**[P1] (conf 9/10) `dream.rs:457-739`**
现有 7 个 `run_dream_*`/`execute_dream_*` 测试：
- `ScriptedDream.content` 是旧 `{"title","detail","subject"}`（`dream.rs:466`），新 `parse_groups`（spec §5.5）要求 `{"rounds":[...]}` → 旧 content 每行被判 `rounds.is_empty()` → `bad_lines++` → **所有测试静默退化为只测 mechanical fallback 分支**。测试"绿"但 LLM-success 分支（真正的新功能）零覆盖。
- `dream.rs:476 assert!(mem.contains("seq[1,3]"))` 断言整段 [a,b]；新逻辑 seq 是 per-final-event 回合并集，不再是 [1,3]。

spec §8 列了"要测什么"，没列"现有 7 测试怎么改"。**Fix**：§8 加"Test migration"子节——ScriptedDream content 改 `{rounds:[1,2,...],title,detail,subject}`；每个现有测试 content 重写；加显式"LLM-success"测试断言 `stats.filled_by_mechanical == 0`。

### F6 · 合并非连续回合 → drill 返回其它组
**[P1] (conf 8/10) spec §5.6 + §6.1**
spec §6.1 声称"合并组 seq 并集 → `mem history --seq min_a..max_b` 还原整组"。仅当组内回合 **seq 连续**时成立。`reconcile_groups` 不强制连续——接受 `rounds:[1,5,9]`。若 LLM 合并非相邻回合：`seq_a=round1.seq_a, seq_b=round9.seq_b`，drill `[seq_a,seq_b]` 返回 round 1-9 **含其它组的回合**。G3 drill 能力被破坏。**Fix**（推荐 A）：`reconcile_groups` 检测非连续 → 拆成连续子组（各带 LLM title）。配测。

### F7 · prepare↔execute events 快照 race
**[P1] (conf 7/10) spec §5.1 + `dream.rs:149`**
`tail_aware_b` 在 prepare 用 events 快照算 b；execute 内部 `dream.rs:149` 又 read_all 一次。若 prepare↔execute 之间用户加了回合，两份 events 不一致——tail 按 prepare 快照算，execute 用新 events 过滤 `[a,b]` → tail 边界错位（该整的没整 / 该留的没留）。**Fix**：tail_aware_b 移入 execute_dream 内，用同一份 events（也是 split_rounds 用的那份）；prepare 只接 `tail_rounds` 参数不预算 b。

---

## P2 — DRY / 设计 / 优化

### F8 · read_all 3 次/trigger + dispatch_dream 锁模式
**[P2] (conf 9/10) `dream.rs:149` + `agent.rs:301` + spec §9**
透传 events 后：read_all（决策）+ read_all（prepare 若传入）+ read_all（execute:149）= **3 次/trigger**。且 `dispatch_dream` 现状是"决策持锁 read_all → 放锁 → prepare 另持锁"两个临界区（`agent.rs:308-323`），传 events 要么跨锁持有要么 clone 大 Vec。**Fix**：execute_dream 接 `events: &[HistoryEvent]`，删内部 read_all；dispatch_dream 一次 read_all 透传全程。plan §9 必须明确这个锁模式。

### F9 · struct 简化（CROSS-MODEL TENSION）
**[P2] (conf 8/10) spec §5.2/§5.3/§5.6**
- review 倾向（温和）：合并 `MechanicalEvent`+`FinalEvent`（字段仅差 round_idx）→ 5 struct 减到 4。
- outside voice（激进）：5 struct → 2（`Round` + `FinalEvent`）；`Group` 改 tuple `(Vec<usize>, String, String, String)`；`ReconcileStats` 改 `[u32;4]` 或 inline eprintln。6 fn → 4（`merge_event`/`tail_aware_b` inline）。
- **决策点**：简化程度。outside voice 更激进减面，review 保留 struct 可读性。

### F10 · evt_no 同源
**[P2] (conf 7/10) `memory.rs:36` + `dream.rs:172` + spec §5.7**
day 文件 evt-NNN 用 `day_event_count`（当天累计），MEMORY.md 日节 evt_no 用 `(i+1)`（本次序号）。一天两次 dream → 编号对不上，`mem` drill 难定位。**Fix**：evt_no 用 `day_event_count` 起算。

### F11 · MAX_ROUNDS 强制拆 vs 只 log（CROSS-MODEL TENSION）
**[P2] (conf 7/10) spec §5.6 + §7 表**
- spec 现状：超限只 `over_limit++` 记日志，接受不拆（不丢回合优先）。
- outside voice：log-only 是最差选项（既不强制也不退机械）；主张强制拆 max_rounds 块 或 超限退机械。
- **决策点**：拆 / 退机械 / 保持 log-only。MiniMax M3 数字约束遵从度一般，超限会真实发生。

### F12 · R3 空段 tight-loop
**[P2] (conf 7/10) spec §11 R3**
user 数 ≤ tail_rounds → `tail_aware_b < a` → 空段 → 早退不写 marker → frontier 不推进。idle 时 dream 每 tick 触发都空跑（dispatch_dream 在 UserMessage/JobDone/idle 都可能触发）→ CPU/日志空转。**Fix**（推荐 A）：空段时推进 marker 到 `current_seq`（接受 tail 永不被 dream——符合"停说 2h 就该整"的用户预期）；或加 cooldown。

### F13 · prompt 长 → 长会话总退机械
**[P2] (conf 7/10) spec §5.4 + N2 + §11 R4**
每回合 prompt 块 ~250-400 字（含"机械底座"title/detail 行）。cap=50 → 12-20K 字 prompt。output 行数多 → parse 失败率升 → 大量 mechanical fallback。**长会话正是合并最有价值处，增强却失效**。**Fix**（推荐 B，free）：去掉 prompt 里"机械底座"行（LLM 不需要，user+assistant 已是信号，机械底座是给兜底用的不是给 LLM）；或降 cap 到 20；或加 `bad_lines/total` 遥测阈值后 revisit chunking。

---

## CROSS-MODEL TENSION 汇总

| 点 | review 倾向 | outside voice | 推荐 |
|---|---|---|---|
| F9 struct 简化 | 温和（5→4，合并 Mech+Final） | 激进（5→2，砍 Group/Stats） | 看 user 趣味（explicit vs minimal） |
| F11 MAX_ROUNDS | 保持 log-only（不丢优先） | 强制拆或退机械 | outside voice 更对（log-only 最差） |

其余 findings：outside voice 加强 review（更深根因），无分歧。

---

## spec 必须修订的点（plan 前置）

1. **F1**：§4.2/§5.1/§11 R3 重写——build_messages 改认 until_seq（或明确留尾新机制）。
2. **F2/F3**：§5.7 upsert 按 distinct 日期多次；rotate_month 顺序移到 append 后 / 段跨月分组判定。
3. **F4**：§5.7 + §7 表——append_day_event 错误 propagate（不再 `let _ =`）。
4. **F5**：§8 加"Test migration"子节（ScriptedDream shape + seq assert + LLM-success 断言）。
5. **F6**：§5.6 reconcile_groups 检测非连续拆子组。
6. **F7**：tail_aware_b 移入 execute_dream。
7. **F8**：§9 明确 execute_dream 接 events + dispatch_dream 锁模式。
8. **F9-F13**：按 user 决策。

## NOT in scope（spec §3 已列，确认）
- history 脱敏（#008 关闭）、超长段分批（N2，但 F13 建议 prompt 瘦身）、触发策略、mem CLI、历史回填。

## What already exists（复用）
- `append_day_event`/`append_memory_day`/`upsert_current_month_today`（memory.rs）复用 ✓。
- `parse_extracts`/`dream_prompt`/`DREAM_SYS` 替换（非复用）。
- serde_json、ScriptedDream/DualDream 测试脚手架复用（content shape 改）。

---

## GSTACK REVIEW REPORT

| Review | Trigger | Why | Runs | Status | Findings |
|--------|---------|-----|------|--------|----------|
| Eng Review | `/plan-eng-review` | Architecture & tests (required) | 1 | ISSUES_OPEN | 1 P0 + 6 P1 + 6 P2 |
| Outside Voice | Claude subagent | Independent 2nd opinion | 1 | findings_absorbed | F1 P0 + 5 加强项 |

- **CROSS-MODEL:** Claude 漏 F1（marker.seq vs until_seq），outside voice 抓到——cross-model 价值的最强体现。F6/F4 也由 outside voice 首提。两 reviewer 在 F9/F11 有程度分歧（已列 tension 表）。
- **VERDICT:** ENG REVIEW — REVISE BEFORE PLAN。F1（P0）否决 spec 核心前提 G2，必须先定修复方向再进 writing-plans。

**UNRESOLVED DECISIONS:**
- F1 修复方向（A 改 build_messages 认 until_seq / B 重新设计 / C 砍 G2 / D 仅加测试）—— 待 user 拍。
- F9 struct 简化程度（温和 5→4 / 激进 5→2）—— 待 user 拍。
- F11 MAX_ROUNDS 超限处理（强制拆 / 退机械 / 保持 log-only）—— 待 user 拍。
