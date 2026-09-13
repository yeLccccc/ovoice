# 后台任务渲染（阶段 1）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把后台任务（subagent / bash bg）的系统事件从主对话流移出、修掉显示重影、Jobs 面板卡支持完整输出展开，并落地 AGENT.md 后台结果处理协议——让对话流只剩人话，后台活动归 Jobs 面板。

**Architecture:** 纯前端 + 文档改动，不碰后端。`buildHistoryBubbles`（history 路径）不再把 `external`/`subagent_result` 渲染成伪 user 气泡；Jobs 面板 `buildJobCard` 的 process-kind "完整日志" 从 `alert()` 弹窗改成卡内可展开 `<pre>`；turn-end 重影（#226）先借 Task 1 移除伪 user 气泡验证是否顺带修复，未修复再走 systematic-debugging 深入；AGENT.md 加一段后台任务协议。

**Tech Stack:** Vanilla JS（`src/main.js`，无构建步骤，`node --check` 验证语法）、Markdown（`src-tauri/defaults/AGENT.md`，`include_str!` 嵌入，per-turn `load_pinned` 生效）。

## Global Constraints

- **纯前端 + 文档，零 Rust 改动**：job_id 格式 / 持久化 / 调度模型 / 关闭流程都是阶段 2/3，本分支不碰 `src-tauri/src/*.rs`。
- **验证手段**：`node --check src/main.js`（语法）+ dev server 手测（右键 Reload 刷前端）。dev server 锁 `target/debug/ovoice.exe`，**不要** `cargo build`（见 [[ovoice-dev-server-cargo-lock]]）。改 `AGENT.md` 无需重编译（`load_pinned` per-turn，见 [[ovoice-pinned-files-per-turn-reload]]）。
- **live vs history 双路径**（[[ovoice-live-vs-history-render-paths]]）：本阶段只改 history 路径（`buildHistoryBubbles`）。`external`/`subagent_result` 是后端写入 history 的事件类型，live 路径（`setupAgentEvents`）没有对应渲染入口——故无须同步改 live。
- **数据保留原则**（spec 顶部硬约束）：不删任何历史事件 / job 日志。Task 1 是"不渲染"而非"丢弃事件"——事件仍在 history，`buildHistoryBubbles` 跳过其气泡渲染而已。
- **不引入新依赖、不新建文件**：所有改动落在现有 `src/main.js` + `src-tauri/defaults/AGENT.md`。

---

## File Structure

- **`src/main.js`**（修改）：
  - `buildHistoryBubbles`（~1283-1350）：合并 `external`/`subagent_result` 两分支为"关闭聚合但不渲染气泡"。
  - `buildJobCard` process-kind 日志按钮（~675-678）：`alert()` → 卡内可展开 `<pre>`。
- **`src-tauri/defaults/AGENT.md`**（修改）：在"## 工具"段后追加"## 后台任务协议（subagent / bash bg）"段。
- 无新建文件。

---

## Task 1: buildHistoryBubbles 移除 external / subagent_result 独立气泡

**Files:**
- Modify: `src/main.js:1321-1328`（`buildHistoryBubbles` 的 `external` 与 `subagent_result` 两个 `else if` 分支）

**Interfaces:**
- Consumes: `evField(ev, key)`（已有，读 history 事件字段）、`buildHistoryBubbles` 内部 `asst` 聚合状态。
- Produces: 无新函数/导出。仅改内部分支——`external`/`subagent_result` 事件不再 push group，但仍 `asst = null`。

**背景（为什么不能简单 `continue` 跳过）**：这两个分支当前做两件事——(a) `asst = null` 关闭当前 assistant 聚合气泡（外部事件打断连续 assistant 流），(b) push 一个伪 user 气泡。若直接 `continue` 跳过整段，(a) 不执行 → 后续 `assistant` 事件会续接到此前未关闭的 `asst`，把 external 之后的内容并入 external 之前的气泡（错误聚合）。所以必须保留 `asst = null`，只去掉 push。

- [ ] **Step 1: 改 `buildHistoryBubbles` 的两个分支**

把 `src/main.js:1321-1328` 这段：

```js
    } else if (k === "external") {
      asst = null;
      const what = evField(ev, "what") || "", path = evField(ev, "path") || "";
      groups.push({ node: buildUserBubble(`[外部] ${what} ${path}`.trim(), []), seqStart: ev.seq, seqEnd: ev.seq });
    } else if (k === "subagent_result") {
      asst = null;
      const aid = evField(ev, "agent_id"), summ = evField(ev, "summary") || "", ref = evField(ev, "ref") || "";
      groups.push({ node: buildUserBubble(`[子代理 #${aid} 完成] ${summ} [${ref}]`, []), seqStart: ev.seq, seqEnd: ev.seq });
    } else if (k === "assistant") {
```

替换为：

```js
    } else if (k === "external" || k === "subagent_result") {
      // 后台活动事件（外部触发 / 子代理完成）：不在主对话流渲染独立气泡——
      // 进度与结果归 Jobs 面板（buildJobCard）查看，主对话流只留人话（spec §7.1）。
      // 仍需 asst=null 关闭当前 assistant 聚合，否则后续 assistant 内容会并入此前
      // 未关闭的气泡（外部事件打断连续 assistant 流）。事件本身不丢，仍在 history。
      asst = null;
    } else if (k === "assistant") {
```

- [ ] **Step 2: 语法校验**

Run: `node --check src/main.js`
Expected: 无输出（exit 0）。若报语法错，回 Step 1 检查括号/分号。

- [ ] **Step 3: 手测——历史重载不再出现伪 user 气泡**

dev server 已跑的话，display 窗口右键 Reload。然后：

1. 让主 agent 跑一个后台任务（例："用 bash background 跑 `ping -n 3 127.0.0.1`"），或 spawn 一个子代理（"spawn 子代理数到 10"）。
2. 等任务完成后，**右键 Reload display 窗口**触发历史重载（走 `buildHistoryBubbles`）。
3. 确认主对话流里**不再出现** `[外部] ...` 或 `[子代理 #N 完成] ...` 这类灰色伪 user 气泡。
4. 点开 Jobs 面板，确认该任务**仍在**（Task 1 没动 Jobs 面板，任务卡照常显示）。

Expected: 对话流干净（只剩真 user 提问 + assistant 回答）；Jobs 面板任务完整。

- [ ] **Step 4: Commit**

```bash
git add src/main.js
git commit -m "feat(display): external/subagent_result 移出主对话流

buildHistoryBubbles 不再把 external/subagent_result 渲染成伪 user
气泡（职责归 Jobs 面板）；保留 asst=null 关闭聚合，避免后续 assistant
内容并入前置气泡。spec §7.1。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 2: Jobs 面板 process-kind 完整日志改卡内可展开

**Files:**
- Modify: `src/main.js:675-678`（`buildJobCard` process-kind 的 `[data-l]` 完整日志按钮 onclick）

**Interfaces:**
- Consumes: `invoke("read_job_log", { id })`（已有 Tauri 命令，返字符串日志全文）、`card`（当前 job 卡 DOM）。
- Produces: 无新导出。按钮 onclick 行为从"alert 弹窗"变成"卡内 `<pre>` 展开/收起"。

**背景**：现状（675-678）用 `alert(await invoke("read_job_log", ...))` 弹窗显示日志——弹窗体验差、长日志被截断、无法滚动查找。改成卡内 `<pre>`（点按钮展开再点收起），首次点击拉取，之后纯本地切换。

- [ ] **Step 1: 改 process-kind 日志按钮 onclick**

把 `src/main.js:675-678` 这段：

```js
    card.querySelector("[data-l]").onclick = async () => {
      try { alert(await invoke("read_job_log", { id: j.id })); }
      catch (e) { alert(e); }
    };
```

替换为：

```js
    card.querySelector("[data-l]").onclick = async () => {
      const btn = card.querySelector("[data-l]");
      let pre = card.querySelector(".job-log-full");
      if (pre) {                                   // 已加载过：切换展开/收起
        pre.hidden = !pre.hidden;
        btn.textContent = pre.hidden ? "完整日志" : "收起日志";
        return;
      }
      btn.disabled = true;
      const prevText = btn.textContent;
      btn.textContent = "加载中…";
      try {
        const text = await invoke("read_job_log", { id: j.id });
        pre = document.createElement("pre");
        pre.className = "job-log-full";
        pre.textContent = typeof text === "string" ? text : JSON.stringify(text, null, 2);
        pre.style.cssText =
          "max-height:300px;overflow:auto;white-space:pre-wrap;word-break:break-all;" +
          "margin-top:8px;padding:8px;background:rgba(0,0,0,0.25);border-radius:4px;font-size:12px;";
        card.appendChild(pre);
        btn.textContent = "收起日志";
      } catch (e) {
        alert(e);                                  // read_job_log 失败才回退弹窗
      } finally {
        btn.disabled = false;
        if (!card.querySelector(".job-log-full")) btn.textContent = prevText;  // 失败复原
      }
    };
```

- [ ] **Step 2: 语法校验**

Run: `node --check src/main.js`
Expected: 无输出（exit 0）。

- [ ] **Step 3: 手测——展开/收起 + 失败回退**

dev server display 窗口右键 Reload。然后：

1. 让主 agent 跑一个 process 后台任务（"bash background 跑 `echo line1 && echo line2`"）。
2. 打开 Jobs 面板，找到该 process 任务卡，点"完整日志"。
3. 确认：按钮变"加载中…"→ 卡内出现 `<pre>` 显示日志全文（可滚动）→ 按钮变"收起日志"。
4. 再点"收起日志"→ `<pre>` 隐藏 → 按钮回"完整日志"。再点展开——**不应重新请求**（已加载，纯本地切换，`read_job_log` 只调一次）。
5. （可选）造一个失败：临时把 `read_job_log` 的 id 改错或任务日志被删，点按钮 → 确认弹窗报错 + 按钮复原"完整日志"。

Expected: 展开收起流畅，不重复请求，失败优雅回退。

- [ ] **Step 4: Commit**

```bash
git add src/main.js
git commit -m "feat(jobs): process-kind 完整日志改卡内可展开 <pre>

alert() 弹窗 → 卡内 <pre> 展开/收起；首次拉取后本地切换不再重复
请求 read_job_log；失败回退弹窗 + 按钮复原。spec §7.3。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 3: 验证 + 必要时修 live/history 重影（#226）

> **⚠️ 这个任务不是预设 exact 修法，是 systematic-debugging 流程。** spec §7.2 建议"去掉 `main.js:1566` 的 `!atBottom` 守卫"，但代码分析显示**该守卫本身就是防重影的**（注释 1564-1565 明说：上划翻历史时移除 live wrap，否则与历史聚合气泡重影）。直接去掉会让贴底时 live wrap 也被移除 → turn-end 后气泡消失直到下次 history 重载才回来 → **闪烁**。所以不能盲从 spec 的"去掉守卫"。先借 Task 1 验证 #226 是否其实是伪 user 气泡造成的观感，再决定。

**Files:**
- 可能 Modify: `src/main.js:1553-1584`（`chat-turn-end`）+ `src/main.js:1535-1550`（`chat-turn-start`）。**仅在 Task 3b 确认需要时才动。**

**Interfaces:**
- Consumes: `displayWindow.atBottom`（贴底标志）、`activeAssistantWrap`（当前 live 气泡）、`buildHistoryBubbles`（history 聚合）。
- Produces: 无新导出。

### Task 3a: 借 Task 1 验证 #226 是否已修

**假设**：#226 "显示气泡重复显示用户问答" 的观感，可能来自 `external`/`subagent_result` 被渲染成 `buildUserBubble`（user 样式气泡）混在对话流里——用户看到一堆"伪 user 气泡"以为是重复问答。Task 1 移除后，这个观感可能消失。

- [ ] **Step 1: 重现 #226 原始场景（在 Task 1 已合并的代码上）**

dev server display 窗口右键 Reload。复现 #226 当时观察到的"用户问答重复"场景——通常是：发一条消息 → agent 回答 → 过程中或回答后，用户提问的内容/回答出现两次。

具体手测（参照 #226 原报告）：
1. 发一条普通消息（"你好"），等 agent 回答完。
2. **右键 Reload**（触发 history 重载走 `buildHistoryBubbles`）。
3. 观察：用户气泡"你好"是否只出现一次？assistant 回答是否只出现一次？

- [ ] **Step 2: 判定**

- 若 Reload 后**无重复**（user/assistant 各一份）→ **#226 已由 Task 1 修复**（根因就是伪 user 气泡观感）。跳到 Step 3，Task 3b 不做。
- 若 Reload 后**仍有重复** → 进入 Task 3b（systematic-debugging 深入）。

- [ ] **Step 3（仅 3a 成立时）: Commit 验证记录 + 关闭 #226**

无需代码改动。更新 #226 task 描述记录根因结论，标记 completed：

```
#226 根因：external/subagent_result 被 buildHistoryBubbles 渲染成
buildUserBubble（伪 user 气泡）混入对话流，观感如"重复问答"。
Task 1（移出主对话流）已修。Reload 验证无重复。
```

### Task 3b: 若 3a 未修复——systematic-debugging 深入

**只在 Task 3a Step 2 判定"仍有重复"时执行。** 用 superpowers:systematic-debugging，禁止跳过 Phase 1 直接猜修法。

- [ ] **Step 1: Phase 1 根因调查**

精确重现（不能只凭印象）：
1. 在 display 窗口，记录重现 #226 的**确切操作序列**（发什么、何时 Reload、是否滚动）。
2. 打开 display 窗口 DevTools（右键 → 检查，或 F12），Reload 后在 Elements 面板数 `.bubble.user` 和 `.bubble.assistant` 节点数，确认"哪个气泡真的重复了"。
3. 看 Console 的 `[diag] turn-start` / `[diag] turn-end` 日志（main.js:1536/1554），记录 `atBottom` / `hadWrap` / `chatBusy` 值。

候选根因（逐一查证，不要一次改多个）：
- **C1**：`chat-turn-end` 贴底时（`atBottom=true`）保留 live wrap（1566 守卫跳过 remove），但 `hi` 更新到最新 seq 后，若用户随后滚动触发 history 重载，`buildHistoryBubbles` 又把同 turn 渲染一遍 → live + history 双显。
- **C2**：`chat-turn-start`（1535）新建 wrap 时 `reachedBottom=false`（1545），若紧接着 loader 的下划加载分支命中，把刚落 history 的本 turn 事件又装回（注释 1547 提到的 bug #2 残留）。
- **C3**：`AT_BOTTOM_PX` 容差（注释 1227，~32px）在边界抖动，`atBottom` 在 true/false 间跳变，导致 1566 守卫时移时留。

- [ ] **Step 2: Phase 2-3 假设与最小验证**

对确认的最可能根因，**写一个最小复现**（如 DevTools 手动设 `displayWindow.atBottom=false` 后触发 turn-end，观察是否双显）。形成单一假设（"C1 因为 X"），用最小改动验证（如临时在 1566 加 `console.log` 看 live wrap 是否真与 history 重叠）。

- [ ] **Step 3: Phase 4 修复（确认根因后）**

基于确认的根因应用对应修法。**候选方向**（不是 exact 修法，需 Step 1-2 确认后选）：
- 若 C1：turn-end 时无论 `atBottom` 与否都 remove live wrap，并立即从 `history_tail` 拉本 turn 事件用 `buildHistoryBubbles` 重建插入（"以 history 为唯一真相源"，spec §7.2 的正确落地——代价是多一次拉取）。
- 若 C2：在 `chat-turn-start` 后给 loader 下划加载加 turn 边界保护（跳过 hi 已覆盖的区间）。
- 若 C3：放宽 `AT_BOTTOM_PX` 或对 `atBottom` 加防抖。

任何修法必须配 Step 4 验证。

- [ ] **Step 4: 验证修复**

重复 Task 3a Step 1 的重现序列，确认无重复；且不引入闪烁（贴底时 turn-end 气泡不消失）。若引入新问题，回 Phase 1（不要叠加第二个修法）。

- [ ] **Step 5: Commit**

```bash
git add src/main.js
git commit -m "fix(display): live/history 重影（#226）

<根因一句话>。<修法一句话>。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

（commit message 按实际根因/修法填，不要留尖括号占位。）

---

## Task 4: AGENT.md 加"后台任务协议"段

**Files:**
- Modify: `src-tauri/defaults/AGENT.md`（在 `## 工具` 段之后、`## 路径与存放` 段之前插入新段）

**Interfaces:**
- Consumes: 无。
- Produces: `AGENT.md` 新增一段行为协议，`include_str!` 嵌入二进制，per-turn `load_pinned` 注入主 agent 上下文。

**背景**：spec §8 要求加"后台结果处理协议"。阶段 1 只落地**当前已实现**的部分（后台结果在 Jobs 面板看、不复述全文、多结果分发 sub-agent）。`soft_timeout` / 重启汇报属阶段 3 功能，本阶段不写（避免承诺未实现的行为）。

- [ ] **Step 1: 在 AGENT.md 插入新段**

打开 `src-tauri/defaults/AGENT.md`，找到 `## 工具` 段末尾（`- 直接改盘用 edit...弹卡片让用户改用 edit_card。` 这行之后），在它和 `## 路径与存放（必守）` 之间插入：

```markdown

## 后台任务协议（subagent / bash bg）
后台任务（子代理、bash background）的进度与结果**在 Jobs 面板查看**，主对话流只留人话总结，不复述结果全文。
- 被 job 完成唤醒后，只在「要让用户知道 / 要展示产物 / 要用户决策」时才开口；纯确认性回流（"做完了"、空跑成功）不必复述。
- 多个互不相关的后台结果同时回来：分别 spawn sub-agent 深入处理，自己只做协调 + 一句汇报，别把 N 份结果摊在对话里。
- 任务产物（生成的文件）落 workspace 后用 display 展示卡，别把文件内容贴进文字。
```

- [ ] **Step 2: 验证（per-turn 生效，无需重编译）**

dev server 不用重启。新开一轮对话（发任意消息触发新 turn → `load_pinned` 重读 AGENT.md）。让主 agent 跑一个后台任务，观察它完成后是否：
1. 不在主对话流复述结果全文（只一句总结或不说）。
2. 引导用户看 Jobs 面板 / 用 display 展示产物。

Expected: agent 行为符合协议（不复述、分发、展示卡）。

- [ ] **Step 3: Commit**

```bash
git add src-tauri/defaults/AGENT.md
git commit -m "docs(agent): 加后台任务协议段（不复述/分发/展示卡）

后台结果归 Jobs 面板，主对话流只留人话。spec §8 阶段1 部分
（soft_timeout/重启汇报留待阶段3）。

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage**（spec §7 + 改动清单-阶段1 + §8）：
- §7.1（external/subagent_result 移出主对话流）→ Task 1 ✅
- §7.2（修 live/history 重影 + 收 #226）→ Task 3 ✅（且纠正了 spec "去掉守卫" 的误判）
- §7.3（Jobs 面板卡完整输出可展开）→ Task 2 ✅
- §8（AGENT.md 后台结果处理协议）→ Task 4 ✅
- 改动清单-阶段1 四项（main.js 移除独立气泡 / 修重影 / Jobs 卡增强 / AGENT.md 协议）→ Task 1/3/2/4 全覆盖 ✅

**2. Placeholder scan**：Task 3b Step 5 的 commit message 有 `<根因一句话>` 占位——但 Task 3b 本身是"根因确认后才执行"的调试任务，commit 文案在确认根因后自然填入，这是合理的（不是计划空洞，是调试任务的结果依赖）。其余步骤均含 exact code / exact 命令 / exact 手测。✅

**3. Type consistency**：Task 1 的 `asst` / `evField` / `buildUserBubble`（移除其调用）、Task 2 的 `invoke("read_job_log", { id })` / `card` / `j.id`、Task 3 的 `displayWindow.atBottom` / `activeAssistantWrap` 均与现有代码签名一致。Task 2 新增的 `.job-log-full` class 是新名字，无冲突。✅

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-27-job-display.md`. Two execution options:

**1. Subagent-Driven (recommended)** — 我每个 task 派一个 fresh implementer subagent，task 间做 spec 合规 + 代码质量双 review，最后整分支 review。Task 3b（若触发）单独走 systematic-debugging。

**2. Inline Execution** — 在本会话内用 executing-plans 顺序执行，带 checkpoint。

**注意**：Task 3 有分支（3a 验证 → 可能直接关 #226，或进 3b 深入）。subagent-driven 执行 Task 3 时，implementer 先做 3a 手测，据结果决定是否触发 3b——不要跳过 3a 直接猜修法。
