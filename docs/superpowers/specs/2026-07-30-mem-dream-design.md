# mem dream —— 记忆整理迁入 mem CLI（四级漏斗记忆）

- **日期**：2026-07-30（修订：2026-07-31，布局改为各级独立索引文件）
- **2026-07-31 存储决策变更（最新，以此为准）**：MEMORY.md 从「两级入口（`## 近期` + `## 年总览`）」改为「**三层总览（`## 日` + `## 月` + `## 年`）—— 一个 MEMORY.md 包含一生记忆」；`memory/` 树（年/月/日独立文件）**保留不动**（明细，可下钻）。`## 月` = 当月+上月（2 月窗口），`## 年` = 月主题汇总（一生，**废弃**原「年度概要」）。§7.1 / §7.5 / §9 / §11 / §23 已更新；其余 section 仍可能含旧术语（近期 / 年总览 / 年度概要 / 顶层入口），**一律以 §7.1 + §23 决策记录为准**。详见 `docs/dream-user-expectations.md` 第七节。
- **状态**：设计待 review（未实现）
- **实现分支**：`feat/mem-dream`（须新建，严禁直接落 main / 不在 `feat/dream-completeness` 上叠）
- **相关**：ovoice 现有 dream 子系统（`src-tauri/src/dream.rs` + `agent.rs`）—— 本设计**不动**它，过渡期并存

---

## 1. 背景与动机

ovoice 现状：dream 是主程序内嵌的静默后台子系统（`dream.rs` 写端 + `agent.rs` 的 `DreamTrigger` 触发），memory 区只有"日级"实体（每天一个 day 文件），月主题和日概括都挤在单个 `MEMORY.md` 里，且月层是 2 月窗口（历史月日概括轮转退出）。

三个问题：
1. **架构**：dream 耦合在 GUI 主程序里，无法独立运行 / 测试 / 复用。
2. **记忆范式**：不是"可一生下钻的层级记忆"。没有"从年到月到日到对话"的逐层定位。
3. **总结无归处**：月总结、年总结是重要产物，现状混塞 `MEMORY.md` 并随窗口滚出，没有独立持久归档。

**本设计目标**：把 dream 迁入 mem CLI（mem 从纯读端升级为读写端），按**四级漏斗**范式重组记忆——每级一个**独立索引文件**，永久归档，逐层下钻。

## 2. 核心理念：四级漏斗记忆

记忆永不遗忘，但回想时**从粗到细逐层定位**，每层只加载"概括"，绝不全量塞对话：

```
年   · 月主题汇总（## 年，一生）       → 联想，定位到年/月
 ↓
月   · 该年月主题索引 / 该月日概括       → 联想，定位到天
 ↓
日   · memory/ 该日事件列表（day 文件）  → 定位到事件
 ↓
对话 · 该事件的原始上下文（history seq 指针）
```

每一层是**下一层的索引/概括**，服务于"AI 容易联想和索引"。分工：
- **dream（写）**：把对话整理成这套层级（事件 → 日概括 → 月主题 → `## 年` 月主题汇总）
- **mem（读）**：逐层下钻提取（`ls` / `read` / `index` / `history` 是下钻工具）

## 3. 目标 / 非目标

### 目标
- mem CLI 新增 `dream` 子命令，**整体重写** dream 处理逻辑（不调 `dream.rs` / `memory.rs`，只参考），自包含可独立跑。
- memory 区重组为**各级独立索引文件**（年/月/日）+ **MEMORY.md 三层总览**（`## 日/月/年`，一生）。
- day 文件格式**不变**（已有数据无损复用）。
- 与 ovoice 内嵌 dream 过渡期并存，写同一 cache，互通。
- mem `read` 按粒度路由（`YYYY` / `YYYY-MM` / `YYYY-MM-DD`），支持双路径检索（联想定位 + 直接指定，见附录 A）。

### 非目标（后续阶段，本次不做）
- ovoice 内嵌 dream 的移除 / 切换。
- ovoice 通过 bash 调 `mem dream` 的接线。
- 触发时机归属（ovoice 侧 `DreamTrigger` 是否改用 bash 调 mem）。
- 前端可见性（dream 仍可静默；stdout 天然有，前端接入后续）。
- 保留期调整（day 文件 / 月级 / 年级文件都永久；不删）。

## 4. 架构总览

```
┌─────────────────────────────────────────────────────────┐
│  ovoice 主程序（GUI）                                    │
│    内嵌 dream（dream.rs + agent.rs）—— 过渡期不动        │
│    未来：移除内嵌 dream，改 bash 调 mem dream（后续阶段） │
└─────────────────────────────────────────────────────────┘
                       │ 写同一 cache（过渡期并存）
                       ▼
┌─────────────────────────────────────────────────────────┐
│  cache/                                                 │
│    MEMORY.md            三层总览（## 日/月/年，一生）    │
│    .dream-meta.json     current_month + frontier        │
│    history/*.jsonl      对话级（原始）                   │
│    memory/YYYY/YYYY.md         年级（月主题索引）        │
│    memory/YYYY/MM/YYYY-MM.md   月级（月主题+日概括）     │
│    memory/YYYY/MM/YYYY-MM-DD.md 日级（事件段）          │
└─────────────────────────────────────────────────────────┘
                       ▲ 读/写
                       │
┌─────────────────────────────────────────────────────────┐
│  mem CLI（独立 exe，本设计）                             │
│    dream  ← 写端：四级漏斗整理（本设计核心）             │
│    ls/read/index/history/search  ← 读端：下钻（已有）    │
└─────────────────────────────────────────────────────────┘
```

## 5. 模块边界（自建 vs 复用）

| 层 | 归属 | 说明 |
|---|---|---|
| dream 算法（split/mechanical/tail/parse/reconcile/循环驱动） | **mem 自建** | 参考 `dream.rs`，去 ovoice 运行时耦合（`DreamTrigger` / events 透传 / `Emitter`） |
| 写盘（day / MEMORY.md / 月级 / 年级 / marker / meta） | **mem 自建** | 不调 `memory.rs`；格式严格对齐（硬约束，见 §7） |
| LLM（MiniMax 调用） | **mem 自建** | reqwest 非流式 chat completion（dream 只需一次性 JSON，不需流式/tool_calls） |
| `history.rs`（读 jsonl / `HistoryEvent` / marker 解析） | **复用** | 数据契约，`mem_cli.rs:3` 已在用 |
| `config.rs`（cache 定位 + api_key/model/region） | **复用** | `bin/mem.rs` 的 `resolve_cache` 已在用 |

**新模块**：`src-tauri/src/mem_dream.rs`（mem 专用 dream 实现）。`bin/mem.rs` 加 `dream` 分支（async）。`mem_cli.rs`（只读命令）不动。

## 6. memory 区物理布局（各级独立索引文件）

```
cache/
├── history/                          【对话级】原始对话（不动）
│   └── 2026-07-30.jsonl
├── MEMORY.md                         【三层总览】## 日/月/年（一生）
├── .dream-meta.json                  current_month + frontier（保留）
└── memory/
    └── 2026/                         按年目录
        ├── 2026.md                   【年级】2026 月主题索引（每月一行）
        └── 07/                       按月目录
            ├── 2026-07.md            【月级】月主题正文 + 每天日概括
            ├── 2026-07-30.md         【日级】当天事件段（格式不变，永久）
            └── 2026-07-29.md
```

**每级一个独立索引文件，永久归档**：
- 日级 `memory/YYYY/MM/YYYY-MM-DD.md`：当天事件段（现状格式，永久）。
- 月级 `memory/YYYY/MM/YYYY-MM.md`：该月**月主题正文**（顶部）+ **每天日概括**（每天一行）。当月持续更新，跨月冻结。永久。
- 年级 `memory/YYYY/YYYY.md`：该年**月主题索引**（每月一行精简）。跨月时追加。永久。
- 顶层 `MEMORY.md` 三层总览：**`## 日`**（当天 per-event）+ **`## 月`**（当月+上月日概括）+ **`## 年`**（月主题汇总，一生）。汇总自 memory/ 树各级文件。
- 对话级 `history/*.jsonl`：原始（不动）。

**月级/年级都是独立持久文件**——不再有"2 月窗口轮转丢历史"。每月/每年总结都有自己的归处。

day 文件路径：`memory/{年}/{月}/{年}-{月}-{日}.md`（两位月，沿用 `memory.rs:13-20` 的 `day_file_path` 规则）。月级/年级文件与 day 文件同目录，按文件名格式区分（`YYYY-MM.md` 月级 vs `YYYY-MM-DD.md` 日级；`YYYY.md` 在年目录根）。

## 7. 各级文件结构与格式

### 7.1 `MEMORY.md`（一生总览，三层）
```markdown
# 记忆（MEMORY.md）

由 dream 维护。三层总览（一生记忆）：日 / 月 / 年。**汇总自 memory/ 树各级文件**（§7.2-7.4 实体明细），一个文件看一生。

## 日

- 14:30 evt-20260730-005 修 drag-drop
- 10:00 evt-20260730-004 写 dream 文档

## 月
### 当月 2026-07
- 2026-07-30: 围绕 dream 子系统重构 | drag-drop·F10·MEMORY·月轮转 | 5事件 [seq 23-31]
- 2026-07-29: ...
### 上月 2026-06（冻结）
- ...

## 年

- 2026-07: <月主题>
- 2026-06: <月主题>
- 2025-12: <月主题>
```

- `## 日`：今天事件 per-event（仅当日，滚动）。明细见 `memory/YYYY/MM/YYYY-MM-DD.md`（§7.4）。
- `## 月`：日概括，**只 `### 当月` + `### 上月`（2 月窗口）**，汇总自月级文件（§7.3）。当月每日 upsert；跨月时旧月 → `### 上月`（冻结）。**更早月份不进 MEMORY.md，只在磁盘月文件永久保留**（§7.3）。
- `## 年`：**所有月份月主题汇总**（每月一条，永久累积 = 一生），汇总自年级文件（§7.2）。

### 7.2 `memory/YYYY/YYYY.md`（年级 = 月主题索引）
```markdown
# 2026 月主题索引

- 2026-07: <月主题首句/精简>
- 2026-06: <月主题首句/精简>
...
```
每月一行（月主题精简），用于"看该年、定位月"。月主题**完整正文**在月级文件。

### 7.3 `memory/YYYY/MM/YYYY-MM.md`（月级 = 月主题正文 + 日概括）
```markdown
# 2026-07

## 月主题
<该月完整月主题正文（LLM 综合）>

## 日概括
- 2026-07-30: 围绕 dream 子系统重构 | drag-drop·F10·MEMORY·月轮转 | 5事件 [seq 23-31]
- 2026-07-29: ...
```
- `## 月主题`：完整月主题正文（跨月时 LLM 生成，见 §10）。
- `## 日概括`：每天一行（规则提炼，见 §9），用于"看该月、定位天"。

### 7.4 `memory/YYYY/MM/YYYY-MM-DD.md`（日级 = 事件段，沿用不变）
```
## HH:MM evt-YYYYMMDD-NNN 标题
**主语**: {subject}
**详情**: {detail}
**对话索引**: history/{date}.jsonl#seq[a,b]
**附件**: {attachment}            ← 仅当有附件
```
- append-only，dream 是当天唯一写者，跨天冻结旧文件。
- `evt-NNN`：写前计数（F10 同源）；`对话索引`：代码盖戳（P1③）；本地时区。

### 7.5 MEMORY.md 拼接算法（确定性 + 可靠性契约）

MEMORY.md 是一生总览，dream 是**唯一写者**。拼接是**确定性 Rust 逻辑**（mem_dream 自建，不调 dream.rs/memory.rs，保持自包含 §3）——「读 → 滚 → upsert → 渲染 → 写」整个组装过程不调 LLM；月主题正文存磁盘月级文件（§7.3，LLM 生成见 §10），MEMORY.md `## 年` 只放月主题精简。须满足下列可靠性契约。

**拼接函数 `upsert_memory_md(path, events_out, now_date, seg_ym, meta_current_month)`**：
1. **读**：上一版 MEMORY.md；不存在 → 用模板初始化（`# 记忆` header + 空 `## 日` + 空 `## 月`（`### 当月`/`### 上月`）+ 空 `## 年`）。
2. **解析**：切成 `{ header, day: Vec<Line>, month: {current: Vec<Line>, previous: Vec<Line>}, year: Vec<Line>, other: Vec<Raw> }`。
3. **## 日**：滚出 `ts 日期 ≠ 今天` 的行；当日 FinalEvent 按 `evt-NNN` upsert（`nn` 复用 day 文件返回值，F10 同源）；按时间倒序。仅当日。
4. **## 月**：`### 当月` 段按日期 upsert 当天日概括（§9）；跨月（`seg_ym > meta_current_month`）时把旧当月 → `### 上月`（冻结，2 月窗口），新空当月。
5. **## 年**：跨月时追加 `- YYYY-MM: <月主题精简>`，永久累积（一生）。
6. **结构校验**：渲染前校验骨架（`# 记忆` + `## 日` + `## 月` + `## 年` 顺序齐全，缺则报错回退）。
7. **原子写**：`write(<path>.tmp)` → `fsync` → `rename` 覆盖 `<path>`；写中崩溃只留 `.tmp`，原文件不动。
8. **备份**：写前 `cp <path> <path>.bak`（单份滚动），手动可回滚。

**解析容错**（上一版可能被手改 / 半截写 / 旧格式）：
- 按 `## 标题` 切段；只认 `## 日`、`## 月`、`## 年`，**未知段原样保留**（不丢手写内容）。
- 格式不符的行 → 跳过 + 日志 warning，**不整体失败**（dream 仍推进）。
- 完全无法解析（空 / 严重损坏）→ 模板重建，不中断。

**可靠性契约（plan 验收点）**：
- **幂等**：重整同一 [a,b] → MEMORY.md 字节不变（写前去重 + 原子写）。
- **原子**：写中崩溃 → 文件完整（旧版或新版），无半截损坏。
- **不丢手写**：只动 `## 日` + `## 月` + `## 年`，未知段/行原样保留。
- **可回滚**：`.bak` 保留上一版。
- **失败回退**：IO / 解析致命错误 → propagate Err，**不写 marker**（F4），下次重整同一 [a,b]。
- **结构恒定**：每次写完满足骨架顺序（`# 记忆` + `## 日/月/年`），不写歪。

### 关键不变量
- `## 日`（MEMORY.md）只含当日事件；dream 每次跑后把 `ts` 日期 ≠ 今天的旧行删除。
- 月级/年级文件**永久**，不轮转、不删除。
- 当月 `YYYY-MM.md` 持续 upsert 当天日概括；跨月时写月主题正文后冻结。
- 年级 `YYYY.md` 跨月时追加该月一行；MEMORY.md `## 年` 跨月时追加该月月主题（一生累积）。

## 8. day 文件格式（沿用现状，不变）

见 §7.4。`memory/{Y}/{M}/{date}.md`，append-only，事件段含代码盖戳的 seq 对话指针。

## 9. 日概括成色（规则提炼，默认）

月级 `YYYY-MM.md` 的 `## 日概括` 每天行 + MEMORY.md `## 月`（当月日概括）+ `## 日`（今天 per-event）。默认**规则提炼**（不调 LLM），后面拿实际数据试了再调。

### 设计原则：让 AI 容易联想和索引
概括保留**具体性**——靠具体词锚定联想、靠结构化便于索引。不抽象成泛泛动词。

### 4 类要素
1. **关键词/标签**（最关键）：具体名词（函数名/文件/工具/项目/人名）。联想锚 + `mem search` 入口。
2. **一句话主线**：跨事件提炼（"围绕 X"）。
3. **关键实体**：文件路径/函数名/外部工具（下钻锚）。
4. **元信息**：事件数、seq 范围（下钻对话）。

### 月级日概括格式
```
- {YYYY-MM-DD}: {一句话主线} | {关键词·实体} | {N}事件 [seq {a}..{b}]
```
示例：`- 2026-07-30: 围绕 dream 子系统重构 | drag-drop·F10·MEMORY·月轮转 | 5事件 [seq 23-31]`

### `## 日` per-event 行格式
`- {HH:MM} evt-{YYYYMMDD}-{NNN} {标题}` —— 用 LLM 增强的 per-event 标题（`## 日` 是今天事件列表，`## 月` 才是日概括）。

### 规则提炼来源
- **关键词/实体**：从当天事件 user 文本 + LLM title/detail 抽具体标识符（`/[A-Za-z_][A-Za-z0-9_]+/`、文件路径、已知词表），词频取 Top-N。
- **主线**：聚合当天 per-event 标题，取最高频主题词缀成"围绕 X"。
- **元信息**：`事件数 = 当天段数`，`seq = 当天事件 [min,max]`。

## 10. 月主题（LLM，完整正文存月级文件）

跨月时 LLM 综合旧月全部日段生成一段月主题（沿用 `dream.rs:524` `MONTH_THEME_SYS` 思路）：
- **完整正文** → 写入旧月 `YYYY-MM.md` 的 `## 月主题`（冻结归档）。
- **精简一行**（月主题首句）→ 写入年级 `YYYY.md`（`- YYYY-MM: <精简>`，月索引）。

**prompt 引导**：月主题须含**具体关键词**（主要活动、关键人物、重要产出/转折），不写抽象概括——服务联想+索引。

## 11. `## 年`（月主题汇总，存 MEMORY.md）

MEMORY.md 的 `## 年` = **所有月份月主题汇总**（每月一条 `- YYYY-MM: <月主题精简>`，永久累积 = 一生）。跨月时 dream 把该月主题精简追加到 `## 年`（月主题正文存月级文件 §7.3，LLM 生成见 §10）。

**不另设"年度概要"**（原两级入口设计的年度概要规则提炼已废弃）—— `## 年` 就是月主题汇总，一年的概貌由该年 12 条月主题呈现。

## 12. mem dream 命令接口

```
mem dream                    自主跑，循环到空（默认）
mem dream --mechanical       强制纯机械，不调 LLM（离线/测试）
mem dream --once             只跑一段（a→b 留尾）就停
mem dream --dry-run          只算不写（打印会整理什么）
```

- 无参默认 = 循环跑到空（反复 a→b 直到空段）。
- `main` 不全 async（其他子命令保持同步）；`dream` 分支单独 `tokio::runtime::Runtime::block_on`。
- 退出码：`0` = 正常（含机械兜底）；非 `0` = 致命（缺 api_key 且非 `--mechanical` / IO 失败 / 配置错）。

## 13. I/O 契约

**输入**
- `history/*.jsonl`（对话历史 + 已有 dream marker；marker 算 frontier）
- 上一版 `MEMORY.md` + 现有 `memory/YYYY/YYYY.md` + `memory/YYYY/MM/YYYY-MM.md`
- `.dream-meta.json`（`current_month`）
- `config.json`（api_key / model / region）

**输出**
- `MEMORY.md`（`## 日/月/年` 三层，增量编辑）
- `memory/YYYY/YYYY.md`（年级月主题索引，跨月追加）
- `memory/YYYY/MM/YYYY-MM.md`（月级月主题正文 + 日概括，当月 upsert / 跨月写主题冻结）
- `memory/YYYY/MM/YYYY-MM-DD.md`（day 文件 append）
- `history/*.jsonl`（新 dream marker append，推进 frontier）
- `.dream-meta.json`（跨月时更新 `current_month`）

## 14. dream 处理流程（frontier 机制 + 循环跑到空）

### 14.1 frontier 机制：怎么判断已处理 / 未处理

history.jsonl 每个事件有唯一递增 `seq`（user/assistant/tool_result/marker 都算）。dream 每整理完一段 [a,b]，在末尾 append 一个 **dream marker**，其 `until_seq = b` 就是 frontier（提取前沿）：

```json
{"seq": M, "kind": "marker", "data": {"marker": "dream", "until_seq": b}, "ts": ...}
```

- **已处理** = seq ≤ 最后一个 dream marker 的 `until_seq`
- **未处理** = seq > `until_seq` 的对话事件

**mem dream 无状态**：每次跑都从 history 读最后一个 dream marker 的 `until_seq` 当 frontier（marker 即真相，无需内存状态 / seed，重启不丢）。

**统一用 `until_seq`（修正现状）**：ovoice 现状 `DreamTrigger::seed_from_history`（`dream.rs:52-61`）用 marker **事件自己的 seq** 当 frontier，而运行中 `run_dream` 推进用 `until_seq`——两者不一致，重启后 `marker.seq > until_seq` 会**跳过留尾**。mem dream 统一用 `until_seq`，重启 / 运行一致，留尾不丢。

**推进示例**（`tail_rounds=3`）：
```
history: 1 user A | 2 asst | 3 user B | 4 asst | 5 user C | 6 asst | 7 user D | 8 asst | 9 user E
【第1次】frontier=0 → a=1, cur=9, tail 留 E/D/C → b=5, 整理[1,5], marker(until_seq=5) → frontier=5
         下轮 a=6, tail 只剩 E,D(<3) → 空段 break
【新对话】11 user F | 12 asst | 13 user G   (cur=13)
【第2次】frontier=5 → a=6, tail 留 G/F/E → b=9, 整理[6,9], marker(until_seq=9) → frontier=9
```
每次从旧 frontier 推到新 frontier（留最近 3 个 user），循环到只剩 ≤3 个 user 在尾 = "跑到空"。绝不回头整理 marker 之前的事件。

### 14.2 循环流程

```
resolve_cache → 读 config(api_key/model/region)
loop:
  events = read_all(history/)                          # 复用 history::read_all
  frontier = 最后 dream marker 的 until_seq（无 → 0）   # 无状态，每次重读
  a = frontier + 1
  cur = main 非 marker 最大 seq
  b = tail_aware_b(events, cur, tail_rounds=3)         # 自建，参考 dream.rs:364
  if b < a: break                                       # 空段 = 跑到空
  段 = events[seq∈a..b, main, 非marker]
  回合 = split_rounds(段)                               # 自建
  机械底座 = 每回合 mechanical_extract                  # 自建
  跨月检查（段末月 vs .dream-meta current_month）       # 自建（见 §16）
  groups = LLM增强(回合)；失败 → groups=∅（全机械）      # 自建 reqwest
  events_out = reconcile_groups(groups, 机械, max=5)    # 自建
  落盘 5 处（见 §15）+ 写前去重（见 §17）
  写 dream marker(until_seq=b) → frontier = b
  # 下轮 a = b+1
打印 stats → stdout
```

## 15. dream 写责任（5 处）

每次整理出 `events_out`（一批 FinalEvent）后：
1. **day 文件** `YYYY-MM-DD.md`：每个 FinalEvent → append 事件段（§7.4，返回 `nn`）。
2. **`MEMORY.md ## 日`**：把 `events_out` 里 **`ts` 日期 = 今天** 的 FinalEvent → 插入 `- {HH:MM} evt-{YYYYMMDD}-{nn} {标题}` 行；同时**滚出 `## 日` 里 `ts` ≠ 今天的旧行**。`ts` ≠ 今天的 FinalEvent 不进 `## 日`。
3. **当月 `YYYY-MM.md ## 日概括`**：对 `events_out` 涉及的每个 distinct 日期 → upsert 该天日概括行（§9 规则提炼）。
4. **跨月时**（§16）：旧月 `YYYY-MM.md` 写月主题正文（冻结）；旧月年 `{旧月年}.md` append `- {旧月}: <月主题精简>`（文件不存在则新建）。
5. **MEMORY.md `## 年`**（跨月时，§16）：追加 `- {旧月}: <月主题精简>`（月主题汇总，§11，一生累积）。

## 16. 跨月 / 跨年

段末月 `seg_ym` vs `.dream-meta.json` 的 `current_month`：
- 无 meta → 初始化 `current_month = seg_ym`，不跨。
- `seg_ym > current_month`（跨月）→
  1. LLM 综合旧月成月主题（失败 → propagate，不轮转，下次重试）；
  2. 旧月 `YYYY-MM.md ## 月主题` 写完整正文（该文件其余日概括已在，冻结归档）；
  3. 年级 `{旧月年}.md` append `- {旧月}: {月主题精简}`（月主题精简入年级文件，不存在则新建）；**同时** `MEMORY.md ## 年` append `- {旧月}: {月主题精简}`（月主题汇总，一生累积，§11）；
  4. 写 meta `current_month = seg_ym`。
- 否则（同月/未来月）→ 不动。
- 任一步失败 → propagate Err，**不写 meta**，下次 `mem dream` 仍检测到跨月重试。

## 17. 错误处理与退出码

| 场景 | 行为 | 退出码 |
|---|---|---|
| LLM 失败（网络/HTTP/解析） | catch → `groups=∅` 全机械兜底，仍写 marker 推进 frontier（G1/G4） | 0 |
| IO 失败（写 day/MEMORY/月级/年级/marker） | propagate Err，不写 marker，下次重试（F4） | 非 0 |
| 空段（`b<a`） | 写 marker(cur) 推进，break 循环（F12） | 0 |
| 缺 api_key 且非 `--mechanical` | 报错退出 | 非 0 |
| 跨月/跨年任一步失败 | propagate，不写 meta，下次重试 | 非 0 |

**产物重复防护（写前去重）**：整理 [a,b] 先写一批产物（day 段 / MEMORY `## 日` / 月级日概括 / 年级）再写 marker；若中途 IO 失败（F4）不写 marker，下次重整同一 [a,b]。为防同一事件重复落盘、evt-NNN 跳号，写盘函数**写前去重（幂等）**：
- **day 文件**：append 段前查该事件的 seq 范围是否已存在（day 段含 `对话索引 history/...#seq[a,b]`，命中则跳过）；
- **MEMORY `## 日` / 月级日概括**：按日期 upsert（天然幂等）；
- **年级月主题**：按月 upsert（天然幂等）。

这样重试不会产生重复段或跳号 evt-NNN。`MEMORY.md` 拼接另有原子写 + 解析容错 + `.bak` 备份（见 §7.5）。

## 18. 完整性不变量（从 ovoice dream 继承语义）

mem dream 重写时须保持这些语义：
- **F1 留尾**：`tail_aware_b` 留最近 `tail_rounds=3` 个 user 不整理。
- **F4**：append day 任一 `Err` → propagate，不写 marker。
- **F6**：reconcile 非连续回合拆子组。
- **F7**：`b` 在 execute 内算（同源 events）。
- **F8**：events 全程透传（单次 `read_all`）。
- **F10**：`evt-NNN` 写前计数，day 与 MEMORY `## 日` 行同源。
- **F11**：单组超 `max_rounds=5` 强制拆，加 `(续K)` 后缀。
- **F12**：空段（`b<a`）写 marker(cur) 推进，不 tight-loop。
- **G1**：机械兜底全覆盖（LLM 漏的回合机械补）。
- **G4**：LLM 失败不阻塞（catch 全机械）。
- **P1③**：seq 对话索引代码盖戳。

## 19. 与现状数据迁移（无损）

旧 dream.rs 数据 = 三层 `MEMORY.md`（`## 日/月/年`）+ day 文件。新设计 `MEMORY.md` **也是三层**（结构相同），故 **MEMORY.md 无需重组**，只**补建** memory/ 树的月/年级文件。

- **`MEMORY.md`**：结构不变（旧 `## 日/月/年` → 新 `## 日/月/年`，完全一致）。仅校验骨架齐全（缺则补模板），**不动已有内容**。
- **day 文件** `YYYY-MM-DD.md`：格式不变，已有数据直接复用。
- **补建月级文件** `YYYY-MM.md`（旧 dream.rs 无此文件）：对已有月份从 day 文件**回填** `## 日概括`（每天日概括，规则提炼 §9）；`## 月主题` 若无 LLM 数据则留空或标"（待补）"，下次跨月/手动补。
- **补建年级文件** `YYYY.md`（旧 dream.rs 无此文件）：对已有年份从 `MEMORY.md ## 年` 月主题**回填**（按年分组，每月一条）。
- **`.dream-meta.json`**：保留（`current_month` 复用）。
- **history dream marker**：保留（frontier 复用，mem/ovoice 共享）。
- 迁移幂等：已是新格式（memory/ 树月/年文件已存在）则不动。

## 20. 代码组织

### 新模块 `src-tauri/src/mem_dream.rs`
职责（函数意图，实现由 plan 细化）：
- `run_dream_to_idle(cache, cfg, mechanical) -> Result<Stats, Error>`：循环跑到空的入口。
- dream 算法（自建，参考 `dream.rs`）：`split_rounds` / `mechanical_extract` / `tail_aware_b` / `parse_groups` / `reconcile_groups` / `build_dream_messages`。
- LLM（自建 reqwest）：`dream_round(messages, cfg) -> Result<String, Error>`（非流式，取 `choices[0].message.content`）；`build_month_theme(...)`。
- 写盘（自建，参考 `memory.rs` 三层结构）：
  - `append_day_event`（day 文件事件段，§7.4）
  - **MEMORY.md 三层**（§7.1）：
    - `upsert_memory_day`（`## 日` 当天 per-event，滚动）
    - `upsert_memory_month`（`## 月 ### 当月` 日概括 upsert；跨月旧月 → `### 上月` 冻结，2 月窗口）
    - `append_memory_year`（`## 年` 月主题汇总，跨月追加，一生累积）
  - **memory/ 树明细**：
    - `upsert_month_day_summary`（月级 `YYYY-MM.md ## 日概括`）
    - `write_month_theme`（月级 `YYYY-MM.md ## 月主题`，跨月 LLM 正文）
    - `append_year_month_index`（年级 `YYYY.md` 月主题索引）
- 日概括规则提炼：`extract_day_summary(events_of_day) -> DaySummary`。（**无**年度概要提炼——年度概要已废弃，见 §11）
- frontier：`last_dream_marker_seq(events) -> u64`。

### `bin/mem.rs`
- `dispatch` 加 `Some("dream") => ` 分支（async，不走同步 `-> String` 模式；直接打印 + 返回退出码）。
- `main`：dream 分支 `block_on`，其他子命令保持同步。
- `usage()` 加 `dream` 说明。

### `lib.rs`
- `pub mod mem_dream;`

### `mem_cli.rs`
- **不动**（只读命令保持；`mem read` 按粒度路由见 §22.3）。

## 21. main async 化

mem 现在 `fn main()` 同步。dream 分支单独 async：
```rust
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cache = resolve_cache(&args);
    let clean = strip_loc_flag(&args);
    if clean.get(1).map(|s| s.as_str()) == Some("dream") {
        let code = block_on_dream(&clean, &cache);   // 内建 tokio Runtime + block_on
        std::process::exit(code);                     // dream 走退出码，不走 println
    }
    let out = dispatch(&clean, &cache);
    println!("{out}");
}
```
`block_on_dream` 内建 `tokio::runtime::Runtime::new()?.block_on(async { mem_dream::run_dream_to_idle(...).await })`，返回退出码（0 正常 / 非 0 致命）。dream 分支用 `std::process::exit` 终止，**不走** `dispatch` 的 `println` 路径（dream 自己往 stdout 打 stats + 错误）。

## 22. 待定项（open questions）

1. **锁文件**（防 mem dream 与 ovoice 内嵌 dream 同时写）：过渡期建议加 `.dream-lock`（mem 和 ovoice 跑 dream 前检查/创建，退出释放）。**未最终拍板**——简单靠"idle/cap 间隔长 + 手动 mem dream 不同时"规避也可。ovoice 移除内嵌 dream 后此问题消失。→ **spec review 时定**。
2. **日概括规则提炼的词表/阈值**：默认实现后拿实际数据试，再调（用户已认可“先默认，后调”）。（年度概要已废弃，§11）
3. ~~**年度总结是否上 LLM**~~（**废弃**：年度概要随 §11 取消，`## 年` 用月主题汇总，不再有年度概要提炼）。
4. **跨多月/跨多年回填**：若 dream 长期未跑，`seg_ym` 跳过多个 `current_month`（如 2026-07 → 2027-03），§16 只生成 `current_month` 月主题后轮转到 `seg_ym`，中间月主题缺失（与现状 dream 语义一致）。是否循环补全中间月主题，后续定。

> `mem read` 按粒度路由（`YYYY`/`YYYY-MM`/`YYYY-MM-DD`）已纳入本次目标（§3 + 附录 A.3），不再列为待定。

## 23. 决策记录（已拍板）

| 决策 | 选择 | 理由 |
|---|---|---|
| dream 去向 | 迁入 mem CLI | 解耦 GUI；mem 成为 dream 最终归宿 |
| 触发接口 | `mem dream` 自主跑无参 | mem 无状态 CLI 最自然 |
| 一次跑多少 | 循环跑到空 | bash 调一次清完积压 |
| 实现方式 | 整体重写，完全独立 | mem 成为唯一版本 |
| LLM 调用 | mem 自建 reqwest 非流式 | dream 只需一次性 JSON |
| 记忆范式 | 四级漏斗（年/月/日/对话） | 一生可下钻 |
| **索引文件** | **各级独立文件**：`YYYY.md`（年级）/ `YYYY-MM.md`（月级）/ day 文件（日级）+ `MEMORY.md`（`## 日/月/年` 三层总览） | memory/ 树存明细、MEMORY.md 存总览，并存 |
| 月主题存哪 | 完整正文在 `YYYY-MM.md`；`YYYY.md` 放精简一行索引 | 避免冗余；月文件是完整月记忆，年文件是月索引 |
| `MEMORY.md` 结构 | **三层总览**：`## 日`（今天事件）+ `## 月`（当月+上月，2 月窗口）+ `## 年`（月主题汇总，一生）；汇总自 memory/ 树各级文件 | 一个 MEMORY.md 包含一生记忆；memory/ 树存明细、MEMORY.md 存总览，并存 |
| `## 年` 内容 | **月主题汇总**（每月一条，跨月追加，永久累积）；不另设年度概要 | 一生概貌由月主题呈现；原年度概要规则提炼废弃 |
| `## 日` | per-event 仅当日（滚动） | 今天事件；明细在 day 文件 |
| 日概括成色 | 规则提炼（关键词驱动，默认） | 稳定可复现；后拿实际数据调 |
| LLM 现状 | dream 增强 + 月主题保持；日概括用规则提炼 | 用户定“LLM 保持现状” |
| 过渡 | 与 ovoice 内嵌 dream 并存，ovoice 不动 | 先改好 mem，再谈合项目 |
| frontier 机制 | dream marker 的 `until_seq` 作游标；mem dream 无状态，每次重读最后 marker 的 `until_seq` | marker 即真相，重启不丢；重启/运行一致，留尾不丢 |
| 容错 | 写前去重（幂等）：day 段按 seq 查重，MEMORY/月级/年级按日期/月 upsert | 重试不重复落盘、不跳号 evt-NNN |
| MEMORY.md 拼接可靠性 | 原子写（.tmp→rename）+ 解析容错（未知段原样保留）+ 按 evt/月幂等 + `.bak` 备份 + 失败不写 marker | 一生总览唯一写者，须可靠稳定、可回滚、不丢手写 |
| 检索模型 | 双路径：联想定位（漏斗下钻）+ 直接指定（时间坐标） | 既可联想回忆，也可按 YYYY/MM/DD 直取 |

## 24. 后续阶段（不在本次范围）

1. ovoice 移除内嵌 dream，改 bash 调 `mem dream`（触发时机归属：ovoice `DreamTrigger` 的 idle/cap 改成 bash 调 mem）。
2. 前端可见性（dream stdout → ovoice 回显 / 报告）。
3. `mem read` 读写路由 + `mem index` 适配四级漏斗。
4. ~~年度 LLM 总结~~（年度概要已废弃，§11；如需更强年级总结，可后续增强月主题 LLM）。

---

## 附录 A：检索模型（双路径）

mem 文件树即时间索引（路径 `YYYY/MM/DD` + seq 指针），支持两条互补检索路径——既可联想下钻，也可按时间坐标直取。

### A.1 联想定位（漏斗下钻）
不知道具体哪天，靠概括逐层联想定位：

`MEMORY.md` `## 年` → `mem read YYYY`（月主题索引,定位月）→ `mem read YYYY-MM`（月主题+日概括,定位天）→ `mem read YYYY-MM-DD`（事件段,定位事件）→ `mem history YYYY-MM-DD --seq a..b`（原始对话）。

### A.2 直接指定（时间坐标）
已知年/月/日，直接取，跳过联想层：

| 想取 | 命令 | 状态 |
|---|---|---|
| 某天原始对话（最细） | `mem history 2026-07-30` | ✅ 现状 |
| 某天指定事件对话 | `mem history 2026-07-30 --seq 23..31` | ✅ 现状 |
| 某天事件段 | `mem read 2026-07-30` | ✅ 现状 |
| 某月每天列表 | `mem ls 2026 7` | ✅ 现状 |
| 某月月级（月主题+日概括） | `mem read 2026-07` | ⏳ 本次加 |
| 某年年级（月主题索引） | `mem read 2026` | ⏳ 本次加 |
| 关键字跨年月搜 | `mem search "drag-drop"` | ✅ 现状 |

### A.3 `mem read` 按粒度路由（本次新增）
`bin/mem.rs` 的 `read` 分支按输入格式判粒度（`YYYY-MM-DD` → day 文件 / `YYYY-MM` → 月级 `YYYY-MM.md` / `YYYY` → 年级 `YYYY.md`），调对应读取。`mem_cli.rs` 加 `read_month` / `read_year`（或 `read_day` 泛化）。

## 附录 B：现状代码参考点（实现时对照）

- `dream.rs:126-204` execute_dream（流程骨架）
- `dream.rs:268-292` split_rounds / `:341-360` mechanical_extract / `:364-382` tail_aware_b
- `dream.rs:399-421` parse_groups / `:440-497` reconcile_groups / `:500-520` build_dream_messages
- `dream.rs:211-231` check_and_rotate_month / `:524` MONTH_THEME_SYS
- `memory.rs:13-59` day_file_path / append_day_event / day_event_count
- `memory.rs:62-72` append_memory_day / `:89-99` upsert_current_month_today / `:218-246` rotate_month / `:260` append_year_theme
- `config.rs:90-113` dream_* 字段
- `bin/mem.rs:8-43` main / dispatch / usage / resolve_cache
