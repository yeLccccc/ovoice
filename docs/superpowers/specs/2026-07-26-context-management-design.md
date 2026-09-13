# 上下文管理 v2 设计（Context Management v2）

> **状态**：定稿 + eng-review 折叠（4 条 P1 正确性铁律 + 关键 P2 已并入；详见末尾「Review 修订记录」），待 `writing-plans`
> **日期**：2026-07-26
> **取代**：v1（在 `feat/context-management` 分支存档，设计被推翻）
> **For agentic workers**：本 spec 由 `/superpowers:brainstorming` 产出，核心决策已与用户确认。待全部定稿后进入 `writing-plans`。

**Goal**：给 ovoice agent 引入**长期、分层、忠实、可深思回忆**的上下文管理 —— 一条 append-only 的 history 作为真相之源；context（LLM 窗口）和 display（前端）都从它派生；dream 子代理把 history 整理成分层记忆（日/周/月/年）；agent 通过 mem CLI 深思，可 drill 到一生的任何一段原始对话。

**核心念**：**history 是唯一真相之源；其他都是它的视图。忠实记录一切，永不删。**

---

## §0 为什么重做（v1 → v2）

v1 病根（详见 `feat/context-management` 分支存档）：

- dialogues 只记 4 类 SessionEvent，LLM 正文 / 工具调用 / 结果全没记
- 对话历史纯内存，重启归零
- consolidate 乱触发（绕过空跳过 + idle 语义错 + 开机就触发）
- 清上下文 = 清显示（同一份 messages 干两件事）
- workspace 埋 `%APPDATA%`，备份不便
- MEMORY.md 让 LLM 自由写、无骨架

**v2 核心翻转**：history 是真相之源，context / display 都是它的派生视图。一切可重建、永不丢。

---

## §1 核心架构：三层模型

| 层 | 角色 | 来源 | 生命周期 |
|---|---|---|---|
| **history** | 真相之源，忠实全量记录 | 每个事件实时 append 到 `history/{date}.jsonl` | **永久，永不删** |
| **context** | LLM 工作窗口 | history 的「上次 marker（dream 或 reset）之后 · main 线程」尾段，上限 N 轮 | dream 时清空；**重启从 history 重建** |
| **display** | 前端显示 | history 尾段的懒加载视图（最近 N 条 + 上划加载全部） | **永久可见**，和 context 完全解耦 |

**不变量**：关软件 = 进程退出，history 留盘；重开 = 从 history 重建 context + 重新渲染 display。任何东西都不丢。

---

## §2 命名约定

| 概念 | 命名 |
|---|---|
| 真相之源 | `history/`（不叫 trace；它就是对话历史） |
| 记忆整理子代理 | **dream**（做梦；神经科学里记忆巩固发生在睡眠/做梦。不叫 consolidate/archive） |
| dream 代码 | `spawn_dream_agent` / `DreamTrigger` / `dream_prompt` / dream marker |
| 记忆深思（读侧） | **mem** CLI（不建 LLM 工具） |
| workspace 默认 | `Documents/ovoice/`（脱离 `%APPDATA%`；设置可改） |

---

## §3 pinned 块（每轮注入 messages 前，4 个）

按此顺序：

1. `system_prompt`（主 system）
2. **SOUL.md** —— 灵魂 / 风格，用户写、agent 永不改
3. **AGENT.md** —— 工具规范 + 临时文件管理 + 项目长时维护（用户初版 + agent 沉淀）。**mem CLI 用法写在这里**
4. **MEMORY.md** —— 四层索引（日 / 周 / 月 / 年），dream 维护

**外部状态不做 pinned**（v2 决定）：外部状态太动态，改成**外部事件发生时直接追加进对话流**（history 的 `external` 事件，渲染成 user 消息，类似 ContextNote）。pinned 从 v1 的 5 块减到 4 块。

---

## §4 history 存储

**物理**：`history/{YYYY-MM-DD}.jsonl`，每天一文件，每行一事件 JSON，append-only，永不删。

**单写不变量（铁律）**：`history/` 有三个并发生产者 —— 主 driver（每个 user/assistant/tool/external/edit/marker 事件）、每个子代理（`thread=agent:N` 事件）、dream（marker）。**所有 append 必须经过同一个 history-writer task**（独占文件句柄）；三方只往**一条 mpsc** 投递事件，由 writer 串行落盘 + 分配 `seq`（或入队时 `AtomicU64` 预分配）。不允许缓冲 writer 或多句柄并发 append（会交错损坏行）。这条是 §13.1「进程崩了才丢」承诺的前提。

**事件 schema**：`{seq, ts, thread, kind, ...}`

- `seq` 全局自增（时间序锚点）
- `thread`：`"main"` | `"agent:{id}"`（主对话和每个子代理串在同一条时间线）
- `kind` 与字段：

| kind | thread | 关键字段 |
|---|---|---|
| `user` | main | text, attachments[hash] |
| `assistant` | main / agent:N | content, thinking |
| `tool_call` | main / agent:N | name, args, call_id |
| `tool_result` | main / agent:N | name, result, call_id |
| `subagent_result` | main | agent_id, summary, ref |
| `external` | main | what, path, hash |
| `edit` | main | path, sha_before, sha_after |
| `marker` | main | marker:"dream"\|"reset", until_seq |

---

## §5 子代理模型（异步，不可商量）

```
主 LLM 调 subagent 工具 → tool_call(main, call_id=c_N)
  → 立即返 tool_result(main, c_N, "已派#N后台运行")   ← 主对话继续
  → 子代理后台跑，全过程实时进 thread=agent:N（完整留底）
  → 跑完回调：总结注入主 agent（新一轮 user 消息）+ 写 subagent_result 事件(main)
            {agent_id, summary, ref:"thread=agent:N seq[a,b]"}
```

- 主 context 加载时：`thread=agent:N` 全跳过；`subagent_result` 渲染成 `[子代理#N完成] {summary} [完整对话:ref]` 的 user 消息
- `ref` 是检索锚点：主 agent 想看子代理细节 → 用 mem 按回调 `thread=agent:N`
- **异步是理念级约束（用户拍板），不可改同步**

---

## §6 context 构造（history → LLM messages）

```
1. 扫 history 找最后 marker（dream 或 reset）→ seq S
2. 取 S 之后、thread=="main" 的事件
3. 按 kind 转成 LLM messages（system/user/assistant/tool）：
     user             → {role:user}
     assistant        → {role:assistant}（合并紧随的 tool_call）
     tool_call        → 并入前一条 assistant 的 tool_calls
     tool_result      → {role:tool, tool_call_id}
     subagent_result  → {role:user, "[子代理#N完成] summary [ref]"}
     external         → {role:user, "[外部] ..."}（按 §3 追加进对话流）
     marker           → 跳过
     thread=agent:N   → 跳过
4. 截到 N 轮（`kind=user` 计数；触顶 → 触发 dream）
5. 前面拼 4 个 pinned 块
```

**配对不变量（铁律，违则 MiniMax 400）**：一个 `tool_call` 与其 `tool_result` 是**原子单元**，重建与截断都不得拆开一对、也不得留孤儿。这正是旧 `window_messages` 死保 `tool_call_id` 的同类坑，v2 必须补回：

- 重建时若末尾剩一条带 `tool_calls` 但无后续 `tool` 的 assistant（崩溃/中断留的尾巴）→ **丢弃**该 assistant。
- `external` / `marker` / `subagent_result` 不得插进一对 `tool_call → tool_result` 中间（工具执行同步、result 紧随 call；重建按 call→result 相邻还原）。
- 第 4 步 N 轮截断**配对感知**：截断点若落在 `tool_call` 与其 `tool_result` 之间，回退到该 call 之前切（保对完整），不切在一对中间。

**driver 行为（替换旧"内存 messages 当真相"模型）**：

- 旧 `agent.rs` 把 `messages: Vec<Value>` 当真相、跨轮 mutate、超 40 折叠（`window_messages`）。**v2 删除 `window_messages`**；messages 改为**每轮 turn 开头按本节从 history 重建**，turn 内 `run_turn` 迭代 mutate messages **并同时把每个事件 append 进 history**（经 §4 单写 task）。
- `SessionEvent::Reset`：写一条 `marker:"reset"` 进 history（context 重建从该 marker 之后开始 = 清空），history 本身不动（永不删）。
- `ContextNote`（外部拖入等）：append 进 history 的 `external` 事件，但**不计入 cap**（见 §7.1）。

发出去的就是标准对话 history 格式，MiniMax 直接吃。

---

## §7 dream（记忆整理）

### 7.1 触发时机（两个触发器，都"轮间"触发）

| 触发器 | 条件 | 默认 |
|---|---|---|
| idle | 距**上次用户活动**（发消息 / 拖文件 / 上传）超过 N 秒 | 600s（可配） |
| cap | 本 session `kind=user` 事件数达到 N | 50（可配） |

- **cap 只数 `kind=user`**：`external` / `subagent_result` 虽渲染成 user 消息，但**不触发 cap**（否则后台子代理完成回调会偷偷顶满 cap、在主 agent 思考中途触发 dream）。
- **marker 的 `until_seq` 在 dream-START 盖戳**（dream 开始时观察到的"现在" seq），不是 dream-end：dream 异步跑期间新到的 turn 事件属于**下一段**，不被本 marker 覆盖。避免"dream 跑着、新 turn 已落盘、marker 却盖到之后"的竞态。

**"轮间"触发** = 在一轮 turn 跑完后、下一轮 build_body 之前。dream 异步跑、推进 marker；下一轮 build 从新 marker 之后取 → context 自然清空。**不打断当前 turn。**

### 7.2 修掉 v1 所有 timing 坑

| v1 坑 | v2 修法 |
|---|---|
| startup 就触发 | **绝不在启动 / 关闭触发**。启动只重建 context + 渲染 display |
| idle = 距上次 dream 的时间 | idle = 距**上次用户活动**的时间 |
| 生产循环绕过空跳过 | 触发前查 history：上次 marker 后无新事件 → skip |
| 多个 dream 并发 | `AtomicBool` 单飞：dream 跑着时新触发 noop |

### 7.3 全场景时序

| 场景 | dream? | 行为 |
|---|---|---|
| 用户聊一会停下（idle 10min） | ✅ | 提取这段 → memory + MEMORY「日」+ 清 context |
| 连续聊到 50 轮 | ✅（轮间） | 提取全部 → 清 context；续时靠 MEMORY「日」续 |
| 聊一半直接关软件 | ❌ | 不 dream；重启从 history 重建 context |
| 启动软件 | ❌ | 只重建 context + 渲染 display |
| idle 了但没新事件 | ❌（空跳过） | skip |
| dream 跑着又触发 | ❌（单飞） | noop |
| 开着软件跨午夜 | 下次 dream 时 | 检测新天 → 晋级 |

### 7.4 dream 的三种操作

1. **追加（append）** —— 新事件 → `memory/{date}.md` + MEMORY「日」
2. **晋级（promote）** —— 跨时间边界，按日历对齐上浮（详见 §8）
3. **修正（revise）** —— 新事件与既有记忆矛盾 → 重写受影响 MEMORY 条目（记忆再巩固）

### 7.5 写权限划分（铁律）

| 文件 | 写法 |
|---|---|
| `history/{date}.jsonl` | append-only，永不碰 |
| `memory/{Y}/{M}/{date}.md` | 当天 append（**必须带 `对话索引` 指针**）；跨天冻结 |
| `MEMORY.md` | 可重写（dream 重新组织各层：追加 / 晋级 / 修正） |

### 7.6 dream 完成后 context

context 清空，但 MEMORY「日」pinned 里有刚提取的事件 → agent 不至于失忆，靠 MEMORY + mem 续。

### 7.7 用户无感

dream 完全静默（后台跑，用户看不到）；「[记忆整理中，请忽略]」只进 agent context，**不渲染前端**；无手动按钮；idle_secs / cap 配置项埋进高级设置，默认无感。

### 7.8 dream 每次 run 的 6 步

```
1. 提取 history[上次 dream marker → 现在] 的新事件（记下这段 seq 范围 [a,b]）。**dream 提取边界只认 dream marker**：reset marker 只清 live context（§6），不影响提取 —— history 是真相，reset 过的事件仍发生过、仍进记忆
   → memory/{date}.md（append，**对话索引指针 `seq[a,b]` 由代码盖戳** —— dream 自己读到的 seq；LLM 只写 标题/详情/主语 正文）+ MEMORY「日」（append）
2. 跨午夜了？→ 昨天「日」→「周」
3. 「周」里有天 >7 天？→ 按其 ISO 周概括进「月」，从「周」移除
4. 「月」里有周 >3 月？→ 按其日历月概括进「年」，从「月」移除
5. 新事件与既有记忆矛盾？→ 修正受影响 **MEMORY.md** 条目（冻结的 day 文件永不改写，原始事件是真相）
6. 写 dream marker 到 history（`until_seq` = 第 1 步开始时观察到的 seq，见 §7.1）→ context 清空 + 注入「[记忆整理中，请忽略]」
```

---

## §8 记忆四层（日 / 周 / 月 / 年）

### 8.1 金字塔（粒度递进，日历对齐）

| 层 | 内容形态 | 窗口（滚动） | 概括对齐 |
|---|---|---|---|
| 日 | 每事件一行：`HH:MM evt-NNN 标题` → memory/{date}.md#anchor | 今天 | 事件级 |
| 周 | 每天一行：当天主要事件标题列表 | 最近 7 天 | 超 7 天 → 按 ISO 周概括 |
| 月 | 每周一段：那周的主题概括 | 最近 3 日历月 | 超 3 月 → 按日历月概括 |
| 年 | 每年一行概览 | 无限累积 | 不合年（用户拍板） |

越上越粗、越早。详细数据永远在 `memory/{date}.md`，四层只是不同粗细的索引 / 摘要。

### 8.2 四层 → 存储 + pinned 映射

| 层 | 存哪 | pinned? |
|---|---|---|
| 日 | MEMORY「日」+ `memory/{Y}/{M}/{date}.md` | ✅ |
| 周 | MEMORY「周」 | ✅ |
| 月 | MEMORY「月」 | ✅ |
| 年 | MEMORY「年」（每年一行概览） | ✅ 小 |
| 历史详情（older 月 / 周 / 日） | `memory/{Y}/{M}/{date}.md` 文件树 | ❌ drill 读 |

四层 pinned 都天然有界（日 = 1 天 / 周 = 7 天 / 月 = 3 月 / 年 = 一行每年）；无限涨的只有文件树。MEMORY.md 永远不会爆。

### 8.3 一条事件的时间线（怎么随时间往上走）

```
Day 1 (07-26) 做了任务X
  日: "14:00 evt-001 完成任务X"           ← 每次 dream 追加
Day 2 (07-27，跨午夜)
  日: (今天的新事件)
  周: "07-26: 完成任务X"                   ← 昨天的日条目移上来
Day 8 (08-02，07-26 满 7 天出窗)
  月: "2026-W30: 完成任务X, ..."           ← 07-26 概括进对应 ISO 周
~3 月后 (2026-10，W30 出 月 窗口)
  年: "2026-07: 完成任务X, ..."            ← W30 概括进对应日历月
```

每步变粗。`memory/2026-07-26.md` 里原始事件段永远在（可深思召回）。

### 8.4 `memory/{Y}/{M}/{date}.md` 事件段格式

```markdown
## HH:MM evt-20260726-001 重构 foo.rs 完成
**主语**: agent（子代理#7）
**详情**: 把 foo.rs 拆成 3 个模块，改了 12 处
**对话索引**: history/2026-07-26.jsonl#seq[3,9]   ← 通往原始逐字对话（drill 命脉；**seq 范围由代码盖戳，非 LLM 生成**）
**附件**: a1b2.png（可选）
```

---

## §9 磁盘布局

**workspace**（默认 `Documents/ovoice/`，可配；**备份这一棵 = 全部用户数据**）：

```
Documents/ovoice/
├── SOUL.md                    pinned，用户写
├── AGENT.md                   pinned，用户初版 + agent 沉淀（含 mem 用法）
├── MEMORY.md                  pinned，四层索引，dream 维护
├── history/{date}.jsonl       真相之源，append-only
├── memory/{Y}/{M}/{date}.md   dream 提取的事件详情（带对话索引指针）
└── attachments/{hash}.{ext}   内容寻址附件存储（去重，history 引用 hash）
```

**程序目录**（绿色软件，拷走即用）：`ovoice.exe` + `mem.exe` + `config.json`（portable 优先，next-to-exe）。`%APPDATA%/com.ovoice.app/` 仅作装版 fallback / 迁移期兼容（jobs 运行态、logs），完整 config 迁移单独排、不阻塞 v2。

**备份边界**：备份 `Documents/ovoice/` 整棵 = 全部用户数据（history + memory + 附件 + 三件套）；程序目录（含 config + mem）单独备份即可整体迁移到新机。

---

## §10 mem CLI（深思，读侧）

### 10.1 核心原理

**文件树即索引；mem 是无状态只读导航器；零 LLM。**

- 不建数据库、不维护倒排索引、不与主程序同步状态
- 每次调用现读文件：导航 = 按路径约定 ls / cat；搜索 = 现场对 memory/ grep；原始对话 = 解析 history jsonl 人性化输出
- 单写多读（指 **memory/**）：dream 是 memory 唯一写者，mem 只读，零竞态零漂移。（`history/` 的单写不变量见 §4，写者是那个 history-writer task，不是 dream）

### 10.2 LLM 边界

| | 用 LLM? | 干什么 |
|---|---|---|
| dream（写侧） | ✅ | 提取 / 概括 history → memory 树 + MEMORY.md |
| 主 agent | ✅ | 决定查什么（发 mem 命令、drill 到哪） |
| **mem CLI（读侧）** | ❌ 零 LLM | drill / search / 读原始，纯机械 |

### 10.3 架构形态

- 独立二进制 `mem`（src-tauri 第二个 `[[bin]]` target），与 ovoice 同 crate 共享 `config` + `workspace_io` 模块
- **config 去耦合（前置重构）**：现 `config::load(app)` / `config::path(app)` 依赖 Tauri `AppHandle`，独立 `mem` 二进制没有。抽 `config::load_from(dir: &Path)`（纯函数，不依赖 AppHandle）；Tauri 侧和 mem 都调它。两个进程都触及 `config.json` 时 **app 胜、mem 只读**（mem 不写 config）。
- workspace 定位：mem 找 config 先看自己旁边（绿色）→ 再看 `%APPDATA%`（装版 fallback），读 `workspace_dir`；`--workspace` 覆盖 + `OVOICE_WORKSPACE` 兜底
- **同目录打包（绿色软件，含 release）**：mem.exe 放 ovoice.exe 旁，不写系统 PATH / 不装 / 不进注册表，拷文件夹即用。**release 通过 Tauri `externalBin`/resources 把 mem.exe 落到 ovoice.exe 同目录**（默认 MSI/NSIS 不会自动放第二个 exe 旁边，须显式配）。**v2 以绿色目录为主分发**（`%APPDATA%` 仅装版 fallback / 迁移期兼容，完整迁移单独排）；mem.exe 同目录是 **v2 范畴**，不是迁移项。
- bash 工具 spawn shell 时**运行时**把自身目录加进 PATH（`current_exe()` 父目录，不改系统环境）
- 只读、无状态

### 10.4 命令集（4 条，全机械）

```
mem ls [year [month]]               列年 / 月 / 天（天带标题，读 {date}.md 首行 ##）
mem read <date>                     打印当天事件段（含「对话索引」指针）
mem history <date> [--seq a..b]     读原始对话（人性化渲染，可按 seq 过滤）
mem search <query> [--raw]          关键字 grep（默认 memory/，--raw 扩到 history）
```

### 10.5 完整 drill 链（机械，一步步缩范围）

```
mem ls                         2024 / 2025 / 2026          （选年）
  └ mem ls 2024                01..12 中有数据的月          （选月）
     └ mem ls 2024 7           07-15:讨论胰腺癌 / 07-16:…   （选日，看标题）
        └ mem read 2024-07-15  事件段 + 对话索引#seq[12,47]  （选事件）
           └ mem history 2024-07-15 --seq 12,47  原始逐字对话 （到底）
```

每步输出 = 下步输入，纯 CLI 串联。dream 没跑过也能 drill（只要有 `{date}.md`）。

### 10.6 为什么不需要 `_index.md`

路径里的 Y/M/D 就是时间索引；日文件首行 `##` 就是标题；事件段里的「对话索引」就是指针。三样都机械可读，不需要 dream 额外写索引文件。**`_index.md` 作废**，dream 只写 `{date}.md`（带指针）+ MEMORY.md。

---

## §11 重启行为

- 后端：扫 history 找最后 marker（dream 或 reset）→ 重建 context（其后的 main 事件，上限 N）
- 前端：从 history 尾段懒加载 display（最近 N 条 + 上划）
- 中途关软件再开 = 无缝续聊（context 重建，display 全可见）

---

## §12 配置字段（定稿）

| 字段 | 类型 | 默认 | UI 位置 |
|---|---|---|---|
| `workspace_dir` | String | `Documents/ovoice/` | "工作目录"行（显著 + 文件夹选择器） |
| `dream_idle_secs` | u64 | 600 | 高级 ▸ 记忆管理（收起） |
| `dream_cap_turns` | u64 | 50 | 高级 ▸ 记忆管理（收起） |
| `display_window_size` | u64 | 50 | 高级 ▸ 显示（收起） |

- **无 `dream_enabled` 开关**：dream 永远自动跑（idle + cap），用户无启停
- **`dream_cap_turns` 与 `display_window_size` 都默认 50 但单位不同**（前者 = `kind=user` 事件数，后者 = 可见气泡数），调试别混淆
- **config 存储位置**：portable 优先（next-to-exe，绿色软件）；`%APPDATA%` 作装版 fallback / 迁移期兼容，完整迁移单独排
- settings UI：workspace 显著（备份路径用户该看），其余折进"高级"，默认无感

---

## §13 已定细节（原待讨论项，均已确认）

### 13.1 错误处理

**原则**：优雅降级，**绝不崩 chat turn**；持久化失败告警 + 尽力；重建容错；dream 失败不推进 marker（自动重试）。

| 场景 | 处理 |
|---|---|
| history 写失败（盘满 / 权限） | 后台写 task `warn` + 重试；mpsc 无界 channel 暂存；进程崩了才丢（极端，文档明示）。**不阻塞 turn** |
| 附件复制失败 | 记 hash 进 history，标"未本地化"；mem 显示 hash 但文件缺。降级不崩 |
| dream 超时 / 失败 | **不推进 dream marker** → 下次 idle / cap 自动重试同一段；context 不清空；静默 warn |
| 重建 context 时 history 损坏（坏行 / 半行） | 逐行解析，**跳过坏行**（warn）；末尾半行（崩在 append 中）跳过 |
| 重建/截断造出孤儿 `tool_call`（有 call 无 result） | 按 §6 配对不变量**丢弃**末尾无 result 的 assistant.tool_calls；截断点回退到完整对之前。绝不把未配对 tool_calls 发给 MiniMax |
| dream marker 缺失 / 损坏 | 当作"从头"重建（取全部尾段，上限 N）；首次 dream 处理全量 |
| MEMORY.md 读取失败 | 跳过 MEMORY pinned 块（warn），不写空串 |
| workspace_dir 无效 / 不可建 | 回退 `%APPDATA%/.../workspace` + UI 显著告警"路径无效已回退" |

### 13.2 测试策略

**纯逻辑（`cargo test --lib`，tempfile + FakeLlmRound）**：`history_writer`（序列化 / 跨天切文件 / append-only / **多生产者并发 → seq 单调 + 无交错坏行**）/ `context_builder`（kind→messages / marker 窗口 / N 轮截断 / thread 过滤 / **配对不变量：丢弃孤儿 tool_call、截断不切一对中间**）/ `dream_trigger`（idle 基于 user 活动 / cap **只数 kind=user**（external/subagent_result 不顶 cap）/ 单飞 / 空跳过，全用 fake time）/ `dream_extract`（脚本化 LLM；**验 seq 指针由代码盖戳且 `mem history --seq a..b` 精确命中提取的那段**）/ `dream_promote`（ISO 周 / 日历月对齐，固定日期 fixture）/ `dream_marker_race`（dream 跑期间新 turn 事件归下一段，`until_seq` = dream-start seq）/ `restart_rebuild`（含**末尾孤儿 tool_call 丢弃**回归）/ `mem_cli`（fixture 文件树跑二进制）/ `append_only_guard`。

**必须手测（dev server + 真 LLM）**：dream 真实提取质量 / display 滑动窗口 / 重启无缝续聊 / 端到端（聊 → idle → dream → MEMORY → mem drill → raw history）/ 子代理异步流（派出 → 后台 → 回调注入）。

**测试基建**：`FakeLlmRound`（dream 提取脚本化输出）、tempfile workspace、mem 的 fixture 文件树。

### 13.3 配置字段

见 §12。

### 13.4 mem 打包

- mem 是同 crate 第二个 `[[bin]]`，build 出 `mem.exe` 放 `ovoice.exe` 旁（**绿色软件**，拷走即用）
- 不写系统 PATH / 不装 / 不进注册表
- bash 工具 spawn shell 时**运行时**把自身目录加进 PATH（不改系统环境）
- mem 找 config：先看自己旁边（绿色）→ 再看 `%APPDATA%`（装版 fallback）
- AGENT.md（pinned）写一节"记忆深思（mem）"：列 4 命令 + drill 路径 + "日常不用主动查，MEMORY 已 pinned；只在要远期细节时深思"

### 13.5 display 滑动窗口（前端）

DOM 至多挂 W 条（`display_window_size`，默认 50，可调）；**前后滚动装一头、卸另一头**，窗口滑动，不无限堆积。**limit 数"可见事件"**：`history_tail`/`history_head` 的 limit 跳过隐藏事件（`marker(dream)`、`thread=agent:N`）后再数满 W 条可见气泡；游标是 seq，但分页按可见事件过滤（否则隐藏事件挤占 W、页面出现空缺/短页）。

**后端两命令**：

```
history_tail(limit, before_seq)   → before_seq 之前的 limit 条（往上翻）
history_head(limit, after_seq)    → after_seq 之后的 limit 条（往下翻）
```

**前端窗口 `[lo, hi]`**：

- 启动：`history_tail(W, None)` → 最近 W 条，记 `[lo, hi]`
- 上划到顶：`history_tail(W, lo)` → prepend 老的；卸掉当前最新的 W 条（**保留 hi 游标**，往下翻能再装回）
- 下划到底：`history_head(W, hi)` → append 新的；卸掉最老的 W 条
- 实时新事件（chat stream）：窗口贴底时 append + hi 前移；在读历史时不自动跳（不打扰阅读）
- 边界：返回 < limit = 到顶 / 到底

**事件 → 气泡**：user / assistant / tool_call / tool_result / subagent_result / external 各自气泡；**`marker(dream)` 隐藏不渲染**（用户无感）。

---

## §14 决策记录（本轮 brainstorming）

| 来源 | 决策 |
|---|---|
| 用户 | history 是真相之源；context / display 是派生视图；忠实记录永不删 |
| 用户 | 三层完全解耦；display 懒加载（最近 N + 上划全部），和 context 无关 |
| 用户 | 重启重建 context（无缝续聊），不归档不 fresh |
| 用户 | 外部状态去 pinned，事件直接追加进对话流 |
| 用户 | 子代理**异步后台**（理念级，不可商量）；完成回调注入总结；完整对话留底 + ref 索引 |
| 用户 | 命名 dream（不叫 consolidate / archive）；history/（不叫 trace） |
| 用户 | 记忆管理用户无感、全自动；agent 有感 |
| 用户 | 年不合年、无限涨（信技术发展跟得上） |
| 用户 | 记忆四层日历对齐（自然周 / 日历月），不做固定天数 |
| 用户 | 深思（mem）做成 bash CLI，不建单独 LLM 工具；drill 100% 机械零 LLM |
| 用户 | drill 链年 → 月 → 日 → 对话，全程 CLI 机械串联 |
| 用户 | workspace 可配参数，默认 `Documents/ovoice/`（脱离 APPDATA） |
| v1 教训 | dream 修掉所有 timing 坑（开机触发 / idle 语义 / 空跳过 / 单飞） |
| v1 教训 | 工具计数级联 → 不加 retrieval 工具，改 mem CLI（LLM 工具集不变） |
| v1 教训 | bash Windows 路径坑 → mem 用 Rust 二进制，read 工具读文件 |
| 用户 | display 滑动窗口（W=`display_window_size` 可调），DOM 不无限堆积，前后滚动装一头卸一头 |
| 用户 | dream 不受用户控制（无 `dream_enabled` 开关），永远自动跑 |
| 用户 | 历史更新完全无感：dream marker 前端隐藏、dream 不进 jobs / 不 toast、history 后台写不推 UI |
| 用户 | 绿色软件：mem.exe 与 ovoice.exe 同目录；config 也 next-to-exe（portable 优先，`%APPDATA%` fallback / 迁移单独排） |

---

## §15 实施分期建议（非绑定）

eng-review 与 outside voice 都指出：4 层记忆 + 6 步 dream promotion（§7.8 / §8）对单机个人应用是较大表面积，而**核心价值（history 当真相 + 重启重建 + mem drill）只靠「日」层就能成立**。建议 plan 阶段考虑分期，降低再次推翻的风险：

- **第 1 期（必须）**：history 单写 + context 重建（含配对不变量）+ 重启重建 + 「日」层 + MEMORY「日」+ mem CLI + display 滑动窗口。
- **第 2 期（可延后）**：「周/月/年」晋级（§7.8 step 2-4 / §8.1）+ revise（§7.4 op3）。

分期不改变本 spec 的设计，只给 plan 一个"先交付能跑的最小子"的选项。

---

## Review 修订记录

`gstack-plan-eng-review` + outside voice（codex 鉴权失败 → Claude subagent fallback）折进的变更：

| 来源 | P 级 | 折进位置 | 变更 |
|---|---|---|---|
| review | P1 | §4 | 加 history 单写不变量（一 task 一 mpsc，seq 由 writer 分配）|
| review+OV | P1 | §6 | 加配对不变量（孤儿 tool_call 丢弃 / 截断配对感知 / external 不插对中间）|
| review+OV | P1 | §6 | 加 driver 迁移（每轮从 history 重建 + 删 window_messages + Reset 写 reset-marker + ContextNote 不计 cap）|
| review+OV | P1 | §7.8 / §8.4 / §13.2 | seq 指针**代码盖戳**，LLM 只写正文 |
| review+OV | P1 | §7.1 | cap 只数 `kind=user`；marker `until_seq` 在 dream-start 盖戳（含 OV 新增的 marker 竞态）|
| review+OV | P2 | §10.3 / §13.4 | config 去 AppHandle 化（`load_from`）+ mem 只读 config |
| review+OV | P2 | §10.3 / §13.4 | mem.exe release 同目录（Tauri `externalBin`）；v2 以绿色目录为主分发 |
| review | P2 | §13.5 | display limit 数可见事件（跳过隐藏 marker/agent:N）|
| review | P3 | §12 | `cap_turns` vs `display_window_size` 单位不同注记 |
| review+OV | P2 | §15 | 4 层记忆分期上线建议（非绑定）|

**Cross-model 共识**：单写不变量、配对正确性、seq 盖戳、driver 迁移、mem 二进制 config/打包 —— 两侧独立给出，强信号。Outside voice 额外贡献：N 轮截断配对感知（并入 §6）、cap 只数 `kind=user`、dream marker 竞态 —— 均已折进。
