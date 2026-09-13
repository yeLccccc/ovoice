# Design · ask_user

## Context

ovoice 现有架构：driver 主循环收 `SessionEvent` → `handle_event` 落 history → `run_turn` 跑 LLM 轮。工具调用通过 `tool_call` 在 run_turn 内串行执行，结果作为 `tool_result` 写回 messages。子代理 (`subagent` 工具) 是 fire-and-forget 模式，不在本次范围。

新需求：agent 主动询问用户并同步等待答案。最大架构挑战是工具返回路径——目前 `tool_call` 工具返回 `String` 直接写 `tool_result` 注入 messages，没有"等异步事件回来"的钩子。

## Goals

- 提供一个工具让主 agent 主动停下来问用户
- 三步式 UI 避免误触
- 15s 超时自动推进兜底
- 不破坏 driver 主循环现有路由
- 不引入新 mpsc 通道、新依赖
- schema 总数 24 → 25

## Non-Goals

- 多并发 ask_user（单飞：question_id 单调）
- ask_user 等待期间 agent 调其他工具（防御报错）
- 跨会话的"待回答队列"持久化（重启即丢）
- 子代理 (`subagent`) 工具内部使用 ask_user（递归锁）
- 自定义键盘快捷键绑定（先用鼠标 + Enter/Shift+Enter）
- 多人协作 / 多用户会话（单用户单会话）
- 推荐的视觉强调形式自定义（固定 ✦ ★ 即可）

## Decisions

### Decision 1: 工具返回类型变更——从 String 到 enum

**背景**：当前所有工具返回 `String` 作为 `tool_result` 内容。

**选择**：`tool_ask_user` 返回新的 `AskUserResult { answer, skipped, timed_out, cancelled, auto_submitted, fallback_used }`，run_turn 入口加类型分派：`String` 走原路径（写 tool_result），`AskUserResult` 走挂起路径（不写 tool_result、把 oneshot 交给 driver）。

**理由**：复用 run_turn 已有的"工具执行完 → 处理结果"框架，最小侵入。返回 enum 在 Rust 是零成本抽象（`Result<String, AskUserHandle>` 等价语义）。

**替代考虑**：新增 SessionEvent 变体 `AwaitUserResult` 由 driver 监听——这条路径需要 run_turn 主动 emit SessionEvent，跟现有"run_turn 不直接 emit、只返结果给 driver"的边界冲突。

### Decision 2: driver 挂起方式——oneshot + question_id 索引

**背景**：driver 主循环 select! 监听多路事件；新加 `AwaitUserRequest` 派发和 `AwaitUserAnswer` 接收。

**选择**：
- `SessionEvent::AwaitUserRequest` 派发：driver 看到 `run_one` 返 `AskUserHandle`，转成 `AwaitUserRequest` emit 给前端，并在 driver 持有一个 `Arc<Mutex<HashMap<String, oneshot::Sender<AskUserResult>>>>`
- `SessionEvent::AwaitUserAnswer` 接收：driver 按 question_id 查表、找到对应 oneshot 发送 result、解冻 turn

**理由**：oneshot 是 tokio 原语，零成本；HashMap 单飞防嵌套；问题空间里 question_id 是 1 个就够。

**替代考虑**：用 mpsc channel + 顺序匹配——浪费、且打断现有 message 顺序。

### Decision 3: 超时语义——只有一种兜底（自动选推荐项）

**背景**：原设计四种兜底（continue/pick_recommended/default_answer/drop），你最后砍到一种。

**选择**：超时 = 自动选推荐项。无推荐项时降级为 `answered: false` 让 agent 自己拍板。多推荐项 schema 拒收（暴露 LLM bug）。

**理由**：用户明确要求"超时自动选推荐项"。无推荐项降级 continue 而非报错是因为：兜底兜底、不该把对话卡死。

### Decision 4: 跳过按钮 = abort，跟 timeout 完全分开

**背景**：原"跳过 = drop 兜底"在砍掉 drop 后语义消失。

**选择**：跳过按钮 = 用户主动取消本次提问，`tool_result: {aborted: true, answered: false}`。agent 收到 abort 知道"用户拒绝答"、跟"用户没看见 (timeout)"区别对待。

**理由**：跳过 vs 超时是两个语义，用户主动表达 ≠ 沉默。

### Decision 5: 历史落地用 source 字段区分

**背景**：现有 `SessionEvent::UserMessage` 已有 `source: String` 字段（`agent.rs:12`）。

**选择**：用户回答作为 `kind=user` 入 history，`source="await_user_answer"`。

**理由**：零侵入、跟现有 `source="user"` / `source="inject"` 等同款处理。dream 触发器只数 `kind=user`，所以新 source 不影响 cap 行为（投票倾向于让"被询问"也顶 cap——agent 被问才答，符合"用户活跃"语义）。

### Decision 6: 复用 InterruptHandle 处理全局中断

**背景**：现有 `InterruptHandle` (tools.rs:40) 是 Arc<Mutex<Option<CancellationToken>>>，由 `interrupt_task` 命令 cancel。

**选择**：ask_user 等待期间，主 driver 的 `interrupt.current()` 关联到 AwaitUserHandle 的 oneshot cancel。`interrupt_task` 一被调，oneshot cancel、driver 解冻 turn、视为 cancelled 结局。

**理由**：复用现有中断语义、不发明新机制。

### Decision 7: 前端三步式 + 暂停倒计时

**背景**：UI 形态已锁：候选项 + 自定义 + 确认 + 15s 暂停倒计时。

**选择**：纯原生 HTML/CSS/JS，不引入框架。modal 模板放在 `index.html` 末尾隐藏，JS 控制显示。倒计时用 `setInterval` + 暂停时 `clearInterval`、续倒时重启。

**理由**：ovoice 现有前端就是 vanilla JS（CLAUDE.md 提到的"vanilla HTML/JS/CSS"模板），保持栈一致。

### Decision 8: 不实现跨 turn 的"未回答问题"持久化

**背景**：如果 agent 在 ask_user 等待期间用户重启应用、会怎样？

**选择**：driver 重启 = ask_user 自动 cancel → tool_result `{cancelled: true, answered: false}` → agent 下一轮知道。

**理由**：现有 turn 状态本就不跨重启持久化，ask_user 不该特殊处理。简单 > 复杂。

## Risks / Trade-offs

### Risk 1: driver select! 加分支可能破坏现有路由

DreamCheck / JobDone / Reset / InjectAttachment 5 个变体已在 select! 中。

**缓解**：
- question_id 全局唯一、按 question_id 索引 oneshot,不靠 select 分支匹配
- AwaitUserRequest / AwaitUserAnswer 是新事件类型、不影响现有 5 个
- 测试覆盖 5 个现有事件在 ask_user 等待期间的接收

### Risk 2: run_turn 工具重试机制对 AskUserHandle 哨兵的兼容性

run_turn 现有 `for (idx, tc) in tool_calls` 循环中,工具失败会重试。AskUserHandle 走挂起路径,不该被重试。

**缓解**：
- run_turn 入口加返回值类型检查：`Err(AskUserHandle)` 直接 break、`String` 走 tool_result
- 单元测试覆盖"工具返 AskUserHandle 时不写 tool_result"

### Risk 3: 前端 modal 跟主 textarea 的 sendMessage 路径冲突

用户在 modal 选完确认 → emit `user-answered`；用户在主 textarea 打字 → emit 普通 UserMessage。两条路径都要走 driver。

**缓解**：
- question_id 全局唯一（uuid v4），driver 按 question_id 路由
- 前端 sendMessage 加判断:如果在 modal 中,默认走 user-answered 路径;否则普通 UserMessage
- modal 关闭时清空 question_id 引用,避免悬挂

### Risk 4: schema 数 24 → 25 的断言同步

`tools.rs:1901` 那条断言跟实际 schema 数量绑定。

**缓解**：在写 schema 时同步改测试断言。

### Risk 5: 跟 `src-tauri/src/llm.rs` 已有临时探针 diff 的并发修改

仓库 master 已有 `[probe] eprintln!` 未提交改动（排查 agent 自报工具数 bug）。

**缓解**：本 change 不动 `llm.rs` 的探针部分，独立 commit revert。开发流程上先 revert 探针再 apply change。

### Risk 6: 前端 15s 倒计时 pause_on_activity 的活动检测

鼠标 hover 选项 → 暂停；但 hover 离开 → 算"活动结束"还是"持续活动"？

**缓解**：
- 用 `mouseenter` 触发暂停、`mouseleave` 不直接续倒（让 `resume_after_idle_secs` 自然到期）
- 键盘活动用 `keydown` 监听,5s 内任意键都重置续倒计时
- 测试覆盖"持续 hover 不续倒"

### Trade-off: 自定义输入模式切 textarea 时丢失草稿

如果用户在 modal 切到自定义输入后,又点回候选项,输入的文本怎么办?

**选择**:保留草稿、不清空,等用户点"确认"或关闭 modal 时一起提交/丢弃。

**理由**:避免用户切来切去丢内容,UX 更稳。
