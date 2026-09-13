# Design · subagent-settlement

## 数据流（修复前 → 修复后）

```
subagents.rs spawn_agent 结算块
│  res: Result<String, String>（Ok=最终 content / Err=错误原因）  cancelled: bool
│
├─ 修复前：Err(e) → answer=""（原因丢失）；Ok("") → answer=""；cancelled → partial
│          事件 data={agent_id, summary, ref}（无状态）；渲染硬编码「完成」
│
└─ 修复后：
   Err(e)  → answer = "失败原因：{e}"（+ partial 非空附「部分产出：{partial}」），ok=false
   Ok("")  → answer = "(无输出)"，                              ok=true
   Ok(s)   → answer = s，                                       ok=true
   cancel  → answer = partial，note = 被用户终止，               ok=false（suppress 逻辑不变）
   事件 data={agent_id, summary, ref, ok}
   渲染：「[子代理 #N 完成] {summary}」 / 「[子代理 #N 失败] {summary}」
```

## 决策记录

### D1 · 失败原因走 answer 字段，不新增 note 臂
`JobOutcome` 结构不动（`ok` + `answer` + `note` 已够表达）。原因文本放 `answer`，`jobdone_body` 的 `(None, false)` 臂（"[子代理 #N 失败]
{answer}"）和 `subagent_result.summary` 自然携带——前端卡片与 LLM 上下文同时受益，零结构变更。

### D2 · `ok` 缺省 true，不做迁移
jsonl 是 append-only 一生记录，不做改写。`context.rs` 渲染时 `data.get("ok") != Some(false)` 即按完成处理：旧事件维持既有显示，不更差。dream（`mem_dream.rs:1200`）只读 `summary` 字符串、不看 ok，天然兼容——且修复后 summary 携带原因/partial，蒸馏输入反而更全。

### D3 · 迭代上限是配置旋钮不是保险丝
用户的真实负载：数小时、数千循环。原「定值 50」拍错了；定值 1000 同样会斩断正常任务。定案：
- 新 config 字段 `subagent_max_iters: u64`，serde 默认 **5000**（覆盖"数千"），`0 = 不限制`（信任螺旋熔断）。
- 失控保护职责划分：**螺旋熔断（SPIRAL_LIMIT=6，连续同参同工具+无文本）是主保险丝**，抓的是病态死循环，与轮数无关；iters 上限只兜"缓慢漂移型失控"的底，因此可以给得很宽且可调。
- 已有配置模式对齐：`max_tool_iters`（主循环）、`max_subagents`（并发）均为 config 字段。

### D4 · 护栏只在渲染层，真身（落盘）永远全文
记忆管线：jsonl 原文 → dream 蒸馏 → MEMORY.md → pin 回上下文。落盘层截断 = 让 dream 对删节文本蒸馏 = 一生记忆源头缺损，违反管线理念。因此：
- **落盘全文**：`HistoryEvent::subagent_result.data.summary` 存 `answer` 全文；registry `j.answer` 全文（status/kill/面板）。
- **渲染护栏**：`context.rs::event_to_message` 的 `subagent_result` 臂，summary 超 **128KB**（定值常量，非配置——防浪堤不是调优旋钮）时 `tools::truncate` 截断并附标记「（已截断，完整产出可用 subagent status 查看）」。128KB≈3-6 万 token，正常任务不可见；防的是病态洪水单发打爆主回合。
- 免疫兜底：context 涨至 300k prompt tokens 自有 dream 触发瘦身；护栏只补"单发瞬间超重"的空档。

### D5 · 提示词对齐 pi 的四个要素
pi 子代理 agent 定义（worker/scout/planner/reviewer.md）的可迁移要素 → 通用 worker 型 prompt：

| pi 要素 | ovoice 落法 |
|---|---|
| 受众意识："你的输出会被没看过这些文件的 agent 使用" | "最终总结是主代理唯一能看到的东西，主代理没看过你读的任何文件" |
| 结构化输出骨架（## Completed / Files Changed / Notes） | `## 结论 / ## 改动与产出 / ## 关键发现 / ## 遗留` 四段骨架 |
| 具体性（文件路径+行号，下游 verbatim 执行） | "具体到文件路径与行号；不贴大段原文，给路径与摘要" |
| 边界自约束（不依赖权限强制） | workspace 归档约定写入 prompt（子代理上下文干净，本不知道 projects/scripts/datas/render） |

镜像面（主 agent 侧）：pi scout.md 开头声明输入格式 → `subagent` 工具 description 增补"prompt 须自包含：背景/目标/涉及文件/验收标准写全，子代理看不到对话历史"。

新 `d_subagent_sys` 默认值（约 15 行，维持 pi worker 的克制长度）：

```
你是 ovoice 的子代理：受主代理委派、在独立干净上下文中完成单一任务的执行者。你看不到主对话历史。

工作区约定：workspace 根下 projects/（项目）、scripts/（脚本）、datas/（数据与文档）、render/（渲染产物），新建文件按类归位；任务若提到 AGENT.md/SOUL.md/MEMORY.md，用 read 自行查看。

动手原则：用 write/read/edit/bash 完成任务；先 read 摸清现状再改，不盲写；长任务每完成一个里程碑让磁盘状态自洽（改一半的东西要么完成要么回退）。

最终总结（主代理唯一能看到的东西）。格式：
## 结论
完成了什么；若失败，失败在哪（一句话）。
## 改动与产出
逐项：精确文件路径 + 做了什么。
## 关键发现
主代理后续需要的数字、结论、路径。
## 遗留
未完成项/风险/需主代理决策的点；没有写「无」。
要求：具体到文件路径（必要时至行号）；给路径与摘要，不贴大段原文。
```

注：`subagent_system_prompt` 是 config 可覆盖字段——已自定义的用户不受默认值更新影响（serde default 仅缺字段时生效），无迁移。

## 触碰面清单（file:line）

| 文件 | 位置 | 改动 |
|---|---|---|
| subagents.rs | :370 常量、:440-507 结算块、:433 run_turn 调用 | 删常量；结算三态重写；iters 从 cfg 读 |
| history.rs | :66 subagent_result() | 加 ok 参数，data 插入 |
| context.rs | :192-198 渲染臂 | ok 分支 + 128KB 护栏（新 const） |
| config.rs | :89-94 字段区、:133 d_subagent_sys | 加 `subagent_max_iters` + 默认函数；重写默认 prompt |
| agent.rs | :115-117 JobDone→事件构造 | 传 ok |
| tools.rs | :996-1008 schema description | spawn 引导文案 |

## 非目标（Non-goals）

- 子代理自身上下文增长问题（数千轮无 dream）——单列后续 change。
- parallel/chain 编排、agents/*.md 声明式专家、用量统计注入——第三批。
- 崩溃孤儿子代理的启动注入（subagent_start/end 事件）——第二批。
- 前端改动。
