## Why

主 agent 在跑长任务时常遇歧义（"用方案 A 还是 B？"、"继续优化还是停？"），目前唯一兜底是 agent 自己拍板——但用户有时比 agent 更懂自己的偏好，强行自决既可能错、也浪费了用户参与的机会。需要给 agent 一个"主动停下来问用户"的工具。

约束：本项目已有 24 个工具、driver 主循环已较复杂、DreamCheck / JobDone / Reset / InjectAttachment 路由密集。新功能要尽量复用现有架构（`SessionEvent` 通道、`InterruptHandle`、`HistoryEvent` jsonl、task 子系统），不发明新管道。

## What Changes

新增 1 个工具 `ask_user`，主 agent 主动调起询问用户；前端把现有 textarea 临时切换为三步式问答面板（选/输入 → 预览 → 确认），15s 超时自动选推荐项。

具体引入：

- 新工具 schema（25 个工具总数，从 24 → 25）
- 新 SessionEvent 变体 `AwaitUserRequest` / `AwaitUserAnswer`
- run_turn 工具返回路径新增"特殊哨兵"分支（`AskUserHandle`，不入 tool_result）
- driver 主循环 select! 新增 AwaitUserAnswer 路由分臂（按 question_id）
- 前端 modal 三步式：候选项 chips + 自定义 textarea + 预览态 + 确认/跳过按钮 + 15s 倒计时（暂停+续倒）
- 历史落地：用户回答作为 `kind=user` 入 history，`source="await_user_answer"` 区分

复用：

- `InterruptHandle` 衔接全局中断
- task 子系统：跳过/abort 路径可选自动写 blocker
- `kill_tree` 等价的取消语义给 AwaitUserHandle 的 oneshot
- dream 触发器只数 `kind=user` 不过滤 source（行为对齐普通消息）

## Capabilities

### New Capabilities

- `agent-ask-user`: 主 agent 主动询问用户的工具及配套前端 UI；定义 schema、4 种结局语义、driver 挂起/解冻机制、schema 校验规则（≤1 个 recommended）、前端三步式交互（暂停倒计时 15s+5s）

### Modified Capabilities

（无——现有 capabilities 的 REQUIREMENTS 不变；新功能是纯增量）

## Impact

- **代码**：
  - `src-tauri/src/agent.rs`（+~80 行：SessionEvent 变体 + run_one 挂起/解冻 select 分支）
  - `src-tauri/src/ask_user.rs`（**新文件，~150 行**：`AskUserHandle`、`run_await_user` 四臂 select）
  - `src-tauri/src/tools.rs`（+~100 行：`tool_ask_user` + schema + 入口分派 + 断言 24→25）
  - `src-tauri/src/llm.rs`（+~20 行：run_turn 收到 AskUserHandle 哨兵的分支）
  - `src/main.js`（+~150 行：modal 状态机 + 倒计时 pause/resume + 用户输入路由）
  - `src/index.html`（+~30 行：modal 模板）
  - `src/styles.css`（+~50 行：三步式视觉 + 推荐项高亮）

- **架构**：
  - driver 主循环新增 1 个 `SessionEvent` 变体 + select 分支
  - 新增 driver 状态：question_id 单飞（防嵌套）
  - 不引入新 mpsc 通道，复用 `SessionHandle.tx`

- **不引入新依赖**

- **测试**：
  - `src-tauri/src/ask_user.rs` 测试模块 5 例：秒答 / 超时+有推荐 / 超时+无推荐（降级 continue）/ 用户跳过 / 全局中断
  - `tools.rs` 同步 `schemas_has_twenty_five_tools` 断言

- **风险**：
  - R1: driver select! 加分支可能破坏现有 DreamCheck/JobDone/Reset/InjectAttachment 路由——按 question_id 隔离
  - R2: run_turn 工具重试机制需绕开 AskUserHandle——run_turn 入口检查返回值类型
  - R3: 前端 modal 要拦默认 send——question_id 全局唯一 + source 字段路由
  - R4: 已有探针 diff `src-tauri/src/llm.rs` 的 `[probe]` 块与本功能无关，独立 revert
