# 消息缓冲队列（Message Queue）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** agent 忙时用户消息进前端缓冲队列（聊天流内可编辑/撤回的排队气泡），turn 结束后自动合并成一条发送；中断按钮在队列非空时升级为「中断并立即发送队列」。

**Architecture:** 纯前端改动（src/main.js + src/styles.css），Rust 侧零改动。新增 `pendingQueue` 内存数组 + `enqueuePending`/`flushQueue` 两条路径；flush 由 `chat-turn-end`/`chat-error` 事件边沿（`chatBusy` 置 false 处）触发，200ms 微延迟 + 二次检查防「JobDone 立即唤醒下一 turn」的乱序。强插复用现有 interrupt-btn：中断 → turn 结束边沿 → flush 自动发生，无需新按钮。

**Tech Stack:** 原生 JS（无框架、无构建步骤）、Tauri v2 invoke/listen、CSS。

**Spec:** `docs/superpowers/specs/2026-08-14-message-queue-design.md`（已用户确认）

## Global Constraints

- 纯前端实现：**不改 src-tauri/ 下任何文件**（`chat`、`interrupt_task` 命令与 SessionEvent 全复用）
- 排队消息**不落 history**：不调任何新后端命令，纯内存 `pendingQueue` 数组，刷新即丢、不提示
- 合并分隔符固定 `"\n\n----\n\n"`（markdown 水平线）
- 排队触发条件：`chatBusy === true || pendingQueue.length > 0`（队非空时即使空闲也先进队，避免乱序）
- 排队气泡**历史重载不渲染**：live DOM only，`buildHistoryBubbles` 不动
- 排队气泡样式：`.pending` 类 = 半透明 + 虚线边框 + 「⏳ 待发送」标签 + 编辑/撤回操作
- 编辑回填**直接覆盖**输入框现有草稿（含附件），原草稿丢失
- flush 失败（invoke chat 报错）：toast 报错 + 消息回滚进队列头部 + 气泡恢复排队态
- 每个 task 结束跑 `node --check src/main.js` 作为语法门禁（在仓库根目录跑）
- 分支：`feat/message-queue`（已存在，spec 已提交 79d5df8）
- dev server 验证用 `npm run tauri dev`（注意：若 dev server 在跑会锁 exe，纯前端改动热重载不受影响；若遇 cargo 锁报错说明误改了 Rust 文件，回查）

## 现状锚点（实现者必读）

| 锚点 | 位置 | 说明 |
|---|---|---|
| `chatBusy` 状态 | src/main.js:40 | `let chatBusy = false;` |
| form submit | src/main.js:1021-1045 | busy 时 1023 行 `if (micState !== "idle" || chatBusy) return;` |
| `buildUserBubble(text, attachments)` | src/main.js:220-242 | user 气泡构造器（复用其骨架） |
| `addBubble(role, text, opts)` | src/main.js:244-261 | `list.insertBefore(wrap, bottomLoader)` 挂载模式 |
| `chat-turn-end` 监听 | src/main.js:1993-2025 | `chatBusy = false` 在 2009 行 |
| `chat-error` 监听 | src/main.js:2028-2040 | `chatBusy = false` 在 2037 行 |
| interrupt-btn | src/index.html:76-80, src/main.js:52-58 | 中断按钮，`hidden` 切换 |
| `pendingAttachments` | src/main.js:43 | 待发附件数组 `[{staged_path, kind, original_name, size}]` |
| `renderAttachmentBar()` / `toast(msg, isErr)` / `scrollBottom()` | main.js 内已有 | 直接调用 |
| 气泡样式基类 | src/styles.css:155-176 | `.bubble` / `.bubble.user` |
| interrupt-btn 样式区 | src/styles.css:287-307 | 样式插入参考位置 |

---

### Task 1: 排队气泡渲染 + 样式

**Files:**
- Modify: `src/main.js`（在 `buildUserBubble` 函数之后、`addBubble` 之前插入新函数）
- Modify: `src/styles.css`（`.bubble.user` 规则之后，约 176 行处插入）

**Interfaces:**
- Consumes: `buildUserBubble(text, attachments)`（main.js:220，已存在）
- Produces:
  - `buildPendingBubble(item)` — item 形如 `{id: number, text: string, attachments: array}`，返回带 `.pending` 类的 DOM 节点（内含编辑/撤回按钮，回调指向 `editPending`/`withdrawPending`，Task 2 实现）
  - `appendPendingBubble(item)` — 挂载到消息列表底部并滚动
  - CSS 类 `.bubble.user.pending`、`.pending-tag`、`.pending-actions`

- [ ] **Step 1: 在 styles.css 加排队气泡样式**

在 `.bubble.error .bubble-text { color: var(--error); }`（src/styles.css:176）之后插入：

```css
/* 排队气泡：agent 忙时缓冲队列中的未发送消息——半透明 + 虚线边框 + 待发送标签 */
.bubble.user.pending {
  opacity: 0.62;
  border: 1px dashed rgba(255, 255, 255, 0.35);
}
.bubble.user.pending .pending-tag {
  font-size: 12px;
  color: var(--text-tertiary);
  margin-bottom: 4px;
  white-space: normal;
}
.bubble.user.pending .pending-actions {
  display: flex;
  gap: 10px;
  margin-top: 6px;
}
```

- [ ] **Step 2: 在 main.js 加 buildPendingBubble / appendPendingBubble**

在 `buildUserBubble` 函数（main.js:242 的 `}` 之后）与 `addBubble` 之间插入：

```javascript
// ── 消息缓冲队列：agent 忙时用户消息排队（spec: 2026-08-14-message-queue）──
// 排队气泡：复用 user 气泡骨架 + .pending 态（半透明/虚线/标签）+ 编辑/撤回操作。
// 纯 live DOM，不落 history；刷新即丢（spec 决策）。
function buildPendingBubble(item) {
  const wrap = buildUserBubble(item.text, item.attachments);
  wrap.classList.add("pending");
  wrap.dataset.pendingId = String(item.id);
  const tag = document.createElement("div");
  tag.className = "pending-tag";
  tag.textContent = "⏳ 待发送";
  wrap.insertBefore(tag, wrap.firstChild);
  const acts = document.createElement("div");
  acts.className = "pending-actions";
  const editBtn = document.createElement("button");
  editBtn.type = "button"; editBtn.className = "link-btn"; editBtn.textContent = "编辑";
  editBtn.addEventListener("click", () => editPending(item.id));
  const delBtn = document.createElement("button");
  delBtn.type = "button"; delBtn.className = "link-btn"; delBtn.textContent = "撤回";
  delBtn.addEventListener("click", () => withdrawPending(item.id));
  acts.appendChild(editBtn); acts.appendChild(delBtn);
  wrap.appendChild(acts);
  return wrap;
}
function appendPendingBubble(item) {
  list.insertBefore(buildPendingBubble(item), bottomLoader);
  scrollBottom();
}
```

- [ ] **Step 3: 语法门禁**

Run: `node --check src/main.js`
Expected: 无输出（exit 0）

（`editPending`/`withdrawPending` 在 Task 2 定义；本 task 结束时 main.js 引用了未定义函数，`node --check` 只查语法不查引用，所以能过——但这意味着本 task 单独运行时点击按钮会报 ReferenceError，属于预期的中间态，Task 2 完成后闭环。）

- [ ] **Step 4: Commit**

```bash
git add src/main.js src/styles.css
git commit -m "feat(queue): 排队气泡渲染与样式——复用 user 气泡骨架加 pending 态"
```

---

### Task 2: 队列状态 + 入队路径 + 编辑/撤回

**Files:**
- Modify: `src/main.js`（状态声明区 main.js:43 附近 + form submit main.js:1021-1045）

**Interfaces:**
- Consumes: `appendPendingBubble(item)`（Task 1）
- Produces:
  - `let pendingQueue = []` / `let pendingSeq = 0` — 模块级状态
  - `enqueuePending(text, attachments)` — 入队 + 渲染气泡 + 清空输入框/附件栏
  - `editPending(id)` / `withdrawPending(id)` — 按 id 出队（查无 no-op）+ 删 DOM；edit 额外回填输入框与附件（覆盖草稿）
  - `updateInterruptTitle()` — 队列非空且 busy 时 interrupt 按钮 title/aria-label 变「中断并立即发送队列（N 条）」
  - `flushQueue()` — Task 3 实现（本 task 只需保证入队路径不调用它）

- [ ] **Step 1: 加状态变量**

在 `let pendingAttachments = [];`（main.js:43）之后加：

```javascript
let pendingQueue = [];      // 消息缓冲队列：[{id, text, attachments}]，agent 忙时排队（纯内存，刷新丢）
let pendingSeq = 0;         // 队列项自增 id
```

- [ ] **Step 2: 加 enqueuePending / editPending / withdrawPending / updateInterruptTitle**

在 Task 1 插入的 `appendPendingBubble` 之后加：

```javascript
function updateInterruptTitle() {
  const t = chatBusy && pendingQueue.length > 0
    ? `中断并立即发送队列（${pendingQueue.length} 条）`
    : "中断当前任务";
  interruptBtn.title = t;
  interruptBtn.setAttribute("aria-label", t);
}
function enqueuePending(text, attachments) {
  const item = { id: ++pendingSeq, text, attachments: attachments || [] };
  pendingQueue.push(item);
  appendPendingBubble(item);
  updateInterruptTitle();
}
// 撤回：按 id 出队 + 删气泡；查无此项（已 flush 的竞态）→ no-op
function withdrawPending(id) {
  const i = pendingQueue.findIndex(m => m.id === id);
  if (i === -1) return;
  pendingQueue.splice(i, 1);
  document.querySelector(`[data-pending-id="${id}"]`)?.remove();
  updateInterruptTitle();
}
// 编辑：出队 + 删气泡 + 回填输入框与附件（直接覆盖现有草稿，spec 决策）
function editPending(id) {
  const i = pendingQueue.findIndex(m => m.id === id);
  if (i === -1) return;
  const m = pendingQueue.splice(i, 1)[0];
  document.querySelector(`[data-pending-id="${id}"]`)?.remove();
  input.value = m.text;
  pendingAttachments = m.attachments.slice();
  renderAttachmentBar();
  input.dispatchEvent(new Event("input")); // 触发现有 input 高度自适应
  input.focus();
  updateInterruptTitle();
}
```

- [ ] **Step 3: 改 form submit 走队列分支**

把 form submit（main.js:1021-1045）的守卫段：

```javascript
form.addEventListener("submit", async (e) => {
  e.preventDefault();
  if (micState !== "idle" || chatBusy) return;
  const text = input.value.trim();
  const atts = pendingAttachments.slice();
  if (!text && atts.length === 0) return;
```

改为（直发快路径不动，只在忙/队非空时改道）：

```javascript
form.addEventListener("submit", async (e) => {
  e.preventDefault();
  if (micState !== "idle") return; // 语音输入进行中：不排队直接挡（队列语义只跟 chatBusy 走）
  const text = input.value.trim();
  const atts = pendingAttachments.slice();
  if (!text && atts.length === 0) return;
  if (chatBusy || pendingQueue.length > 0) {
    // 忙时排队；队非空时即使已空闲也先进队（由 flush 边沿统一发，避免乱序）
    input.value = "";
    input.style.height = "auto";
    enqueuePending(text, atts);
    pendingAttachments = [];
    renderAttachmentBar();
    return;
  }
```

（原 `if (!text && atts.length === 0) return;` 之后的直发代码保持原样，从 `input.value = "";` 那行开始就是原有代码。）

- [ ] **Step 4: 语法门禁**

Run: `node --check src/main.js`
Expected: 无输出（exit 0）

- [ ] **Step 5: 手动验证（dev server）**

Run: `npm run tauri dev`，发起一个长任务（如让 agent 跑 `ping -n 60 127.0.0.1`），busy 期间：
1. 输入消息回车 → 出现半透明虚线「⏳ 待发送」气泡，输入框已清空
2. 再发第二条 → 第二个排队气泡
3. 点第一个气泡「撤回」→ 气泡消失
4. 点剩余气泡「编辑」→ 内容回填输入框、附件栏恢复、气泡消失
5. idle 时（无 turn）队非空状态发新消息 → 也进队（第 3 个排队气泡）

Expected: 全部符合；中断按钮 hover 提示显示「中断并立即发送队列（N 条）」。

- [ ] **Step 6: Commit**

```bash
git add src/main.js
git commit -m "feat(queue): 入队路径与编辑/撤回——busy 或队非空时改道队列"
```

---

### Task 3: flush 机制（边沿触发 + 合并发送 + 失败回滚）

**Files:**
- Modify: `src/main.js`（chat-turn-end 监听 main.js:2009 附近、chat-error 监听 main.js:2037 附近、新函数加在 Task 2 函数之后）

**Interfaces:**
- Consumes: `pendingQueue`、`updateInterruptTitle()`（Task 2）、`toast()`（已有）、invoke `chat`（已有）
- Produces:
  - `scheduleQueueFlush()` — 幂等调度：200ms 微延迟 + 二次检查（`!chatBusy && pendingQueue.length > 0` 才 flush）
  - `flushQueue()` — async：快照出队 → 合并 text（`"\n\n----\n\n"` join）+ 附件并集 → DOM 收拢（首气泡原位转正显示合并全文，其余删）→ 乐观置 busy → invoke chat → 失败回滚（unshift 回队 + 重建排队气泡）
  - `let queueFlushTimer = null` — 调度去重

- [ ] **Step 1: 加 scheduleQueueFlush / flushQueue**

在 Task 2 的 `editPending` 之后加：

```javascript
let queueFlushTimer = null;
// 边沿触发 flush：chat-turn-end / chat-error（busy→false）时调用。
// 200ms 微延迟让潜在的 JobDone 唤醒 turn 先开起来；二次检查不满足则等下一个边沿。
function scheduleQueueFlush() {
  if (pendingQueue.length === 0 || queueFlushTimer !== null) return;
  queueFlushTimer = setTimeout(() => {
    queueFlushTimer = null;
    if (chatBusy || pendingQueue.length === 0) return; // 又 busy / 已空 → 不动
    flushQueue();
  }, 200);
}
async function flushQueue() {
  const batch = pendingQueue.splice(0, pendingQueue.length);
  const text = batch.map(m => m.text).filter(Boolean).join("\n\n----\n\n");
  const atts = [];
  for (const m of batch) for (const a of m.attachments) atts.push(a);
  // DOM 收拢：首个排队气泡原位转正（显示合并全文），其余删除
  const firstEl = document.querySelector(`[data-pending-id="${batch[0].id}"]`);
  if (firstEl) {
    firstEl.classList.remove("pending");
    firstEl.removeAttribute("data-pending-id");
    firstEl.querySelector(".pending-tag")?.remove();
    firstEl.querySelector(".pending-actions")?.remove();
    const body = firstEl.querySelector(".bubble-text");
    if (body) body.textContent = text;
    const oldStrip = firstEl.querySelector(".attach-strip");
    if (oldStrip) oldStrip.remove();
    if (atts.length) {
      // 重建合并后的附件条（复用 buildUserBubble 的 strip 构造：整体重造再搬 strip 过来）
      const fresh = buildUserBubble("", atts);
      const strip = fresh.querySelector(".attach-strip");
      if (strip) firstEl.appendChild(strip); // .pending-actions 已删，append 即落在气泡末尾
    }
  } else {
    addBubble("user", text, { attachments: atts }); // 气泡不在 DOM（极罕见）→ 重建已发送气泡
  }
  for (const m of batch.slice(1)) document.querySelector(`[data-pending-id="${m.id}"]`)?.remove();
  // 与直发路径一致的乐观置 busy：防 flush→turn-start 之间用户再发产生第二条独立消息
  chatBusy = true;
  sendBtn.disabled = true;
  interruptBtn.disabled = false;
  interruptBtn.hidden = false;
  updateInterruptTitle();
  try {
    await invoke("chat", { text, attachments: atts.map(a => ({ staged_path: a.staged_path, kind: a.kind })) });
  } catch (err) {
    toast("队列发送失败：" + err, true);
    chatBusy = false;
    sendBtn.disabled = false;
    interruptBtn.hidden = true;
    // 回滚：消息回队列头部（保序），气泡恢复排队态
    pendingQueue.unshift(...batch);
    const sent = firstEl && !firstEl.classList.contains("pending") ? firstEl : null;
    if (sent) sent.remove();
    for (const m of batch) appendPendingBubble(m);
    updateInterruptTitle();
  }
}
```

- [ ] **Step 2: 在两个边沿点挂 scheduleQueueFlush**

2a. `chat-turn-end` 监听（main.js:1993 起）里 `chatBusy = false;`（原 2009 行）之后紧跟一行：

```javascript
    chatBusy = false;
    scheduleQueueFlush(); // 队列非空时 200ms 后合并发送（边沿触发）
```

2b. `chat-error` 监听（main.js:2028 起）里 `chatBusy = false;`（原 2037 行）之后同样加：

```javascript
    chatBusy = false;
    scheduleQueueFlush(); // 出错也是 idle 边沿：队列照发（下轮重试语义）
```

- [ ] **Step 3: 语法门禁**

Run: `node --check src/main.js`
Expected: 无输出（exit 0）

- [ ] **Step 4: 手动验证（dev server）**

`npm run tauri dev`：
1. 长任务 busy 期间排 3 条 → turn 结束后 ~200ms：3 个排队气泡收拢成 1 个正常 user 气泡（内容为三段以 `----` 分隔的合并文本），agent 开新 turn 回应合并消息
2. turn 结束瞬间若有 JobDone 唤醒新 turn（可造：busy 时排消息 + 一个后台 job 恰好完成）→ 消息不被误发，等下一个空闲边沿
3. 断网/停 app 模拟 invoke 失败不易造，代码路径 review 确认回滚逻辑（unshift 保序 + 气泡重建）

Expected: 1、2 符合；turn 结束后 `chatBusy` 立刻又被 turn-start 置真时队列原样保留。

- [ ] **Step 5: Commit**

```bash
git add src/main.js
git commit -m "feat(queue): turn-end/error 边沿触发合并发送——200ms 防乱序+失败回滚"
```

---

### Task 4: 强插按钮升级 + reset 清队

**Files:**
- Modify: `src/main.js`（interrupt 按钮监听 main.js:52-58、chat-reset 监听 main.js:2044 附近、form submit 直发路径的 interruptBtn.title 设置处）

**Interfaces:**
- Consumes: `flushQueue()`、`scheduleQueueFlush()`、`updateInterruptTitle()`（Task 2/3）
- Produces: 无新接口（行为完善）

- [ ] **Step 1: interrupt 点击处理兼容「队列空但 idle」直 flush**

interrupt 按钮只在 `chatBusy` 时可见（现有 hidden 逻辑），但守卫放宽松以兜底 flush 失败回滚等边缘。把 main.js:52-58 的监听改为：

```javascript
interruptBtn.addEventListener("click", async () => {
  if (!chatBusy) {
    // 极罕见：按钮可见但 turn 已结束（如 flush 回滚窗口）→ 队列非空直接发
    if (pendingQueue.length > 0 && !chatBusy) flushQueue();
    return;
  }
  interruptBtn.disabled = true;
  interruptBtn.title = "中断中…";
  try { await invoke("interrupt_task", {}); }
  catch (e) { console.warn("[chat] interrupt 失败", e); interruptBtn.disabled = false; updateInterruptTitle(); }
});
```

中断后的 flush 不在这里调：中断 → turn 结束 → `chat-turn-end` 边沿 → `scheduleQueueFlush()` 自动接手（这就是「强插=中断+flush」的实现，无需新按钮）。

- [ ] **Step 2: chat-turn-end 恢复按钮时刷新 title**

`chat-turn-end` 监听里 `interruptBtn.hidden = true;`（原 2011 行）之后加：

```javascript
    updateInterruptTitle(); // 恢复默认 title（队列此时已 flush 或为空）
```

- [ ] **Step 3: chat-reset 清队列**

`chat-reset` 监听（main.js:2044 起）里 `list.replaceChildren(topLoader, bottomLoader);` 之后加：

```javascript
    pendingQueue = [];              // 排队气泡已随 DOM 清掉，队列同步清（消息未发过，直接丢）
    if (queueFlushTimer !== null) { clearTimeout(queueFlushTimer); queueFlushTimer = null; }
```

- [ ] **Step 4: form submit 直发路径的 title 设置改走 updateInterruptTitle**

form submit 直发分支里 `interruptBtn.title = "中断当前任务";`（原 1032 行）改为：

```javascript
  updateInterruptTitle();
```

（flushQueue 内已直接调 updateInterruptTitle，两处发消息路径统一。）

- [ ] **Step 5: 语法门禁**

Run: `node --check src/main.js`
Expected: 无输出（exit 0）

- [ ] **Step 6: 手动验证（dev server）**

`npm run tauri dev`：
1. busy 期间排 2 条 → 点中断按钮（title 显示「中断并立即发送队列（2 条）」）→ 当前 turn 被中断写「用户中断」→ ~200ms 后队列合并发送、气泡转正
2. 设置页点重置（触发 chat-reset）→ 排队气泡与队列全清

Expected: 强插全链路 ≤ 1s 完成；reset 后队列计数归零。

- [ ] **Step 7: Commit**

```bash
git add src/main.js
git commit -m "feat(queue): 中断按钮升级强插语义 + reset 清队 + title 统一管理"
```

---

### Task 5: 端到端场景验证 + final review

**Files:**
- 无新改动（本 task 是验证 + 全分支 review；发现问题回前面的 task 修）

**Interfaces:**
- Consumes: 全部前序 task
- Produces: 验证记录（附在 commit message / 汇报里）

- [ ] **Step 1: 跑 spec 的 6 场景清单**

`npm run tauri dev`，逐条验证（spec「测试策略」节）：

| # | 场景 | 操作 | 预期 |
|---|---|---|---|
| 1 | 排队+合并 | 长任务 busy 时发 3 条 → 等 turn 结束 | 3 排队气泡 → 1 条合并消息（`\n\n----\n\n` 分隔）→ agent 一个 turn 回应 |
| 2 | 编辑 | 排队气泡点「编辑」 | 回填输入框+附件，可改可重发 |
| 3 | 撤回 | 3 条排队撤回 1 条 → flush | 剩 2 条合并发送 |
| 4 | 强插 | busy + 队列非空点中断 | turn 中断 → 队列立即合并发送 |
| 5 | 防乱序 | turn 结束瞬间 JobDone 唤醒新 turn 且队列非空 | 消息不被误发，等下一个空闲边沿 |
| 6 | 失败回滚 | review 代码路径（invoke 失败难稳定复现） | unshift 保序回队 + 气泡恢复排队态 + toast |

场景 5 的造法：让 agent spawn 一个 background bash job（`sleep 5 && echo done`），main agent turn 还在跑时排消息，job 完成会与新 turn 竞争——观察队列消息始终不在错误时机发出。

- [ ] **Step 2: 回归检查**

- idle 直发路径完全不受影响（无队列时行为与改动前一致）
- 语音输入（micState ≠ idle）时发送仍被挡、不入队
- 刷新页面：队列消失、无报错（预期行为，非 bug）
- 历史上翻/下划加载（displayWindow 机制）与排队气泡无冲突（排队气泡只 append 在 bottomLoader 前）

- [ ] **Step 3: 全分支 final review**

对 `git diff master..HEAD` 做整体 review：检查所有 `pendingQueue` 读写点的一致性（enqueue/edit/withdraw/flush/rollback/reset 六处）、`updateInterruptTitle` 调用点覆盖、无 Rust 文件改动（`git diff master..HEAD --stat` 确认只有 main.js/styles.css/spec/plan）。

- [ ] **Step 4: 收尾 commit（如有修复）**

```bash
git add -A
git commit -m "fix(queue): 端到端验证发现的问题修复"
```

（无修复则跳过。）
