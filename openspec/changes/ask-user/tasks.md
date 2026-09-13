# Tasks · ask_user

## 1. 数据结构与 SessionEvent 扩展

- [ ] 1.1 在 `src-tauri/src/agent.rs` 加 `SessionEvent::AwaitUserRequest { question_id, question, context, options, allow_custom, require_confirm, timeout_secs, pause_on_activity, resume_after_idle_secs }` 变体
- [ ] 1.2 在 `src-tauri/src/agent.rs` 加 `SessionEvent::AwaitUserAnswer { question_id, answer, skipped, timed_out, cancelled }` 变体
- [ ] 1.3 新建 `src-tauri/src/ask_user.rs`，定义 `AskUserHandle { question_id, receiver }`、`AskUserResult { ... }` 类型
- [ ] 1.4 导出 `pub use ask_user::*` 到 `lib.rs`

## 2. 工具入口与 schema

- [ ] 2.1 在 `src-tauri/src/tools.rs` 加 `tool_ask_user(args: &Value, ctx: &ToolsCtx, cfg: &Config, round: Arc<dyn LlmRound>) -> Result<String, AskUserHandle>` 函数
- [ ] 2.2 schema 校验:`options` 中 `recommended=true` 数量 ≤ 1,否则返错误
- [ ] 2.3 加 `ask_user` 到 `tools.rs::schemas()` 列表末尾
- [ ] 2.4 在 `tool_dispatch` 加 `ask_user => tool_ask_user(...)` 分派
- [ ] 2.5 同步 `tools.rs:1901` 的 `schemas_has_twenty_four_tools` → `schemas_has_twenty_five_tools`

## 3. driver 挂起/解冻机制

- [ ] 3.1 `agent.rs::run_one` 检测到工具返 `AskUserHandle` 时:emit `await-user` 给前端 + 注册 question_id → oneshot 映射
- [ ] 3.2 `agent.rs::run_one` 加 `SessionEvent::AwaitUserAnswer` 分支:按 question_id 查表,oneshot send `AskUserResult`,清理映射
- [ ] 3.3 防御嵌套:同一 question_id 已存在时,新 ask_user 返错误而非覆盖
- [ ] 3.4 衔接 `InterruptHandle`:interrupt_task cancel 时,所有 active question 的 oneshot 也 cancel,driver 视为 cancelled 结局

## 4. run_turn 集成

- [ ] 4.1 `src-tauri/src/llm.rs::run_turn` 工具执行返回值类型分派:`String` → 现有 tool_result 路径,`Err/AskUserHandle` → 挂起路径
- [ ] 4.2 挂起路径:不写 tool_result,return 一个特殊 `TurnResult { awaiting: Some(AskUserHandle) }`
- [ ] 4.3 driver 看到 awaiting 时,把 Handle 转 AwaitUserRequest 派发,不进入下一轮
- [ ] 4.4 解冻路径:用户答/超时/跳过后,driver 把 `AskUserResult` 作为 tool_result 注入 messages,重入 run_turn

## 5. 前端 modal UI

- [ ] 5.1 `src/index.html` 加 modal 模板(隐藏):题目区、候选项 chips 容器、自定义输入容器、预览态、确认/跳过按钮、倒计时显示
- [ ] 5.2 `src/main.js` 加 `showAskUserModal(payload)` 函数:渲染 question + options,挂事件监听
- [ ] 5.3 `src/main.js` 加倒计时逻辑:`setInterval` + 暂停/续倒状态机
- [ ] 5.4 `src/main.js` 加事件监听:`mouseenter`/`mouseleave` 选项触发暂停,`keydown` 触发暂停 + 重置续倒
- [ ] 5.5 `src/main.js` 加"确认提交" → emit `user-answered` 给 Rust
- [ ] 5.6 `src/main.js` 加"跳过" → emit `user-answered` 带 skipped=true
- [ ] 5.7 `src/styles.css` 加 modal 样式:三步式视觉、推荐项 ✦★ 高亮、自定义输入切换动画

## 6. 历史落地

- [ ] 6.1 `agent.rs::handle_event` 加 `AwaitUserAnswer` 分支:转成 `UserMessage { source: "await_user_answer", ... }` 落 history
- [ ] 6.2 确认 dream 触发器只数 `kind=user` 不过滤 source,行为对齐普通消息

## 7. 测试

- [ ] 7.1 `src-tauri/src/ask_user.rs` 测试模块:5 例
  - 秒答(`answered: true`)
  - 超时+有推荐(`auto_submitted: true, answer: <推荐>`)
  - 超时+无推荐(`answered: false, fallback: "no_recommended"`)
  - 用户跳过(`aborted: true`)
  - 全局中断(`cancelled: true`)
- [ ] 7.2 schema 校验测试:多 recommended 时拒收
- [ ] 7.3 tools.rs 断言同步:`schemas_has_twenty_five_tools`
- [ ] 7.4 防御嵌套测试:同时 2 个 ask_user 时,第二个报错

## 8. 收尾

- [ ] 8.1 独立 commit revert `src-tauri/src/llm.rs` 的临时 `[probe]` 探针
- [ ] 8.2 `cargo test --tests` 通过
- [ ] 8.3 `cargo clippy --all-targets` 无新增 warning
- [ ] 8.4 `pnpm tauri dev` 起得来、modal 视觉/交互正常
