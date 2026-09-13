# 记忆机制（mem + dream）

ovoice 的记忆系统回答一个问题：**常驻助手如何在有限上下文里拥有一生的记忆**。答案是一条单向流水线：原始历史永远完整，压缩只发生在蒸馏层，蒸馏产物以"索引"形态常驻上下文。

```
history/*.jsonl（唯一真相源，append-only，永不改写）
        │  dream（唯一写者，LLM 驱动）
        ▼
┌─ 四级漏斗 ──────────────────────────────────────────┐
│ L1 日段  cache/memory/{Y}/{M}/{date}.md             │
│         ## HH:MM 标题 + 摘要 + seq[a,b] 指针         │
│ L2 月摘要  月界轮转时把当月日段蒸馏成月档             │
│ L3 年摘要  年界同理                                  │
│ L4 MEMORY.md  一生记忆的索引，每轮 pinned 进 system  │
└─────────────────────────────────────────────────────┘
        │  mem_*（只读工具，agent 主动调用）
        ▼
   agent 回忆：mem_list / mem_read / mem_search / mem_history
```

## 一、原始层：history JSONL

所有对话事件（user / assistant / tool_result / 子代理汇报 / marker）按序落盘 `cache/history/YYYY-MM-DD.jsonl`，带全局递增 `seq`。它是唯一真相源：上下文重建、dream 输入、前端回放都读它。**没有任何流程改写或删除它**——记忆系统的全部安全性都建立在"原料永不损毁"上。

## 二、蒸馏层：四级漏斗

### L1 日段（`memory.rs`）

dream 按"活动段"（user 输入 / spawn 子代理 / 5 分钟时间间隔为界）把历史事件切成回合，LLM 蒸馏后追加为当日 Markdown 文件里的一个事件段：

- 路径 `cache/memory/{Y}/{M}/{YYYY-MM-DD}.md`，标题 `## HH:MM`，正文是蒸馏摘要；
- 每段由**代码盖戳**对话索引 `seq[a,b]`（P1 铁律③）——指向 history 里对应的原始区间，`mem_read` 可凭它回放原文；
- `evt_no` = 文件里已有 `##` 段数 + 1，跨进程一致。

### L2 / L3 月与年

月界（跨月首跑）与年界轮转：把上一个月/年的日段（或月档）再蒸馏一层，逐级上卷。级别越低越接近原文，越高越抽象——查近期细节走日段，回忆"去年这时候"走年档。

### L4 MEMORY.md 索引

漏斗的塔尖，**一生记忆的目录**。它不是记忆本身，而是"什么记忆存在、在哪一层"的索引。它被 pinned 进每轮的 system 消息（与 SOUL.md / AGENT.md 同级），所以模型永远"知道自己记得什么"，需要细节时用 mem 工具下钻——这是"始终拥有一生记忆"的实现方式：索引常驻，原文按需取。

## 三、触发与调度

| 触发 | 条件 | 入口 |
|---|---|---|
| 空闲整理 | idle ≥ `dream_idle_secs`（默认 7200s = 2h） | 60s ticker 投 `DreamCheck`，driver 串行判定 |
| 容量整理 | 最近一轮 context `prompt_tokens` ≥ `dream_context_trigger_tokens`（默认 300k，服务器真实计数） | 同上 |
| 主动整理 | agent 调 `dream` 工具（`force=true`，跳过阈值但仍守安全门） | 工具同步返回收据 |

安全门（防并发/空转）：`armed` + 不 `in_flight` + 自上次 marker 后确有新的 main 线程事件。整理**异步后台跑，不打断当前回合**。

容量参数：`dream_merge_max_rounds`（留尾回合数——最近的 user 回合不进 dream，下轮上下文仍可见）与 `dream_batch_max_events`（单批事件数上限，防 LLM 长上下文失焦）。

## 四、上下文窗口的推进

dream 完成后写入一条 `marker` 事件并推进 `until_seq`。上下文重建（`build_messages`）读到最新 marker 即**硬切**：只发送 marker 之后的事件。于是"整理多少、忘掉多少"由同一个动作原子决定——记忆进入漏斗的瞬间，原文退出工作集（但仍躺在盘上，`seq` 指针随时可回放）。

## 五、只读工具（agent 视角）

| 工具 | 作用 |
|---|---|
| `mem_list` | 列出记忆文件（按日/月/年层级浏览） |
| `mem_read` | 读指定日段/月/年档正文 |
| `mem_search` | 关键词检索记忆文件 |
| `mem_history` | 按 `seq[a,b]` 指针回放 history 原文（蒸馏 → 原文的下钻闭环） |

`memory/` 只有 dream 一个写者，四个工具全部只读——零写竞态，读永远一致。

## 六、一致性约束（实现者必读）

- **时间同源**：日段文件名/标题用本地时间，offset 必须与 history writer 同源（同一个 `local_offset_secs()`），否则 `seq[a,b]` 指针会指向错误日期的文件而断裂；
- **蒸馏不截断**：dream 的输入序列化可以截断展示细节，但**结论性内容必须来自全文**——落盘层（含子代理结算汇报）永远全文，这是设计哲学第 3 条在记忆子系统的落地；
- **token 记账**：每次 dream 的 prompt/cached/completion 记入 marker 事件，成本可审计。
