# 任务管理子系统（Task Management）设计

> 日期：2026-08-11 ｜ 分支：`feat/task-management` ｜ 状态：设计定稿，待实现

---

## 0. 一句话定位

给 agent 一份**独立于对话流的当前工作记忆**——一个用 sqlite 维护、可查可改、有专门页面可视化的任务状态板，让 agent 能跨越长时段持续推进同一件事。

**不是给人用的 todo app**。任务表的消费者是 agent（经 task 工具），用户侧的 tasks 视图是监督与偶发编辑入口。

---

## 1. 背景与动机

ovoice 是本地优先、长期记忆、可动手干活的桌面 AI 助手（PROJECT.md §0）。现有记忆体系：

- **dream**：长期情景记忆，把对话整理成 日→月→年 三层结构化叙事，管「过去发生了什么」。
- **history**：对话真相源（append-only jsonl）。

缺失的一块是**当前工作记忆**——「现在做到哪、下一步是什么」。agent 没有「任务」这个一等概念，无法跨会话/跨天持续追踪一个目标。用户每次得重新交代 context；关掉重开 agent 不知道之前在做什么。

本子系统填补这块。两支柱分工：

| 支柱 | 性质 | 职责 |
|---|---|---|
| dream（已有） | 长期情景记忆，压缩叙事 | 管「过去发生了什么」 |
| 任务表（新） | 当前工作记忆，状态板 | 管「现在做到哪」 |

---

## 2. 与 MEA（LongHorizon-Harness）的关系

阿里 LongHorizon-Harness 用 MEA 循环（Manager 管理 / Executor 执行 / Auditor 审计）解决长程任务的三个失效模式（错误复合 / 上下文腐化 / 任务状态丢失），核心思想是「把任务状态外置成独立真相源，只允许环境验证过的事实更新」。

本子系统**只吸收 MEA 的状态建模层**，不做执行框架层：

| 层 | 是否做 | 说明 |
|---|---|---|
| 状态建模层（固定目标 + 可验证完成 + 自述/验证分离） | ✅ 吸收 | 落地为 `goal` + `acceptance` + `verified` |
| 执行框架层（Manager/Executor/Auditor 三角色 + fresh-context 循环 + runs/ 落盘） | ❌ 不做 | 桌面助手定位不匹配无人值守自动化 |

---

## 3. 设计边界（红线，强制）

本子系统是**独立子系统，跟对话流零耦合**。两条红线贯穿全文：

```
红线 1：任务功能不碰 context / 不改对话历史
  ├─ 不动 context.rs build_messages
  ├─ 不动 history.jsonl
  ├─ 不动 dream / memory
  ├─ 不动 SOUL/AGENT/MEMORY 块
  └─ 不每轮注入 system（agent 不被动看任务板，按需查）

红线 2：agent 通过 tool 接入 + 有专门可视化页面
  ├─ agent 侧：7 个 task 工具（走 run_turn 工具循环）
  └─ 用户侧：tasks 视图（前端第 5 视图）+ 5 个 Tauri command
```

⚠️ **被这两条红线否决的设计**（曾考虑，现撤回）：
- ~~任务板每轮注入 system 块~~（违反红线 1，是 context 操作）
- ~~build_messages 拼 render_board()~~（违反红线 1）
- ~~AGENT.md「每轮主动 surface 任务」~~（依赖注入，撤了注入就撤这条）
- ~~audit_log 一生历史追溯~~（用户决策：任务表只维护当前态，历史归 dream）

---

## 4. 设计哲学：状态板，不是档案

任务表是**状态板（始终反映现实）**，不是**档案（记 diff）**。

| | 状态板（本设计） | 档案（已否决） |
|---|---|---|
| 隐喻 | 始终反映当前现实 | 记录变更历史 |
| 价值在 | 当前准确 | 可追溯 |
| 维护 | 主动持续同步 | 事情发生就记 |
| 历史 | 不存（归 dream） | audit log |

**关键风险点**：任务表不维护 = 失效。做了一步没勾、卡了没标、完成没 done → 状态板撒谎 → agent 决策基于错误信息。

→ 设计上正视此风险：`verified` 维度 + AGENT.md 自主边界把「同步状态」钉成 agent 核心职责。

---

## 5. 数据模型（Task 实体）

### 5.1 Task 字段（20 字段 + 3 嵌套）

```rust
struct Task {
    // ── 描述层（4）：它是什么 ──
    id:           u64,            // 引用键（父子/工具调用）
    title:        String,         // 名称（识别入口）
    goal:         String,         // 目标/动机（防漂移；区别 title）
    detail:       String,         // 背景约束细节（agent 决策依据）

    // ── 分类结构（3）：它属于哪 ──
    horizon:      Horizon,        // 时间视野 current/short/long/vision
    parent_id:    Option<u64>,    // 父任务（拆解链；None=顶层）
    tags:         Vec<String>,    // 主题标签（跨 horizon 横向分组）

    // ── 状态进度（3）：做到哪了 ──
    status:       Status,         // todo/active/done/dropped（WHERE）
    acceptance:   Vec<CheckItem>, // 验收清单（WHAT'S LEFT）
    verified:     bool,           // 已验证完成（HOW SURE）

    // ── 阻塞产出（2）：卡在哪 / 产出啥 ──
    blockers:     Vec<Blocker>,   // 阻塞项（有未解决 blocker = 卡住）
    artifacts:    Vec<Artifact>,  // 产出物/参考资料（验证靠看产出）

    // ── 排序（1）──
    sort_index:   u32,            // 同 horizon+parent 下排序（兼任优先级）

    // ── 时间轴（5）：生命周期节点 ──
    created_at:   i64,
    updated_at:   i64,            // 最后改动（活跃度判断）
    due_date:     Option<i64>,    // 截止（agent 催的依据）
    completed_at: Option<i64>,    // 完成时间
    archived_at:  Option<i64>,    // 归档（Some=已归档，兼任 archived flag）

    // ── 元数据 + 注入辅助（2）──
    created_by:       Actor,          // user/agent（来源统计）
    last_progress:    Option<String>, // 当前进展摘要（视图展示）
}
```

### 5.2 枚举与嵌套类型

```rust
enum Horizon { Current, Short, Long, Vision }   // 今天/几月~几年/3年+
enum Status  { Todo, Active, Done, Dropped }    // blocked 不在此（由 blockers 派生）
enum Actor   { User, Agent }
enum ArtifactKind { File, Link, Note }

struct CheckItem {
    text:     String,
    done:     bool,
    evidence: Option<String>,   // 验证依据（文件/链接/seq）；done 鼓励带
}
struct Blocker {
    reason:    String,
    raised_at: i64,
    resolved:  bool,
}
struct Artifact {
    kind:      ArtifactKind,
    reference: String,          // 路径/URL/文本
    note:      Option<String>,
}
```

### 5.3 状态进度三元组（核心，详释）

```
status      WHERE（粗粒度位置）    任务处在生命周期的哪个阶段
acceptance  WHAT'S LEFT（剩余清单）完成还差哪些可验证的子目标
verified    HOW SURE（完成判定）   「done」是声称还是已证实

三者回答不同问题，不能互相替代。
```

**Horizon 语义**：
| horizon | 含义 |
|---|---|
| Current | 今天/本周聚焦，可执行下一步 |
| Short | 几周~几个月，有明确产出 |
| Long | 今年~1-3 年，方向性 |
| Vision | 3 年+，指引性远期图景 |

**Status 枚举与流转**：
```
Todo ──start──► Active ──complete──► Done
  │                │
  │                └──► Dropped（放弃，区别 Done：放弃非达成；保留不重犯）
  └──► Dropped（建了就不做）
Active ◄──reopen── Done/Dropped（偶尔重开）
```

⚠️ **无 Blocked enum**：「卡住」由 `blockers` 数组派生——Active + 有未解决 blocker = 卡住。注入/视图渲染时标 ⚠️。理由：blocked 是「正在做但被卡」的子状态，从属 Active，不独立成 enum。

**verified 维度（MEA「完成靠审计不靠自述」的落地）**：
```
status=done + verified=false → 自述完成 ⚠️（待验证）
status=done + verified=true  → 验证完成 ✅（acceptance 全过 + 有 evidence）
status≠done + verified=true  → 逻辑错（禁止）
status≠done + verified=false → 正常未完成
```

agent 自主管理时，自己标 done 容易「自述完成」（MEA 警告的核心）。`verified` 强制留出「声称 vs 证实」的缝。

**progress 派生（不存盘）**：
`progress = acceptance 中 done 数 / 总数`。无 acceptance 的任务无进度，靠 `last_progress` 文本描述。

---

## 6. 状态机汇总

```
status 流转：
  Todo → Active → Done
                → Dropped
  Active ↔ Done/Dropped（reopen）

verified（独立维度）：
  done + verified=false → ⚠️ 自述
  done + verified=true  → ✅ 验证

阻塞派生：
  Active + 有未解决 blocker → 渲染 ⚠️ 卡住
  blocker.resolved=true → 不再算阻塞

归档：
  任意状态可归档（archived_at 盖戳）→ 主视图隐藏，归档视图可查可恢复
```

---

## 7. 持久化（sqlite）

### 7.1 为什么 sqlite（修订前几版曾考虑 json）

任务表是 ovoice 里**唯一的关系型子系统**（父子树 / horizon 过滤 / 时间排序 / 复杂查询），sqlite 索引毫秒级；jsonl 全扫+内存过滤在「一生 1000 任务」量级能扛但查询体验差。依赖用 `rusqlite + bundled` 静态编 C 源码进二进制，无外部 dll，打包无负担。

### 7.2 文件布局

```
cache/
└─ tasks.db                ← 单一 sqlite 文件（一生保留）
   tasks.db-wal            ← WAL（运行时；备份先 checkpoint）
   tasks.db-shm            ← 共享内存（运行时）
```

无 audit 日志文件（任务表只维护当前态，历史归 dream）。

### 7.3 依赖（src-tauri/Cargo.toml）

```toml
rusqlite = { version = "0.31", features = ["bundled"] }
```

`bundled` 用 `cc` crate 编 sqlite C 源码进二进制：
- 无外部 .dll/.so（解决打包顾虑）
- 首次编译 +30s~1min，增量快
- release 包 +~1.5MB（ovoice 现 18MB，可接受）
- `tauri.conf.json` 不用改（不是 externalBin，跟 mem.exe 那套无关）

### 7.4 表结构

```sql
PRAGMA journal_mode = WAL;            -- 读不阻塞写
PRAGMA foreign_keys = ON;
PRAGMA user_version = 1;              -- schema 版本（启动迁移用）

CREATE TABLE tasks (
    id            INTEGER PRIMARY KEY,
    title         TEXT    NOT NULL,
    goal          TEXT    NOT NULL DEFAULT '',
    detail        TEXT    NOT NULL DEFAULT '',
    horizon       TEXT    NOT NULL,             -- current/short/long/vision
    parent_id     INTEGER REFERENCES tasks(id), -- NULL=顶层
    tags          TEXT    NOT NULL DEFAULT '[]', -- JSON array
    status        TEXT    NOT NULL,             -- todo/active/done/dropped
    acceptance    TEXT    NOT NULL DEFAULT '[]', -- JSON [CheckItem]
    verified      INTEGER NOT NULL DEFAULT 0,   -- 0/1
    blockers      TEXT    NOT NULL DEFAULT '[]', -- JSON [Blocker]
    artifacts     TEXT    NOT NULL DEFAULT '[]', -- JSON [Artifact]
    sort_index    INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    due_date      INTEGER,                      -- NULL=无截止
    completed_at  INTEGER,
    archived_at   INTEGER,                      -- NULL=未归档（兼任 archived flag）
    created_by    TEXT    NOT NULL,             -- user/agent
    last_progress TEXT                          -- NULL=无摘要
);
```

**嵌套集合（acceptance/blockers/artifacts/tags）用 JSON 列不拆表**：访问模式是整组读（render 时全要）/ 整组写（update 时替换），不常单独 WHERE 子项；避免 4 表 JOIN 复杂度。未来真要频繁查子项再拆表（YAGNI）。

### 7.5 索引（5 个，覆盖常用查询）

```sql
CREATE INDEX idx_tasks_status_horizon_sort
    ON tasks(status, horizon, sort_index);                      -- 看板
CREATE INDEX idx_tasks_parent     ON tasks(parent_id);          -- 拆解树
CREATE INDEX idx_tasks_due
    ON tasks(due_date) WHERE due_date IS NOT NULL;              -- due-soon（部分索引）
CREATE INDEX idx_tasks_updated    ON tasks(updated_at);         -- 活跃度
CREATE INDEX idx_tasks_active
    ON tasks(id) WHERE archived_at IS NULL
                  AND status IN ('todo','active');              -- 活跃任务（部分索引）
```

### 7.6 async 桥接（关键）

rusqlite 基于 C sqlite3，**同步阻塞 API，Connection 非 Send**，不能跨 await。解决方案：db actor task。

```
db actor task：
├─ 启动：std::thread 或 tokio::task::spawn_blocking 起专门线程
│         持有 Connection（单线程独占 = sqlite 不需要锁，天然单写者）
├─ 通信：mpsc channel 接 DbCommand，返回 oneshot 结果
├─ API：tasks.rs 暴露 async fn task_list(...) → 投 DbCommand → await oneshot
└─ ◄ 跟 ovoice 现有模式同款 ►
    history writer task / job writer task 都是「专门 task 独占句柄串行写」
    db actor 就是「专门线程独占 Connection 串行执行 SQL」，一致性零违和
```

agent 工具（task_list 等）在 tools.rs dispatch 里调 `tasks::task_list(...).await`，对工具透明。

### 7.7 schema 版本化 + 迁移

```rust
// db actor 初始化阶段
let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
match version {
    0 => { create_schema_v1(&conn)?; conn.pragma_update(None, "user_version", 1)?; }
    1 => { /* 当前版本，无需迁移 */ }
    // 未来 2 => { migrate_v1_to_v2(&conn)?; ... }
    _ => bail!("unknown schema version")
}
```

### 7.8 备份

```
在线热备（不锁写）：sqlite3 tasks.db ".backup '$dest'"
或：PRAGMA wal_checkpoint(TRUNCATE) 后复制 .db 单文件
```

### 7.9 边界（不碰其他存储）

任务表是 ovoice **唯一关系型存储**。其他文件栈不动：
```
history.jsonl      对话真相源（append-only）
memory/*.jsonl     dream 整理的情景记忆（append-only）
scheduler.json     定时任务（快照）
config.json        配置（快照）
.ovoice-jobs/      后台任务日志（append-only）
tasks.db           ★ 任务表（sqlite，唯一关系型）
```

---

## 8. agent 工具族（7 个）

### 8.1 工具清单

```
查询（2）
├─ task_list(filter?)         列任务（horizon/status/archived 过滤 + 排序）
└─ task_get(id)               任务详情（goal/acceptance/blocker/artifact 全字段）

写入（5）── 维护状态的动作
├─ task_add(...)              建（title+goal+horizon+parent?+due?+acceptance?+created_by）
├─ task_update(id, patch)     改字段（status 流转 / goal/title/due / horizon / sort_index / blockers）
├─ task_check(id, idx, done, evidence?)   勾验收项（推进 → 派生 progress）
├─ task_progress(id, note)    刷新 last_progress 摘要
└─ task_archive(id, archived) 归档/恢复
```

### 8.2 工具数量级联（⚠️ 易漏点）

`tools.rs` schemas() 当前 12 个工具，加 7 个 task 工具 → **19**。
**必须同步改 `llm.rs` 的 `tools.len()==12` 断言**（memory: ovoice-tool-count-cascade，历史易漏点）。

`dispatch` 加 7 分支，调 `tasks::async API`。

### 8.3 子代理工具集

`SUBAGENT_TOOLS` 维持 `["write","read","edit","bash"]`，**不含 task 工具**。
理由：子代理是「干活手」，不管任务板（任务板是主 agent 的工作记忆，子代理只被派去执行具体子任务）。

### 8.4 工具落 history 的说明

task 工具是普通工具，调用时 `tool_result` 落 history 是 ovoice 既有机制（所有工具都这样），**不是为任务表新增的耦合**。这部分不违反红线 1。

---

## 9. agent 自主管理（Manager 角色）

### 9.1 自主动作（不等用户指令）

```
用户提新目标      → 自主 task_add，告知「已登记 #N」
推进工作          → 自主 task_check 勾验收 + task_progress 记摘要
遇阻塞            → 自主 raise blocker（task_update blockers）
拆解              → 自主建子任务（parent_id 串）
任务做完          → 自主 task_update status=done（默认 verified=false）
工具验证产出后    → 自主升 verified=true（附 evidence）
done 老旧         → 自主 task_archive
```

### 9.2 制衡（自主 ≠ 随意改）

agent 自主管理最大风险：**乱改用户规划 + 自述完成**（MEA 警告）。三道防线：

```
防线 1：verified 维度
  done 默认 verified=false（自述）；升 true 要 evidence + 轻确认

防线 2：AGENT.md 自主边界（推荐「中」）
  完全自主：add / check / progress / raise blocker / 标 done(verified=false)
  轻确认：  改 goal/title（核心定义）· 归档 active · 升 verified=true
  不交 agent：hard delete（只 archive）

防线 3：history + dream 记 agent 行为轨迹
  agent 每次 task 工具调用都落 tool_result 进 history → dream 整理成情景记忆
  事后能从对话回忆「agent 当时做了什么」（无 audit 也问责）
```

### 9.3 核心职责转移

agent 主动行为重心 = **让任务状态始终准确反映现实**（同步状态，非记历史）。
做了一步就 check / 卡了就 blocker / 成了就 done+verified。

### 9.4 AGENT.md 增补内容（红线 1 修正后）

**撤回**：「每轮主动 surface 任务」（依赖注入，撤了注入就撤这条）。
**改为**：当对话涉及任务相关话题（用户提到目标/进度/某任务）时，主动用 task 工具查；agent 决策时按需 task_list/task_get 获取任务上下文。

---

## 10. 用户侧（CRUD + 可视化页面）

### 10.1 Tauri command（5 个，lib.rs 注册）

```
list_tasks(filter?)          列任务（看板渲染）
add_task / update_task       增改（用户手动编辑）
archive_task(id, archived)   归档/恢复
delete_task(id)              硬删（仅用户能；agent 不能，只 archive）
```

这些 command 跟 agent 的 task 工具**共用 `tasks::async API + DbActor`**（同一 db actor，单一写者）。

### 10.2 tasks 视图（前端第 5 视图）

跟 chat/settings/jobs/scheduler 并列的第 5 个视图。

```
看板布局：四列 current | short | long | vision
卡片：
  ├─ title + 进度条（acceptance 勾选率派生）
  ├─ 状态徽章（⚠️blocker / ⏰due / ✅verified / 🗄️archived）
  └─ horizon 颜色区分
卡片交互：
  ├─ 点开 → 编辑弹层（全字段）
  ├─ 拖排 → 更新 sort_index
  └─ 滑动 → 归档
归档视图：切换看「已归档」任务（可恢复）
任务详情：
  ├─ acceptance 勾选（带 evidence 展示）
  ├─ blocker 列表（原因 + raised_at + resolved）
  └─ artifact 列表（文件/链接/笔记）
```

### 10.3 双渲染路径（memory: live-vs-history-render-paths）

**task 工具调用卡在 chat 流**：agent 在 run_turn 里调 `task_list`/`task_add` 等工具时，其 `tool_call`/`tool_result` 进 history（跟 bash/write 工具同一机制），chat 重载时需渲染成工具卡——必须同时改 `setupAgentEvents`（live）和 `buildHistoryBubbles`（history），否则历史重载退化成 JSON。

**tasks 视图的任务卡不在 chat 流**：视图里的看板卡片有自己的渲染路径（独立视图），不触发 chat 双路径。

两条路径独立：
- chat 流渲染「agent 调了什么 task 工具」（工具调用痕迹）
- tasks 视图渲染「当前任务全貌」（看板卡片）

⚠️ 跟红线 1 不冲突：task 工具的 tool_result 落 history 是工具机制的必然（所有工具都这样），不是任务子系统「专门」改 history；任务子系统本身不写 history marker、不注入 system。

### 10.4 实时刷新（task-changed event）

```
db actor 每次 task 写入 → emit "task-changed" event
前端 listen "task-changed" → 重查 list_tasks → 重渲染看板
独立事件通道，不读对话历史（红线 1 兼容）
```

agent 在对话里改任务 / 用户在视图里改任务 → 两侧都触发 emit → 视图实时同步。

---

## 11. 后端架构（src-tauri/src/）

```
新模块 tasks.rs
├─ 类型：Task / Horizon / Status / Actor / ArtifactKind / CheckItem / Blocker / Artifact
├─ DbActor：mpsc 接 DbCommand + oneshot 返回；独占 Connection
├─ migrate(version)：建表 / 索引 / PRAGMA / user_version
├─ async API：task_list / task_get / task_add / task_update / task_check /
│             task_progress / task_archive（agent 工具 + 用户 command 共用）
└─ 启动：spawn 专门线程 + 初始化 Connection + migrate

tools.rs 改造
├─ schemas() 加 7 个 task 工具（12 → 19）
├─ dispatch 加 7 分支（调 tasks::async API）
└─ ⚠️ 同步 llm.rs 的 tools.len() 断言（必查项）

context.rs
└─ 不动（红线 1）

lib.rs
├─ 注册 5 个用户侧 command（list/add/update/archive/delete_task）
├─ setup 阶段启动 DbActor
├─ 关闭时 checkpoint + close
└─ emit task-changed event（DbActor 写入后触发）

AGENT.md 增补
└─ 任务相关话题时主动用 task 工具查（不写每轮 surface）
```

---

## 12. 前端架构（src/）

```
index.html
└─ 新视图 tab「任务」+ 看板四列布局

main.js
├─ 看板渲染（四列 + 卡片 + 拖排 + 编辑弹层）
├─ CRUD 绑定（invoke 用户侧 command）
├─ listen task-changed（agent/用户改 → 实时刷新）
└─ 归档视图切换

styles.css
└─ 看板/卡片/徽章样式（horizon 颜色 / 状态徽章）

⚠️ 不动 setupAgentEvents / buildHistoryBubbles（任务卡不在 chat 流）
```

---

## 13. 数据流（端到端）

```
agent 写入流
  agent 工具调用 task_xxx ─► tools.rs dispatch ─► tasks::async API
    ─► DbActor mpsc ─► 独占线程执行 SQL ─► oneshot 回结果
    ─► tool_result 落 history（既有工具机制，非新增耦合）
    ─► DbActor emit task-changed ─► 前端 tasks 视图刷新

用户编辑流
  tasks 视图操作 ─► invoke 用户 command ─► tasks::async API ─► DbActor ─► SQL
    ─► DbActor emit task-changed ─► 视图刷新

agent 查询流（按需，非每轮注入）
  对话涉及任务话题 / agent 决策需要 ─► task_list / task_get 工具 ─► tasks::async API
    ─► DbActor ─► SQL ─► 返回 ─► tool_result 落 history ─► agent 读结果

（无注入流 —— 红线 1 否决每轮注入 system）
```

---

## 14. 一条任务的完整生命周期（设计自洽验证）

以「写一本电子书《X》」为例：

```
诞生：用户聊「我想写本电子书」→ agent task_add
  {title:"电子书《X》", goal:"系统讲清 X", horizon:long, status:todo,
   acceptance:[], created_by:agent}

定义完成标准：agent 拆解 acceptance
  acceptance = [{第1章},{第2章},{第3章},{校对},{排版PDF}]

开工：task_update status=active

推进：每写完一章 task_check（带 evidence=文件路径）+ task_progress 刷新摘要
  进度派生 2/5 → 3/5

卡住：缺资料 → task_update blockers=[{reason,raised_at,resolved:false}]
  视图标 ⚠️ blocker；agent 对话里被问及时主动说明

解除：资料到手 → blocker resolved=true

完成：acceptance 全勾 + agent 用 read 工具验证产出存在
  → task_update status=done + verified=true（附 evidence 摘要）
  视图移出 active 列

归档：done 一段时间后 task_archive
  archived_at 盖戳；主视图隐藏；归档视图可查可恢复

多年后回忆：dream 早把这段对话整理进情景记忆
  agent bash mem drill → 回看「电子书项目当年怎么做的」
  （任务表现在状态由 db 提供，过程由 dream 提供）

放弃分支：用户说「不写了」→ task_update status=dropped
  保留记录，未来不重犯同样目标
```

---

## 15. 影响面

```
受影响需改动：
├─ tools.rs（schemas +7 / dispatch +7）
├─ llm.rs（tools.len() 断言 12 → 19）
├─ lib.rs（注册 5 command + setup DbActor + task-changed emit）
├─ AGENT.md（增补按需查任务规则）
├─ index.html / main.js / styles.css（tasks 视图）
├─ main.js setupAgentEvents + buildHistoryBubbles（chat 双路径：task 工具调用卡渲染）
└─ Cargo.toml（+rusqlite bundled）

不受影响（红线 1 保证）：
├─ context.rs build_messages（不动）
├─ history.jsonl 路径（task 工具的 tool_result 是既有机制）
├─ dream / memory（不动）
├─ SOUL/AGENT/MEMORY 块（不动）
└─ scheduler / config / jobs / voice 子系统（不动）
```

---

## 16. 不变量与风险

| 不变量 / 风险 | 说明 |
|---|---|
| tool 数量级联 | schemas 12→19 ↔ llm.rs 断言同步（必查，memory: tool-count-cascade） |
| build_messages 不动 | 红线 1；~15 个 context 测试零改 |
| 被动缓存中性 | 不注入 system → 任务表对 M3 缓存零影响 |
| 向后兼容 | tasks.db 不存在 → migrate 建 v1 → 空 tasks → 系统正常 |
| dream 不碰 | dream 读 history 写 memory，任务表独立，零耦合 |
| 单写者一致性 | DbActor 独占 Connection 串行；sqlite WAL 读不阻塞写 |
| 状态板撒谎风险 | 设计正视：维护是主动行为，AGENT.md 规则把「同步状态」钉成 agent 核心职责 |
| agent 乱改风险 | 三防线（verified + AGENT.md 边界 + history/dream 记行为） |
| sqlite 依赖 | bundled 静态编，无外部 dll；release +1.5MB；首次编译 +1min |
| scope 校验 | 无 audit（砍）；无 task_history 工具；无每轮注入；当前态单一真相 |

---

## 17. 风险等级评估

**低-中。**

- 红线 1（不碰 context/history/dream）保证改动隔离在 `tasks.rs` + `tools.rs` + 前端视图，不扩散到对话核心。
- sqlite + bundled 是成熟方案，依赖可控。
- 最大风险点是 **tool 数量级联**（历史易漏）和 **sqlite async 桥接**（db actor 模式 ovoice 有先例：writer task）。
- 设计**完全可逆**：tasks.db 删除 / feat 分支不合并 → 系统回到现状，对话功能零影响。

---

## 18. 改动清单（量级估计）

| 文件 | 改动 | 量级 |
|---|---|---|
| `src-tauri/Cargo.toml` | +rusqlite bundled | ~1 行 |
| `src-tauri/src/tasks.rs`（新） | 类型 + DbActor + migrate + async API | ~400 行 |
| `src-tauri/src/tools.rs` | schemas +7 / dispatch +7 | ~150 行 |
| `src-tauri/src/llm.rs` | tools.len() 断言 12→19 | 1 行 |
| `src-tauri/src/lib.rs` | 5 command + setup DbActor + emit | ~100 行 |
| `src-tauri/ovoice/AGENT.md` | 增补按需查任务规则 | ~15 行 |
| `src/index.html` | tasks 视图 tab + 看板骨架 | ~60 行 |
| `src/main.js` | 看板渲染 + CRUD + listen + chat 双路径（task 工具卡） | ~500 行 |
| `src/styles.css` | 看板/卡片/徽章样式 | ~150 行 |

产品码 + 测试 ≈ **1400~1600 行**。多任务量级（SDD 切片候选）。

---

## 19. 测试策略（概要）

**tasks.rs**：
- migrate：v0→v1 建表 + user_version=1
- CRUD：add/get/list/update/check/progress/archive 全字段往返
- 状态机：status 流转合法性 / verified 逻辑错禁止
- 派生：progress 从 acceptance 计算 / 阻塞从 blockers 派生
- 索引：看板查询 / 拆解树 / due-soon / 活跃任务

**tools.rs**：
- schemas 数量 = 19（同步 llm.rs 断言）
- dispatch 7 个 task 工具路由正确

**lib.rs**：
- 5 个 command 往返
- DbActor 启动/关闭

**前端**：手动验证（无 JS 测试框架）：看板渲染 / CRUD / 实时刷新 / 归档视图。

---

## 20. 后续阶段（不在本 spec 范围）

本 spec 是**任务管理子系统 v1**（当前态维护 + agent 工具 + 可视化页面）。后续可能演进：

- **audit 历史**（如未来需要一生追溯）：加 audit_log 表 + task_history 工具
- **任务板注入**（如未来要 agent 主动 surface）：加 context.rs 注入（违反当前红线 1，需重新评估）
- **dream 任务层整理**：dream 读 tasks.db 整理任务史
- **跨任务聚合查询**：task_search / task_recent 等

这些都不在 v1，标在这里避免遗忘。

---

## 附录 A：被否决的设计（决策记录）

| 设计 | 否决理由 |
|---|---|
| 任务板每轮注入 system 块 | 红线 1（context 操作）；用户明确任务功能不碰 context |
| audit_log 一生历史追溯 | 用户决策：任务表只维护当前态，历史归 dream |
| jsonl 文件存储 | 任务表是关系型子系统，sqlite 索引查询优势真实 |
| MEA 执行框架（Manager/Executor/Auditor 三角色自动化） | 桌面助手定位不匹配无人值守自动化 |
| Blocked 作为 Status enum | 由 blockers 数组派生，从属 Active |
| progress 独立字段 | acceptance 勾选率派生（避免双源） |
| priority / urgency | sort_index + due_date 兼任 |
| started_at | audit 的 status 流转记录兼任（无 audit 则用 updated_at） |
| children 字段 | 从 parent_id 派生 |
| dependencies 字段 | blockers 兼任 |
| archived: bool | archived_at 的 Some/None 兼任 |
| related_convos（关联对话片段） | 靠 dream 回忆，第一版解耦任务表 ↔ 对话 |
| progress_journal | audit + last_progress 兼任（且本版无 audit） |
