# Eng Review — agent-session-jobs spec

> 日期：2026-07-24 · 分支 `feat/agent-session-jobs` · skill `/plan-eng-review`
> 评审对象：`docs/superpowers/specs/2026-07-24-agent-session-jobs-design.md`（实现前）
> 模式：FULL_REVIEW（非 plan mode，无 plan 文件；报告写本独立文档，未调 ExitPlanMode）

## GSTACK REVIEW REPORT

| Review | Trigger | Why | Runs | Status | Findings |
|---|---|---|---|---|---|
| CEO Review | `/plan-ceo-review` | Scope & strategy | 0 | — | (未跑，可选) |
| Codex Review | outside voice | 独立第二意见 | 1 | issues_found→folded | codex 未认证→Claude subagent 代跑，8 findings 全采纳 |
| Eng Review | `/plan-eng-review` | 架构/测试/性能(required) | 1 | CLEAN(已 fold) | 4 节 12 + outside 8 = 20 findings，全部折进 spec v2 |
| Design Review | `/plan-design-review` | UI/UX | 0 | — | (面板 UI 细节可在实现后 `/design-review`) |
| DX Review | `/plan-devex-review` | 开发者体验 | 0 | — | (N/A) |

**VERDICT:** ENG CLEARED — 20 findings 全部解决并折进 spec v2，可进 writing-plans。

NO UNRESOLVED DECISIONS

## 完成摘要

- Step 0 范围挑战：复杂度告警触发（~8 文件 + 2 服务），用户选 **A 全量含面板**，范围锁定不再重开。
- 架构评审：4 issues（A1 pipe-drain / A2 SessionHandle 拆分 / A3 并发上限 / A4 assign 硬校验）。
- 代码质量：4 issues（C1 ToolsCtx / C2 chat send 失败 / C3 env allowlist / C5 ASCII×2；C4 文档澄清）。
- 测试：覆盖图产出，3 项 gap + 回归铁律（T1 pipe-drain / T2 边界 unit / T4 FE 冒烟）。
- 性能：1 issue（P1 历史窗口化；P2 日志清理→TODO）。
- Outside Voice：Claude subagent 跑（codex 未认证），8 新缺口全采纳（G1 turn 边界+system 注入 / G2 kill 竞态+去 --async+句柄 / G3 背压 / G4 reset 清 registry+read 上限）。
- 失败模式：**0 critical gap 残留**（pipe-drain/system 注入/kill 竞态/turn 边界 4 个本会是静默 P1，均已 fold + 配测试）。
- Lake Score：20/20 recommendations chose complete option（P2 日志清理唯一降为 TODO，非 shortcut）。

## NOT in scope（明确推迟）

- **子 agent（SubAgent 型 Job）**：二期，复用本期 Job/callback/Session/面板基础设施。
- **跨 app 重启持久化**：Session + JobRegistry 内存态，退出即清（长视频随重启丢失，用户已确认）。
- **给模型的 job 管理工具**：v1 fire-and-forget，取消/查看走面板。
- **完整历史 summarization**：v1 只做旧 tool 结果截断折叠（P1 最小版）。
- **昂贵命令确认门**：后台+全自动烧 quota，v1 面板可见可终止兜底。
- **FE 测试基建**：裸 ES module，FE 改动走 `/qa` 手测清单。
- **多会话/多窗口**。

## What already exists（复用，未重建）

- `llm::run_loop` 的循环体 → 重构为 `run_turn`（复用 `HttpRound`/`AppEmitter`/`MAX_ITERS`/`RoundResult`/`consume_sse`）。
- `tools::tool_bash` 的 spawn + Job Object + `decode_output` → 复用为 `spawn_process_job`。
- `AppEmitter` 事件 emit → 复用，仅增 `chat-turn-start/end`/`chat-error`/`job-update`。
- `win_job::Job`（KILL_ON_JOB_CLOSE）→ 复用，句柄搬进 registry。
- `resolve_path`/`truncate`/`READ_MAX` → 复用。
- 无平行流程被新建。

## 失败模式（新代码路径）

| 路径 | 现实故障 | 测试? | 错误处理? | 用户可见? |
|---|---|---|---|---|
| 后台 job stdout | pipe 不 drain→缓冲满→假死锁 | ✅ T1 | ✅ pipe-drainer 任务(A1) | 否（静默）→已修 |
| JobDone 注入 | kill 后 guardian 仍发→误唤醒 | ✅ T2 | ✅ emit 前查 Killed(G2) | 否→已修 |
| 首条消息 | 无 system→persona 丢 | ✅ T2 | ✅ spawn_session 注入(G1) | 否（静默）→已修 |
| 多 turn 渲染 | JobDone 交错串气泡 | ✅ T4 E2E | ✅ turn-start/end 事件(G1) | 是（乱）→已修 |
| driver 死亡 | chat send 静默无效 | ✅ T2 | ✅ send 失败返 Err(C2) | 是（报错） |
| app 崩溃中途 | mmx 孤儿进程 | 难测(T4 手测) | ✅ Job Object 句柄(G2)+assign 硬校验(A4) | 否 |

**Critical gap（无测试+无处理+静默）：0。** 4 个潜在静默 P1 均已 fold + 配测试。

## Implementation Tasks（折进后续 plan）

- [ ] **T1 (P1, CC:~15min)** `jobs.rs` — pipe-drainer 任务持续 drain stdout/stderr→log（backs A1）
  - Verify: 写 >4KB stdout 的进程不阻塞、正常完成单测
- [ ] **T2 (P1, CC:~20min)** `agent.rs` — SessionHandle(state)/driver 拆分 + spawn_session 注入 system（backs A2/G1）
  - Verify: 首条 UserMessage 前 messages[0] 为 system 单测
- [ ] **T3 (P1, CC:~15min)** events+`main.js` — chat-turn-start/end + 前端按事件界定气泡（backs G1）
  - Verify: E2E 两 JobDone 背靠背不串气泡
- [ ] **T4 (P1, CC:~10min)** `jobs.rs` — guardian emit 前查 Killed，kill 不误唤醒（backs G2）
  - Verify: kill_job 后无 JobDone 回调单测
- [ ] **T5 (P2, CC:~15min)** `jobs.rs` — Job Object 句柄存 registry + assign 失败硬校验（backs G2/A4）
  - Verify: assign 失败→Failed
- [ ] **T6 (P2, CC:~10min)** `jobs.rs` — MAX_RUNNING=8 + 完成释重句柄（backs A3）
  - Verify: 第 9 个被拒
- [ ] **T7 (P2, CC:~15min)** `tools.rs` — ToolsCtx + env allowlist(MINIMAX_*/MMX_*)（backs C1/C3）
  - Verify: 非 allowlist env 被忽略
- [ ] **T8 (P2, CC:~10min)** `agent.rs` — session_tx bounded(64) + chat send 失败返 Err（backs G3/C2）
  - Verify: driver 死→chat 返 Err
- [ ] **T9 (P2, CC:~10min)** spec+`tools.rs` — 去 --async 轮询指引 + bash description 重写（backs G2）
  - Verify: description 文案
- [ ] **T10 (P2, CC:~15min)** `agent.rs` — 历史窗口化（旧 tool 结果折叠）（backs P1）
  - Verify: 长消息裁剪单测
- [ ] **T11 (P3, CC:~10min)** commands — reset_session 清 registry + read_job_log READ_MAX（backs G4）
  - Verify: reset 后 registry 空
- [ ] **T12 (P2, CC:~20min)** tests — T1 pipe-drain 测 + T2 边界 unit 组 + T4 FE 冒烟清单 + 回归绿（backs 测试节）
  - Verify: `cargo test` 全绿

**并行化**：T1/T5/T6（jobs.rs）同模块顺序；T2/T8/T10（agent.rs）同模块顺序；T7（tools.rs）依赖 T2 的 ToolsCtx；T3 依赖事件契约。**两 lane**：Lane A jobs.rs(T1→T5→T6) ∥ Lane B agent.rs(T2→T8→T10)；合并后 T7→T4→T9→T11→T3→T12。前端 T3 在事件契约定后做。
