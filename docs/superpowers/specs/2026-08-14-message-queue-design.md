# 消息缓冲队列设计（Message Queue）

> 日期：2026-08-14 · 分支：`feat/message-queue`（自 master 新开）· 状态：已与用户逐节确认

## 背景与目标

当前 ovoice 前端在 agent 忙（`chatBusy === true`）时，输入框发送被静默忽略（`src/main.js:1191` 直接 return）。用户在 agent 长任务执行期间想补充说明、纠正方向，只能干等或中断重来。

目标：LLM 忙时用户消息进**前端缓冲队列**；agent 完成一轮停下来后，队列所有消息**合并成一条**发出；排队期间可**撤回**、**编辑**、**强制插入**（打断当前 turn 立即发）。

## 已确认的需求决策

| 决策点 | 结论 |
|---|---|
| 批量形态 | 队列多条**合并成一条** user 消息，只触发一个 turn |
| 强插语义 | **打断当前 turn**（等同现有停止按钮）+ 立即合并发送队列 |
| 队列位置 | **纯前端内存**，刷新/重启即丢，不落盘不持久化 |
| UI 展示 | 排队消息以**聊天流内气泡**展示（区别于已发送样式），可编辑/撤回 |
| 排队触发 | `chatBusy` 时排队；**队列非空时新消息也排队**（即使已空闲，先进队再统一发，避免乱序） |

## 非目标

- 不改 Rust 侧任何代码（`chat` command、`SessionEvent`、driver 全复用）
- 排队消息不落 history、不做跨刷新持久化
- 不做队列长度限制、不做合并预览界面

## 架构

```
前端（src/main.js）
├─ pendingQueue: [{id, text, attachments}]        ← 纯内存数组
├─ 发送路径（form submit, main.js:1189）
│   ├─ idle && 队空 → 照旧 invoke("chat")          ← 快路径不变
│   └─ busy || 队非空 → pendingQueue.push(...)     ← 渲染「待发送」气泡
├─ flushQueue()（新）
│   ├─ 触发：agent-done 事件边沿（busy→idle）后 200ms 微延迟
│   ├─ 合并：多条以 "\n\n----\n\n" 连接为一条 text（附件合并取并集）
│   └─ invoke("chat", 合并消息) → 清队列；DOM 上 N 个排队气泡收拢成
│       1 个已发送气泡（首个气泡原位转正显示合并全文，其余删除）
├─ 排队气泡（buildPendingBubble）
│   ├─ 样式：半透明 + 虚线边框 + 「⏳ 待发送」标签（.pending 样式类）
│   ├─ ✏️ 编辑：内容回填输入框（覆盖现有草稿），气泡删除，出队
│   ├─ ✖ 撤回：气泡删除，出队
│   └─ data-pending-id 按 id 操作队列与 DOM
└─ 强插（flushNow）
    └─ 「⚡ 发送并打断」按钮（pendingQueue.length > 0 && chatBusy 时显示）
        → invoke("interrupt_task") → 等 agent-done → flushQueue()
```

Rust 侧现状（全部复用、零改动）：

- `chat` command（`src-tauri/src/lib.rs:84`）→ `SessionEvent::UserMessage` → driver bounded channel（容量 64）
- `interrupt_task` command（`src-tauri/src/lib.rs:202`）→ cancel 当前 turn 的 CancellationToken，返回是否真有 turn 被 cancel
- driver 事件循环（`src-tauri/src/agent.rs:243`）：turn 进行中收到的事件在 channel 排队，turn 结束逐个处理

## 细节设计

### 合并格式

```
第一条内容

----

第二条内容

----

第三条内容
```

分隔符 `\n\n----\n\n`（markdown 水平线，LLM 可识别多条消息边界）。附件（attachments）合并取并集。

### 排队气泡

- 复用 `buildUserBubble` 骨架，加 `.pending` 样式类：半透明 + 虚线边框 + 「⏳ 待发送」小标签
- 操作按钮：✏️ 编辑（回填输入框、气泡删除、出队）、✖ 撤回（气泡删除、出队）
- **历史重载不渲染排队气泡**（排队消息不进 Rust 侧 history）

### 边沿检测与防乱序

```
agent-done 事件到达（busy→idle 边沿）
  ↓ 200ms 微延迟（让潜在的 JobDone 唤醒 turn 先开起来）
  ↓ 二次检查：chatBusy === false && pendingQueue.length > 0
  ├─ 是 → flushQueue()
  └─ 否（又 busy 了）→ 不动，等下一个 agent-done 边沿
```

- 连续多个边沿都因 busy 不 flush：队列保持不动，不丢消息，无告警。
- flush 期间新消息 push 进队列：本轮 flush 只合并 push 之前的快照，新消息等下轮边沿。

### 强插按钮

- 显示条件：`pendingQueue.length > 0 && chatBusy`
- 点击：`interrupt_task` → 等 agent-done → flushQueue()
- 队列为空时不显示（现有停止按钮语义不变）

## 异常处理

| 场景 | 处理 |
|---|---|
| `invoke("chat")` 失败（session 关闭等） | toast 报错，消息**回滚进队列**（气泡恢复排队态），下个边沿重试 |
| 编辑时输入框已有草稿 | 直接覆盖，原草稿丢失（简单优先） |
| 撤回时按 id 查无此项（竞态） | no-op |
| 中断失败（interrupt_task 返回 false，无 turn 在跑） | 直接 flushQueue() |
| flush 中途新消息入队 | 本轮只发快照，新消息留下轮 |
| 刷新/关闭页面 | 队列丢，不提示 |

## 测试策略

Rust 零改动、纯前端逻辑，项目前端无自动化测试框架，沿用手动场景清单验证：

1. busy 时发 3 条 → 三个排队气泡 → turn 结束 → 合并成 1 条发送，气泡转已发送
2. 排队气泡编辑 → 回填输入框，可改可重发
3. 撤回一条 → 其余正常合并发送
4. 强插 → 当前 turn 中断 → 队列立即合并发送
5. turn 结束瞬间 JobDone 又唤醒 turn → 消息不被误发，等下一个空闲边沿
6. chat 调用失败 → 消息回滚回队列，气泡恢复排队态
