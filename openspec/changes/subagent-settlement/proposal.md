# Proposal · subagent-settlement

## Why

子代理的"结算"对主 agent 不诚实，长任务的迭代上限与真实负载冲突，注入产出无护栏，提示词过薄：

1. **失败被吞**：`subagents.rs` 结算块里 `res=Err(e)` 时 `answer = unwrap_or_default()` 变空串，错误原因 `e` 丢失、partial 产出不回带（只有 cancelled 分支带）；更严重的是 `context.rs:197` 渲染 `subagent_result` 时**硬编码"完成"**——失败的子代理在模型眼里是"[子代理 #N 完成]"+空正文，不仅丢信息还在撒谎。`jobdone_body` 的三态文案只到前端卡片，到不了 LLM 上下文。
2. **迭代上限错位**：`SUBAGENT_MAX_ITERS=1000` 定值（`subagents.rs:370`）。真实任务为数小时、数千个循环，1000 会拦腰斩断正常工作；但完全去掉上限又失去失控兜底。
3. **注入无护栏**：`answer` 全文直进主上下文（`context.rs:197`），单个失控子代理可灌 MB 级文本把主回合打爆 413。但若在落盘层截断会污染 dream 的蒸馏输入（`mem_dream.rs:1200` 直接读 `data.summary`），违反"jsonl 原始记录永远完整、压缩只发生在 dream 蒸馏层"的记忆管线理念。
4. **提示词过薄**：子代理上下文干净（只有一句话 system prompt + 任务 prompt），却不知道 workspace 归档约定（projects/scripts/datas/render），输出无结构化骨架，无受众意识；主 agent 侧 spawn 工具 description 没有"任务描述须自包含"的引导——子代理跑偏的两大根源。

参考：pi-agent（earendil-works/pi）的子代理设计——受众意识（"你的输出会被没看过文件的 agent 使用"）、结构化输出骨架（## Completed / ## Files Changed / ## Notes）、具体性要求（文件路径+行号）、以及"LLM 可见视图有界、details/真身永远完整"的分层。

## What Changes

1. **结算三态诚实化**
   - `subagents.rs` 结算块：`Err(e)` → answer = `失败原因：{e}`，partial 非空则附 `部分产出：{partial}`；`Ok("")` → answer = `(无输出)`；cancelled 分支维持现状（partial + note）。
   - `history.rs::subagent_result()` data 增加 `"ok": bool` 字段。
   - `context.rs` 渲染按 `ok` 选「完成」/「失败」；**存量 jsonl 事件缺 `ok` → 默认 true（完成），向后兼容不迁移**。
2. **迭代上限配置化**：`SUBAGENT_MAX_ITERS` 定值删除，新增 config 字段 `subagent_max_iters`（u64，serde 默认 **5000**，0 = 不限制）。失控保护职责仍由既有螺旋熔断承担（`SPIRAL_LIMIT=6` 连续同参同工具即停）。
3. **渲染层护栏（真身永不截断）**：`build_messages` 渲染 `subagent_result` 时对 summary 做 128KB 截断（复用 `tools::truncate`，带显式「已截断，可用 subagent status 查看完整产出」标记 + drill 指引）。jsonl 落盘、dream 蒸馏输入、registry/status/面板全部保持全文。
4. **提示词对齐 pi**：
   - 重写 `d_subagent_sys()` 默认子代理 system prompt：身份定位 + workspace 归档约定 + 动手原则（read 先于盲写、长任务阶段性收敛）+ 结构化输出骨架（`## 结论 / ## 改动与产出 / ## 关键发现 / ## 遗留`）+ 受众意识（"总结是主代理唯一能看到的东西"）+ 具体性要求（精确路径，不贴大段原文）。
   - `subagent` 工具 description 增补：spawn 的 `prompt` 须自包含（背景/目标/涉及文件/验收标准写全），子代理看不到对话历史。

## Capabilities

### New Capabilities

- `agent-subagent-settlement`: 子代理结算的诚实化语义（三态结算、ok 字段、向后兼容）、迭代上限配置、渲染层护栏与真身完整性的分层边界、子代理提示词与 spawn 引导规范。

### Modified Capabilities

（无——`agent-ask-user` 等既有 capability 的 REQUIREMENTS 不变）

## Impact

- **代码**（约 +140 / -30）：
  - `src-tauri/src/subagents.rs`（结算块重写 + 删定值常量）
  - `src-tauri/src/history.rs`（`subagent_result()` 加 ok 参数）
  - `src-tauri/src/context.rs`（渲染按 ok 分支 + 128KB 护栏）
  - `src-tauri/src/config.rs`（`subagent_max_iters` 字段 + 默认 5000 + `d_subagent_sys` 重写）
  - `src-tauri/src/agent.rs`（构造事件传 ok）
  - `src-tauri/src/tools.rs`（subagent description 增补）
- **不引入新依赖**；无前端改动（`jobdone_body` 三态文案自动受益于 answer 携带原因）。
- **兼容性**：存量 jsonl（无 ok）默认渲染"完成"；已自定义 `subagent_system_prompt` 的用户不受默认值更新影响（serde default 仅在字段缺失时生效）。
- **测试**：`subagents.rs`（失败/空成功/cancelled 三态 + iters 上限传入）、`history.rs`（构造器签名）、`context.rs`（ok 渲染两分支 + 护栏截断 + prefix 组合）、`agent.rs`（jobdone 失败场景扩展）、`config.rs`（新字段默认值）。全量 `cargo test` 回归。
- **风险**：
  - R1 存量事件缺 ok → 按"完成"渲染（接受：旧数据里本就这么显示，不会更差）。
  - R2 128KB 护栏正常任务不可见（≈3-6 万 token，1M 窗口下数十个量级）。
  - R3 数千轮任务的子代理自身上下文持续增长（无 dream）——本批不解决，失败/上限时原因可回传（❶ 覆盖），后续单列。
