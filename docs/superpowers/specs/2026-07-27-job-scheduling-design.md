# 后台任务调度 + 持久化 + 渲染 实现规格

> **状态：可实施 spec（design doc）。** 2026-07-27 brainstorming 定稿，gstack-plan-eng-review 过一遍（见末尾 GSTACK REVIEW REPORT）。
> **当前阶段：阶段 2 完成（2026-07-27，feat/job-persistence 已 `--no-ff` 合并 master；T1-T4/T6/T7 已交付，T5 关闭提醒暂跳过待重做——自定义标题栏 `win.close()` 不触发 CloseRequested），阶段 3 待开始**（见「实施路线图」）。
> 配套存档：`2026-07-27-task-lines-design.md`（任务线探讨稿）。本 spec 是那条线的**日层第一块砖**。

## 目标

1. 后台任务（`bash bg` / `subagent`）支持**依赖关系**
2. 结果**攒批 + idle 事件驱动调度**（一批一气泡，不再 1 JobDone = 1 turn）
3. job **持久化**（按日 jsonl），重启后显示**当天**任务 + 汇报上次强制退出失败的任务
4. job_id 改**日期 + 序号**格式（`YYYYMMDD-NN`）
5. **软超时**（不 kill，汇报当前输出，agent 主动关闭）
6. 系统事件**移出主对话流** + 修 live/history 重影（顺带验收 #226）
7. **关闭提醒**：退出时若有运行中任务，提醒用户

## 数据保留原则（硬约束，置顶）

**所有 job 输出、对话历史，永久保留，不管有用没用。**

- 持久化**只增不删**（append-only）。永不清理、永不归档删除。
- 存储设计假设数据只增不减：jsonl 文件长期累积，按日切分（`YYYY-MM-DD.jsonl`）便于按日读取，但不删旧文件。
- `.ovoice-jobs/{id}.log`（进程输出全文）同样永久保留，作为"点开看完整输出"的 drill 目标。
- 这条原则**否决**任何"清理 / 归档 / TTL"设计。

## 背景 / 现状（探查确认）

- `agent.rs:87` `triggers_turn`：每个 JobDone 立即触发一次主 agent turn，**无攒批**。N 并发 = N 气泡。
- `jobs.rs:86` `next_id`：内存 u64，从 1 自增，**不持久化**，重启 reset；process / subagent 共用。
- `.ovoice-jobs/{id}.log`：只存进程 stdout/stderr 文本，**不存状态元数据**。
- 启动时**完全不 load**，registry 空，`list_jobs` 返空 → Jobs 面板白板。
- `main.js:1321-1328`：`external` / `subagent_result` 历史重载渲染独立 `[外部]` 气泡，和 Jobs 面板信息重复。
- `main.js:1566`：live assistant 气泡与 history 聚合气泡在 `atBottom=true` 时并存 → 重影（#226）。

---

## 实施路线图（分 3 阶段）

每阶段独立分支、独立可 ship、有用户可见价值。做完一阶段更新本节状态 + TaskList。

| 阶段 | 分支 | 范围 | 交付 | 风险 | 状态 |
|---|---|---|---|---|---|
| **1** | `feat/job-display` | 前端渲染：external / subagent_result 移出主对话流 + 修 live/history 重影（`main.js:1566` reachedBottom 仅上划置 false——systematic-debugging 锁根因，纠正 spec 原议「无条件以 history 为准」）+ Jobs 面板卡增强（完整输出可展开）+ AGENT.md 处理协议 + 回调卡嵌入气泡顶部 + Jobs Reload 自动刷新 | 收 #226，对话流干净 | 低（纯前端 + 提示词） | ✅ 完成（2026-07-27 合并 master） |
| **2** | `feat/job-persistence` | job_id 改 String `YYYYMMDD-NN` + 按日 jsonl 持久化（复用 `workspace_io` DialoguesWriter）+ 启动 load 显示当天 + 旧数据迁移（legacy id）+ 关闭提醒 + forced-exit 标 failed | 重启后看到当天任务（退出提醒暂跳过） | 中（类型变更 + 持久化 + 迁移） | T1-T4/T6/T7 完成；关闭提醒跳过待重做（custom titlebar `decorations:false` → JS `win.close()` 不触发 CloseRequested，前端驱动 fix 未确认根因，已 revert 留 dormant 后端） | |
| **3** | `feat/job-scheduling-core` | 调度模型（completed_pending 队列 + idle turn-busy 标志 + ready 筛选）+ dependencies（环检测 + 未知 id 报错）+ 软超时 + outcome 合并 + 重启汇报 | 一批一气泡、依赖、软超时 | 中高（动 driver select） | 待开始 |

---

## 1. job_id 格式

```
YYYYMMDD-NN     例：20260727-01、20260727-02
```

- **JobId 类型 `u64` → `String`**（决策记录 D1）。
- **当天序号 NN**：2 位补零（01–99）；超 99 自动扩展（100、101…）。
- **生成**：启动 load 今日 job 列表 → 算 max 序号 → 新 job 用 `max+1`。跨日自动从 01 重启。重启不 reset（从持久化恢复）。
- **跨天执行**：id 编码 spawn 日，jsonl 跟 id 走（写 spawn 日文件）。23:50 启动的 job 跨到 00:10 完成，仍记在 spawn 日的 jsonl。
- 影响面：`jobs.rs` JobId 类型、history external/subagent_result 引用、`read_job_log(id)` 参数、`main.js` 显示。

## 2. dependencies 参数

- `subagent` / `bash bg` 加可选 `dependencies: [job_id...]`，默认空。
- 主 agent spawn 时显式声明。不搞自动推断。
- 语义 = **结果处理偏序**：C 要被处理，需 A/B 已有 outcome（done / failed / soft_timeout / killed 均算"有结果"）。
- **环检测（A1 修复）**：spawn 声明 dependencies 时做拓扑检查，发现环立即报错回 agent，不入队列（防死锁）。
- **未知 id 报错（A2 修复）**：dependencies 引用不存在的 job_id，立即报错回 agent，不当死等。
- **超时兜底**：天然防死锁（失败 / kill 也产 outcome）之外，单个任务超久没完成不阻塞 ready 推进。

## 3. 调度模型（idle 事件驱动）

```
后台任务完成 → JobOutcome 入 completed_pending 队列（不触发 turn）→ 看主 agent 状态：

  busy（turn-busy 标志 = true，即有 in-flight turn）
    → 留队列。当前 turn 结束（清 turn-busy）时，回到 idle 检查

  idle（turn-busy = false）
    → 立即处理：筛 ready（dependencies 已满足）
       → 按 finished_at 排序
       → 同 id 多份 outcome 取最后一份（A3 修复：done 覆盖 soft_timeout）
       → 整批（含异归属）喂一个 turn → 一个气泡
       → turn 内 spawn 的新任务结果进队列，等下轮 idle（不开新气泡）
```

- **turn-busy 标志（A4 修复）**：driver 维护显式 `turn_busy: bool`，turn 开始置位、结束清位。select 据此决定 JobDone 进队列还是立即处理。不靠"猜 turn 在不在跑"。
- **idle = turn-busy = false**（不要求"无任何 active 后台任务"，那会饿死）。任务间时序等待由 dependencies 表达。
- 异归属整批喂：配套提示词（§8），主 agent 一个 turn 内分别处理（必要时分发 sub-agent），一个气泡总结。

## 4. 持久化

```
{cache_dir}/.ovoice-jobs/YYYY-MM-DD.jsonl      每天一个文件，永不删（数据保留原则）
```

**append-only 事件流**（复用 `workspace_io` 的 `DialoguesWriter` + Jsonl 后台写，Q2/并发写修复：不另起 writer）：

```jsonl
{"schema":1,"id":"20260727-01","kind":"process","label":"ping...","status":"running","started_at":...,"deps":[]}
{"id":"20260727-01","status":"done","code":0,"finished_at":...,"tail":"job1"}
{"id":"20260727-02","kind":"agent","label":"数到10","status":"running","started_at":...,"deps":["20260727-01"]}
{"id":"20260727-02","status":"soft_timeout","finished_at":...,"tail":"...","note":"已超时，任务仍在跑"}
{"id":"20260727-02","status":"done","finished_at":...,"tail":"..."}
{"id":"20260727-03","status":"failed","reason":"forced_exit","finished_at":...,"note":"app 强制退出时未完成"}
```

- **schema 版本标记（Q3 修复）**：文件首行（或每条事件）带 `schema` 字段，未来字段演化好迁移。
- **load 时按 id fold**（取最后一条）。
- 字段：`schema` / `id` / `kind` / `label` / `status` / `started_at` / `finished_at` / `code` / `tail` / `note` / `reason` / `deps`。
- **跨天（A5 修复）**：jsonl 跟 id 的 spawn 日，不跟完成日。
- **DRY（Q2 修复）**：jobs.rs 和 subagents.rs 共用一套 job 状态机 + 共享 writer，不写两份。

## 5. app 生命周期（关闭 + 启动）

### 5.1 关闭流程（关闭提醒 + forced-exit 标 failed）

- app 关闭（Tauri `WindowEvent::CloseRequested`）时，检测有无 running job。
- **有 running job → 弹确认**："还有 N 个任务在跑（列出），确定退出？退出后这些任务判失败。"
- **用户确认强制退出**：所有 running job 标 `status=failed, reason=forced_exit`，写 jsonl。然后放行关闭。
- **用户取消**：阻止关闭，继续运行。

### 5.2 启动 load + 重启汇报

- 启动读**今天**的 jsonl → fold → 重建 registry → emit 给 Jobs 面板（当天可见）。
- **forced-exit failed 的 job**：不标 orphaned（不用这个模糊词），就标 `failed + reason=forced_exit`。
- **重启汇报（取代原 orphaned 方案）**：启动后如果队列里有 forced-exit failed 的 job outcome，**主动触发一个 turn**把这批喂主 agent（"上次这些任务因强制退出失败了，要不要重做 / 告知用户"）。接入调度模型，一批一气泡。
- 更早的任务按日 drill（`job ls YYYY-MM-DD`）。`list_jobs` 返回内存活跃 + 今日持久化合并。

## 6. 软超时

```
guardian 到 timeout_secs：
  → 不 kill 进程
  → 发软超时 outcome：{status: soft_timeout, tail: 当前输出, note: "已超时，任务仍在跑"}
  → 进程继续，drainer 继续收
后续真完成 → 再发 done outcome（队列里同 id 取最后，done 覆盖 soft_timeout）
agent 收到软超时后自主决定：kill / 继续等 / 先用现有输出
```

- **不催**：timeout 默认宽松（process 1800s 起、subagent 更长，per job `timeout_secs` 可配；bash bg 已有该参数，subagent 补上）。
- **资源上限（P1 修复）**：soft_timeout 进程不能无限堆。最终上限：超过 N 次软超时 或 总宽限 M 分钟后强制 kill（记 killed）。或要求 agent 必须在软超时后处理（kill / 接受）。
- soft_timeout 算"有 outcome"，不阻塞依赖。

## 7. 线 1 渲染（前端 main.js）

1. `external` / `subagent_result` **不再在主对话流渲染独立气泡**。`buildHistoryBubbles` 跳过这两类，或折叠成可展开的"后台活动"摘要行。职责归 Jobs 面板。
2. 修 live/history 重影：turn-end 后**无条件**以 history 聚合为准替换 live（`main.js:1566` 的 `!atBottom` 守卫去掉）。
3. Jobs 面板任务卡增强：完整输出可展开（`read_job_log` 拉全文）、状态清晰。

## 8. 提示词（AGENT.md）

加"后台结果处理协议"：
- 被一批后台结果唤醒后，只在"要让用户知道 / 要展示产物 / 要用户决策"时才说人话；纯确认性回流不复述。
- 多个八竿子打不着的任务结果，分别 spawn sub-agent 深入处理，自己只协调 + 一句汇报。
- 收到 `soft_timeout`：自主判断 kill / 继续等 / 先用现有输出，并向用户说明。
- **重启汇报**：收到 `failed + reason=forced_exit` 的一批，告知用户"上次这些因退出失败"，问要不要重做。

## 9. 为任务线留口子（呼应存档稿）

| 本次改动 | 任务线角色 |
|---|---|
| `job_id` 带日期 | atomic 节点持久 id |
| `deps` 字段 | 任务线的边 |
| 按日 jsonl（永不删） | 任务线日层 checkpoint + 历史档 |

本 spec 实现后，往上叠 project / vision 层即水到渠成。**不在本 spec 做** project/vision 层。

---

## 改动清单（按阶段）

**阶段 1**（`feat/job-display`）：
- `main.js`：移除 external/subagent_result 独立气泡；修 `main.js:1566` 重影；Jobs 面板卡增强。
- `AGENT.md`：加"后台结果处理协议"。

**阶段 2**（`feat/job-persistence`）：
- `jobs.rs`：JobId u64→String；id 生成（今日 max+1）；按日 jsonl 读写（复用 DialoguesWriter）；schema 版本。
- `subagents.rs`：共用状态机 + writer。
- `lib.rs`：启动 load 今日；`list_jobs` 合并；`read_job_log` 参数 String；**关闭流程**（CloseRequested 检测 + 弹确认 + 标 failed）。
- `main.js`：关闭确认对话框。
- 迁移：旧数字 id → `legacy-<n>`，旧 `.log` 保留。

**阶段 3**（`feat/job-scheduling-core`）：
- `agent.rs`：`completed_pending` 队列；`turn_busy` 标志；JobDone 不立即 triggers_turn；idle ready 筛选 + FIFO + 同 id 取最后 + 整批喂；启动主动触发汇报 turn。
- `jobs.rs` / `subagents.rs`：软超时（不 kill + soft_timeout outcome + 资源上限）。
- `tools.rs`：subagent / bash schema 加 `dependencies`；subagent 加 `timeout_secs`。
- 环检测 + 未知 id 报错。

## 测试矩阵（纯逻辑单测，tempfile + FakeRound/FakeEmitter）

**调度模型**（阶段 3）：
- idle/busy 两路径（turn_busy 置位/清位）
- ready 筛选（依赖满足 vs 未满足）
- FIFO（按 finished_at）
- 同 id 多 outcome 取最后（done 覆盖 soft_timeout）
- 整批喂一个 turn（含异归属）
- 启动主动触发汇报 turn（forced-exit failed 批）

**持久化**（阶段 2）：
- round-trip：写 jsonl → 重启 load → fold 正确状态
- schema 版本向前兼容
- 跨天：spawn 日 vs 完成日
- 并发写安全（复用 writer 的单写者保证）

**job_id**（阶段 2）：
- 跨日序号重置（午夜边界）
- 序号从持久化恢复（重启不 reset）
- 超 99 扩展
- legacy id 迁移

**依赖**（阶段 3）：
- 环检测报错
- 未知 id 报错
- 失败/kill 的依赖算满足（不死锁）

**软超时**（阶段 3）：
- soft_timeout 不 kill + tail 返回
- 后续 done 覆盖
- 资源上限触发强制 kill

**关闭流程**（阶段 2）：
- 有 running job 弹确认
- 强制退出 → 全标 failed + reason=forced_exit
- 取消 → 阻止关闭

## 验证

- `cargo check --tests --manifest-path src-tauri/Cargo.toml` + `cargo test --lib`（dev server 占 exe，见 [[ovoice-dev-server-cargo-lock]]）
- `node --check src/main.js`
- 手测（dev server）：
  - 并发 3 无依赖任务 → 一批一气泡
  - C 依赖 A、B → 等 A/B ready 才处理
  - 关 app 重启 → Jobs 面板显示当天 + 主 agent 收到"上次失败"汇报
  - job_id `YYYYMMDD-NN`，重启序号续接
  - 软超时：短 timeout 触发 soft_timeout，进程不杀、tail 返回，agent 可 kill
  - 退出时有 running 任务 → 弹确认

## 非目标

- 任务线 project / vision 层（未来，存档稿）
- 真 resume（job checkpoint / 跨重启续跑进程）
- 自动依赖推断
- 任务板 UI（View 层，待任务线 spec）
- **数据清理 / 归档 / TTL**（数据保留原则明确否决）

---

## 决策记录

- **D1**：job_id 改 String `YYYYMMDD-NN`（不用内部 u64 + 显示格式化，因数字 id 不持久，格式化无意义）。
- **D2**：数据保留——所有 job 输出 + 对话历史永久保留，永不清理。
- **D3**：关闭流程——退出时提醒 running 任务，强制退出标 failed（reason=forced_exit），重启汇报主 agent。取代原 orphaned 标记。
- **D4**：依赖环 spawn 时拓扑检测报错；未知 id 立即报错。
- **D5**：旧数据直接迁移（数字 id → `legacy-<n>`，旧 `.log` 保留）。
- **D6**：调度 idle = turn-busy=false（不等所有后台任务，防饿死）。
- **D7**：软超时不 kill，但设最终资源上限（防僵尸进程累积）。

---

## GSTACK REVIEW REPORT

| 维度 | 发现 | 处理 |
|---|---|---|
| 架构 | A1 依赖环死锁 | D4 修复（spawn 拓扑检测） |
| 架构 | A2 未知 id 死等 | D4 修复（立即报错） |
| 架构 | A3 soft_timeout+done 双 outcome | §3 修复（同 id 取最后） |
| 架构 | A4 idle 检测缺口 | D6 修复（turn-busy 标志） |
| 架构 | A5 跨天 jsonl 落哪天 | §4 修复（跟 spawn 日） |
| 架构 | A6 orphaned 语义 | D3 取代（forced-exit failed + 重启汇报） |
| 代码质量 | Q1 旧数据兼容 | D5 修复（迁移 legacy id） |
| 代码质量 | Q2 DRY（jobs/subagents 各写） | §4 修复（共用状态机 + writer） |
| 代码质量 | Q3 schema 版本化 | §4 修复（schema 字段） |
| 测试 | 缺纯逻辑单测矩阵 | §测试矩阵 补全（6 组） |
| 性能 | P1 soft_timeout 资源累积 | D7 修复（最终上限） |
| 性能 | P2 旧 jsonl 清理 | D2 否决（永久保留） |
| 范围 | 偏大 | §实施路线图 分 3 阶段 |

**VERDICT**：架构方向认可（event sourcing + idle 事件驱动 + 软超时 + 关闭提醒，boring by default 合格）。6 个 gap 全部补进 spec，3 阶段切分降低风险。可进入 writing-plans / 实施。

NO UNRESOLVED DECISIONS
