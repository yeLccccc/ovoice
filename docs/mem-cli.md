# mem CLI —— 记忆深思工具

> 零 LLM 只读 drill（ls/read/history/search）+ dream 写端（day/month/year）+ pack 渲染。
> cache 定位：`--cache > $OVOICE_CACHE > (--workspace 别名) > next-to-exe config > %APPDATA% config > %APPDATA%/cache`。

## 架构：三层 jsonl 真相源 + MEMORY.md 渲染

```
history/{date}.jsonl          ← 对话原文（append-only，dream 只读不写除 marker）
    ↑ dream day 提取
memory/.../{date}.jsonl       ← 日层索引卡（每事件一行 JSON）
    ↑ dream month 提取
memory/.../{YM}.jsonl         ← 月层（天卡 + 月度主线）
    ↑ dream year 提取
memory/{Y}/{Y}.jsonl          ← 年层（月卡 + 年度主线）

MEMORY.md                     ← pack 从三层 jsonl 渲染（展示层，非真相源）
```

**真相源** = 三层 jsonl（dream 写、ls/read/search 读）。MEMORY.md 是 pack 渲染的 markdown 展示层。

## 指令

### dream —— 三层提取（写端，LLM）

```
mem dream [day|month|year] [日期] [--once] [--mechanical] [--dry-run]
```

| 子命令 | 真相源（读下层） | 输出 | 触发 |
|--------|-----------------|------|------|
| `dream day`（默认） | history jsonl | 日层 jsonl + marker | frontier 之后有新事件 |
| `dream month [YYYY-MM]` | 当月日层 jsonl | 月层 jsonl（天卡+主线） | 手动 / 每日（后续） |
| `dream year [YYYY]` | 当年月层 mainline | 年层 jsonl（月卡+年度主线） | 手动 / 跨年 |

**marker**（dream day 写）：记录 dream 运行细节，不只进度。

```json
{
  "kind": "marker", "marker": "dream", "until_seq": 7825,
  "seg": [7015, 7825],         // 处理范围
  "rounds": 7,                 // 活动段数
  "events": 3,                 // 提取事件数
  "batches": 1,                // 分批数
  "tokens": {"in": 7773, "out": 2702, "cached": 128},
  "elapsed_ms": 20338,
  "tail_rounds": 0,
  "model": "MiniMax-M3"
}
```

- `frontier = last marker until_seq + 1`（只处理新事件，增量）
- marker 丢 → frontier=0 → 全量重跑（不检测日层文件存在）
- dream round 失败：**绝不机械兜底**，重试 3 次后中断（marker 不推进，下次重来）

**日层提取**（dream day）：
- 切活动段（user 输入 ∪ spawn 子代理 ∪ ts>5min）
- 按 batch_max_events（默认 100）+ CHAR_BUDGET（40K 字）段边界切批
- 强输入：user/content/thinking/subagent summary 全文 + tool name+标识参数 + 工具状态首行
- DREAM_SYS_V2：索引卡导向（detail 写事实禁空动作 / title 高区分 / keywords 中英文）
- 跨批 next_context 滚动衔接
- max_tokens 32768（M3 支持 128K 输出）

### ls —— 列索引（只读，扫一眼定位层）

```
mem ls [YYYY | YYYYMM | YYYYMMDD]    （也认横线 YYYY-MM-DD）
mem ls                                （无参→列年）
```

- 年（4位）：各月 + month_title + 事件数
- 月（6位）：各天 + day_title + 事件数
- 日（8位）：各事件 `[type] title — seq[a..b]`

### read —— 读详情（只读，渲染正文）

```
mem read <YYYY-MM-DD | YYYY-MM | YYYY>
```

- 日：事件卡（hhmm/evt/type/title + **detail** + keywords/主语/对话索引）
- 月：天卡（day_title + top_events + drill）+ 月度主线（mainline + key_outputs + keywords）
- 年：月卡（month_title + drill）+ 年度主线（year_mainline + keywords）

### history —— 读原始对话（只读，drill 到原文）

```
mem history <date> [--seq a..b]
```

- 读 history/{date}.jsonl 原文，可按 seq 闭区间过滤
- compact 日期自动转横线（20260731 → 2026-07-31）
- 人性化渲染：`[seqN 角色] 内容`（user/assistant/tool_result/external/subagent_result/marker）

### search —— 关键字定位（只读，语义匹配）

```
mem search <query> [--raw]
```

- **case-insensitive**（ipS = IPS）
- **只搜日层 jsonl**（跳过月/年索引，避免上层概要重复噪音）
- **语义匹配**：解析 JSON，检查 title/detail/type/evt + keywords/top_events/key_outputs 数组
- 跳过 JSON 字段名噪音（`"attachment":null` 不误匹配）
- 空 query 守卫（不 dump 全量）
- `--raw`：扩到 history/ 原文（默认只搜 memory/）

### pack —— 从三层 jsonl 重拼 MEMORY.md

```
mem pack
```

- 纯渲染（无 LLM），读三层 jsonl → 拼 markdown → 原子写 MEMORY.md
- ## 日：今天事件（title + detail）
- ## 月 ### 当月 + ### 上月：月度主线 + 关键产出 + 关键词 + 天卡（day_title + top_events）
- ## 年：年度主线 + 月卡
- 幂等（全量重写，非增量）
- 边界友好（无数据时友好提示，不崩）

## 文件布局

```
cache/
├─ memory/
│  ├─ {年}/
│  │  ├─ {年}.jsonl              年层
│  │  └─ {月}/
│  │     ├─ {年}-{月}.jsonl       月层
│  │     ├─ {年}-{月}-{日}.jsonl  日层（事件索引卡）
│  │     ...
│  └─ MEMORY.md                   pack 渲染（展示层）
├─ history/
│  └─ {年}-{月}-{日}.jsonl        对话原文（真相源）
└─ config.json                    api_key/model/region/dream_*
```

## 三层 drill 链

```
年层(ls/read YYYY)
  → 月份定位(ls/read YYYY-MM)
    → 日期定位(ls/read YYYY-MM-DD)
      → 事件定位(seq 范围)
        → history drill(history --seq a..b 读原文)
```

或捷径：`search <关键词>` → 直接命中 evt + seq → `history --seq` 读原文（2 步）。

## exit code

- 合法查询 + 有数据：0
- 合法查询 + 无数据：0（正常，只是无结果）
- 非法日期/格式：2
- 未知命令：2

## config.json 相关字段

| 字段 | 默认 | 说明 |
|------|------|------|
| `dream_tail_rounds` | 3 | 留尾回合数（最近 N 个 user 回合不进 dream） |
| `dream_merge_max_rounds` | 5 | 单组最大回合数（超出 F11 拆分） |
| `dream_batch_max_events` | 100 | 一批最大事件数（+ CHAR_BUDGET 40K 双约束） |
