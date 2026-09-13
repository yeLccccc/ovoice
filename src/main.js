// ovoice 前端：chat / speak / asr_one_shot / get_config / save_config。
import { startRecorder } from "./voice.js";
import { renderMarkdown, enhanceMarkdown, bindLinkOpener } from "./markdown.js";

const { invoke } = window.__TAURI__.core;
const { convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// 自绘标题栏窗口控制（decorations:false）。包 try/catch：即便窗控 API 不可用，
// 也不能中断下方聊天主逻辑（IIFE 在文件顶部，抛错会吞掉后续初始化）。
(() => {
  try {
    const w = window.__TAURI__;
    if (!w?.window?.getCurrentWindow) { console.warn("[titlebar] __TAURI__.window 不可用，窗控按钮将失效"); return; }
    const win = w.window.getCurrentWindow();
    const minBtn = document.getElementById("win-min");
    const maxBtn = document.getElementById("win-max");
    const closeBtn = document.getElementById("win-close");
    const syncMax = async () => {
      const maxed = await win.isMaximized().catch(() => false);
      maxBtn.classList.toggle("is-max", maxed);
      maxBtn.title = maxed ? "还原" : "最大化";
      maxBtn.setAttribute("aria-label", maxed ? "还原" : "最大化");
    };
    minBtn?.addEventListener("click", () => win.minimize());
    maxBtn?.addEventListener("click", async () => { await win.toggleMaximize(); syncMax(); });
    closeBtn?.addEventListener("click", () => win.close());
    win.onResized(() => syncMax());
    // 整条标题栏可拖动窗口：左键按下、且落点不在按钮上时触发 startDragging
    const topbar = document.querySelector(".topbar");
    topbar?.addEventListener("mousedown", (e) => {
      if (e.button !== 0) return;
      if (e.target.closest("button")) return;
      win.startDragging();
    });
    syncMax();
  } catch (e) { console.error("[titlebar] 初始化失败:", e); }
})();

let chatBusy = false;        // 工具循环进行中：阻止语音重入（M3）
let activeAssistantWrap = null; // 事件路由目标（chat-turn-start 创建，turn-end 清空）
let lastConfig = null;  // 最近加载的 config：readForm 回传非表单字段（media_roots / minimax_region），防 save 清空
let pendingAttachments = []; // [{staged_path, kind, original_name, size}] 待发附件
let pendingQueue = [];      // 消息缓冲队列：[{id, text, attachments}]，agent 忙时排队（纯内存，刷新丢）
let pendingSeq = 0;         // 队列项自增 id

const form = document.getElementById("chat-form");
const input = document.getElementById("chat-input");
const list = document.getElementById("messages");
const sendBtn = document.getElementById("send-btn");
const interruptBtn = document.getElementById("interrupt-btn");
// 中断当前 turn:直接 cancel 后端 token(run_turn 在工具间隙/流式中途退出,写「用户中断」兜底)。
// 不经 driver 队列——命令并发执行,否则排在 turn 后面就没中断意义了。
interruptBtn.addEventListener("click", async () => {
  if (!chatBusy) {
    // 极罕见：按钮可见但 turn 已结束（如 flush 回滚窗口）→ 队列非空直接发
    if (pendingQueue.length > 0) flushQueue();
    return;
  }
  interruptBtn.disabled = true;
  interruptBtn.title = "中断中…";
  try { await invoke("interrupt_task", {}); }
  catch (e) { console.warn("[chat] interrupt 失败", e); interruptBtn.disabled = false; updateInterruptTitle(); }
});
const subtitle = document.getElementById("subtitle");
const micBtn = document.getElementById("mic-btn");
const attachBtn = document.getElementById("attach-btn");
const attachBar = document.getElementById("attach-bar");

// 视图切换
const chatView = document.getElementById("chat-view");
const settingsView = document.getElementById("settings-view");
const settingsBtn = document.getElementById("settings-btn");
const settingsBack = document.getElementById("settings-back");
const configForm = document.getElementById("config-form");
const bgLayer = document.getElementById("bg-layer");

// 任务看板视图
const tasksView = document.getElementById("tasks-view");
const tasksBtn = document.getElementById("tasks-btn");
const tasksBack = document.getElementById("tasks-back");
const tasksRefresh = document.getElementById("tasks-refresh");
const tasksNew = document.getElementById("tasks-new");
const tasksShowArchived = document.getElementById("tasks-show-archived");
const tasksEmpty = document.getElementById("tasks-empty");
const tasksBoard = document.getElementById("tasks-board");
const taskModal = document.getElementById("task-modal");
let tasksCache = [];        // 当前展示的任务数组
let editingTaskId = null;   // null=新建模式；number=编辑模式

const INPUT_PLACEHOLDER = input.placeholder;

// 配置表单字段
const F = {
  apiKey: document.getElementById("cfg-api-key"),
  llmModel: document.getElementById("cfg-llm-model"),
  systemPrompt: document.getElementById("cfg-system-prompt"),
  workspace: document.getElementById("cfg-workspace"),
  cache: document.getElementById("cfg-cache"),
  ttsModel: document.getElementById("cfg-tts-model"),
  voiceId: document.getElementById("cfg-voice-id"),
  voiceIdCustom: document.getElementById("cfg-voice-id-custom"),
  voiceLabelCustom: document.getElementById("cfg-voice-label-custom"),
  addVoiceBtn: document.getElementById("cfg-add-voice"),
  workspacePick: document.getElementById("cfg-workspace-pick"),
  cachePick: document.getElementById("cfg-cache-pick"),
  speed: document.getElementById("cfg-speed"),
  vol: document.getElementById("cfg-vol"),
  pitch: document.getElementById("cfg-pitch"),
  audioFormat: document.getElementById("cfg-audio-format"),
  baiduAppid: document.getElementById("cfg-baidu-appid"),
  baiduApikey: document.getElementById("cfg-baidu-apikey"),
  baiduSecret: document.getElementById("cfg-baidu-secret"),
  baiduDevpid: document.getElementById("cfg-baidu-devpid"),
  holdGate: document.getElementById("cfg-hold-gate"),
  voiceTrigger: document.getElementById("cfg-voice-trigger"),
  voiceAction: document.getElementById("cfg-voice-action"),
  bgEnabled: document.getElementById("cfg-bg-enabled"),
  bgPath: document.getElementById("cfg-bg-path"),
  bgPick: document.getElementById("cfg-bg-pick"),
  bgClear: document.getElementById("cfg-bg-clear"),
  bgOpacity: document.getElementById("cfg-bg-opacity"),
  bgOpacityVal: document.getElementById("cfg-bg-opacity-val"),
  glassOpacity: document.getElementById("cfg-glass-opacity"),
  glassOpacityVal: document.getElementById("cfg-glass-opacity-val"),
  maxAttachmentMb: document.getElementById("cfg-max-attachment-mb"),
  maxSubagents: document.getElementById("cfg-max-subagents"),
  dreamIdleSecs: document.getElementById("cfg-dream-idle-secs"),
  dreamContextTriggerTokens: document.getElementById("cfg-dream-context-trigger-tokens"),
  dreamCapTurns: document.getElementById("cfg-dream-cap-turns"),
  displayWindowSize: document.getElementById("cfg-display-window-size"),
  maxToolIters: document.getElementById("cfg-max-tool-iters"),
  userPrefix: document.getElementById("cfg-user-prefix"),
  userPrefixEnabled: document.getElementById("cfg-user-prefix-enabled"),
};

// 当前音色候选 {id,label}（默认 + 自定义），保存时整体写回 config.voices
let voicesState = [];

// 把 voicesState 渲染进音色下拉，并选中 currentId（不在列表则补一个临时项）。
function renderVoices(currentId) {
  F.voiceId.innerHTML = "";
  let found = false;
  for (const v of voicesState) {
    const opt = document.createElement("option");
    opt.value = v.id;
    opt.textContent = `${v.label}（${v.id}）`;
    if (v.id === currentId) { opt.selected = true; found = true; }
    F.voiceId.appendChild(opt);
  }
  if (!found && currentId) {
    const opt = document.createElement("option");
    opt.value = currentId;
    opt.textContent = `${currentId}（未在候选列表）`;
    opt.selected = true;
    F.voiceId.insertBefore(opt, F.voiceId.firstChild);
  }
}

// 历史重渲染（buildHistoryBubbles）期间抑制 scrollBottom——渲染器自带 scrollBottom 会跳底、
// 破坏上下滚动加载的 scroll anchoring。buildHistoryBubbles 进出时置 true/false。
let _suppressScroll = false;
function scrollBottom() {
  if (_suppressScroll) return;
  // 用户上划翻历史（atBottom=false）时不强拉视口——否则 scroll 事件命中 scroll-down 分支，
  // history_head 回拉本回合已落 history 的事件（user/assistant/tool_result），
  // appendHistoryBubble 再渲染一遍 → 与 live 气泡/工具卡重复（用户气泡重复 + 工具拆分，bug #2 延伸）。
  if (!displayWindow.atBottom) return;
  list.scrollTop = list.scrollHeight;
}

const ATTACH_ICON = { image:"🖼", video:"🎬", audio:"🎵", html:"🌐", pdf:"📄",
  docx:"📝", csv:"📊", markdown:"📑", text:"📃", unsupported:"❓" };

// task_* 工具的人话名（spec §10.3 / plan Task 11）。命中则用，未命中回退 `🔧 <name>`。
const TASK_TOOL_LABEL = {
  task_list: "📋 列任务",
  task_get: "📋 查任务详情",
  task_add: "✏️ 建任务",
  task_update: "✏️ 改任务",
  task_check: "✅ 勾验收",
  task_progress: "📍 记进展",
  task_archive: "🗄️ 归档",
};
function taskToolLabel(name) {
  return TASK_TOOL_LABEL[name] || null;
}

// 把 staged 结果并入待发列表并刷新预览条。
function addStaged(items) {
  for (const it of items) {
    if (!it.ok) { toast((it.error || "添加失败") + (it.original_name ? `：${it.original_name}` : ""), true); continue; }
    pendingAttachments.push(it);
  }
  renderAttachmentBar();
}

function renderAttachmentBar() {
  attachBar.innerHTML = "";
  if (pendingAttachments.length === 0) { attachBar.hidden = true; return; }
  attachBar.hidden = false;
  for (let i = 0; i < pendingAttachments.length; i++) {
    const a = pendingAttachments[i];
    const chip = document.createElement("span");
    chip.className = "attach-chip";
    chip.setAttribute("role", "listitem");
    chip.setAttribute("aria-label", `${a.original_name}，${a.kind}，按删除键移除`);
    chip.tabIndex = 0;
    const mb = (a.size / 1048576).toFixed(a.size > 1048576 ? 1 : 2);
    chip.innerHTML = `<span class="attach-ico">${ATTACH_ICON[a.kind] || "📎"}</span>`
      + `<span class="attach-name"></span><span class="attach-meta">${mb} MB</span>`;
    chip.querySelector(".attach-name").textContent = a.original_name;
    const x = document.createElement("button");
    x.type = "button"; x.className = "attach-x"; x.setAttribute("aria-label", `移除 ${a.original_name}`);
    x.textContent = "✕";
    const idx = i;
    const remove = () => { pendingAttachments.splice(idx, 1); renderAttachmentBar(); };
    x.addEventListener("click", remove);
    chip.addEventListener("keydown", (e) => {
      if (e.key === "Backspace" || e.key === "Delete") { e.preventDefault(); remove(); }
    });
    chip.appendChild(x);
    attachBar.appendChild(chip);
  }
}

// 文件选择 → stage → 入栏
async function stageFiles(filePaths) {
  if (!filePaths || filePaths.length === 0) return;
  try {
    const items = await invoke("stage_attachments", { paths: filePaths });
    addStaged(items);
  } catch (e) { toast("添加附件失败：" + e, true); }
}

// 📎 → tauri-plugin-dialog 原生选择器拿真实路径（<input type=file> 在 Tauri v2 webview 拿不到 .path）
async function pickAttachmentPaths() {
  try {
    const picked = await invoke("plugin:dialog|open", { options: { multiple: true } });
    if (!picked) return [];
    return Array.isArray(picked) ? picked : [String(picked)];
  } catch (e) { toast("选择文件失败：" + e, true); return []; }
}
if (attachBtn) {
  attachBtn.addEventListener("click", async () => {
    const paths = await pickAttachmentPaths();
    if (paths.length) stageFiles(paths);
  });
}

// 构造一条 user 气泡节点（不挂载；调用方决定 append/prepend/插到流式 wrap 内）。
// 实时发送（addBubble）与历史回放（appendHistoryBubble）共用，DRY。
// attachments 元素既兼容 live 的 {staged_path,kind,original_name,size} 也兼容 history 存储的
// {staged_path,kind}（缺 original_name 时从 basename 推）。返回新建的 wrap。
function buildUserBubble(prefix, text, attachments) {
  const wrap = document.createElement("div");
  wrap.className = "bubble user";
  // 前缀非空时折叠显示（默认收起，照 assistant「思考过程」details 模式）。
  // 前缀是用户配置的固定模板，与用户实际输入区分开：收起只露 summary，点开看全文。
  const pfx = (prefix == null ? "" : String(prefix)).trim();
  if (pfx) {
    const det = document.createElement("details");
    det.className = "bubble-prefix";
    const sum = document.createElement("summary");
    sum.textContent = "前缀";
    const pbody = document.createElement("div");
    pbody.className = "prefix-body";
    pbody.textContent = prefix; // 原文（含换行），不用 innerHTML
    det.appendChild(sum);
    det.appendChild(pbody);
    wrap.appendChild(det);
  }
  const body = document.createElement("div");
  body.className = "bubble-text";
  body.textContent = text || "";
  wrap.appendChild(body);
  const atts = Array.isArray(attachments) ? attachments : [];
  if (atts.length) {
    const strip = document.createElement("div");
    strip.className = "attach-strip";
    for (const a of atts) {
      const chip = document.createElement("span");
      chip.className = "attach-chip";
      const name = a.original_name || String(a.staged_path || "").split(/[\\/]/).pop() || "(附件)";
      chip.innerHTML = `<span class="attach-ico">${ATTACH_ICON[a.kind] || "📎"}</span><span class="attach-name"></span>`;
      chip.querySelector(".attach-name").textContent = name;
      strip.appendChild(chip);
    }
    wrap.appendChild(strip);
  }
  return wrap;
}

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
    scheduleQueueFlush(); // 回滚后队列非空且 idle：自动调度下次重试（否则只能等用户干预）
  }
}

function addBubble(role, text, opts) {
  opts = opts || {};
  if (role === "user") console.trace("[diag] addBubble(user) " + JSON.stringify(String(text)).slice(0, 60));
  let wrap;
  if (role === "user") {
    wrap = buildUserBubble(opts.prefix || "", text, opts.attachments);
  } else {
    wrap = document.createElement("div");
    wrap.className = "bubble " + role;
    const body = document.createElement("div");
    body.className = "bubble-text";
    body.textContent = text;
    wrap.appendChild(body);
  }
  list.insertBefore(wrap, bottomLoader);
  scrollBottom();
  return wrap;
}

// 朗读按钮：点击时实时读取该气泡正文（后端已剥离 <think>）。
// .actions 复用（attachTokenBadge 可能先建放 token 角标），保证 token 与朗读同一行。
function attachSpeak(wrap) {
  let actions = wrap.querySelector(".actions");
  if (!actions) {
    actions = document.createElement("div");
    actions.className = "actions";
    wrap.appendChild(actions);
  }

  const btn = document.createElement("button");
  btn.className = "speak-btn";
  btn.type = "button";
  btn.textContent = "朗读";
  let audio = null;

  btn.onclick = async () => {
    if (audio && !audio.paused) {
      audio.pause();
      btn.textContent = "朗读";
      return;
    }
    const text = wrap.querySelector(".bubble-text").textContent;
    if (!text.trim()) return;
    btn.disabled = true;
    btn.textContent = "合成中…";
    try {
      const res = await invoke("speak", { text });
      audio = new Audio(`data:audio/${res.format};base64,${res.audio_base64}`);
      btn.textContent = "停止";
      audio.play();
      audio.onended = () => (btn.textContent = "朗读");
    } catch (e) {
      alert("TTS 失败: " + e);
      btn.textContent = "朗读";
    } finally {
      btn.disabled = false;
    }
  };

  actions.appendChild(btn);
  return btn;
}

// token 用量角标：和朗读按钮同一行（.actions 内，token 在前、朗读在后）。
// live 经 chat-usage 事件、history 经最终 assistant event 的 data.usage，双路径共用。
function attachTokenBadge(wrap, usage) {
  if (!wrap || !usage) return;
  const p = usage.prompt_tokens ?? 0;
  const c = usage.completion_tokens ?? 0;
  if (!p && !c) return; // 无数据不显示
  let actions = wrap.querySelector(".actions");
  if (!actions) {
    actions = document.createElement("div");
    actions.className = "actions";
    wrap.appendChild(actions);
  }
  let badge = actions.querySelector(".bubble-token");
  if (!badge) {
    badge = document.createElement("span");
    badge.className = "bubble-token";
    actions.insertBefore(badge, actions.firstChild); // token 在朗读按钮前
  }
  // cached_tokens 在服务器 usage.prompt_tokens_details.cached_tokens（MiniMax OpenAI 兼容真实结构）；
  // 顶层 cached_tokens fallback 兼容旧扁平落盘数据。
  const cached = usage.prompt_tokens_details?.cached_tokens ?? usage.cached_tokens ?? 0;
  const parts = [`输入 ${p}`, `输出 ${c}`, `命中 ${cached}`];
  badge.textContent = parts.join(" · ");
}

// ===== 语音输入（麦克风一次性识别）=====
let micState = "idle"; // idle | recording | recognizing
let recorder = null;
let recTimer = null;
let recStartTs = 0;

function fmtSec(total) {
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `${m}:${s.toString().padStart(2, "0")}`;
}

function setMicState(state) {
  micState = state;
  micBtn.classList.toggle("recording", state === "recording");
  micBtn.classList.toggle("busy", state === "recognizing");
  micBtn.disabled = state === "recognizing";
  if (state !== "idle") sendBtn.disabled = true;
}

function insertRecognized(text) {
  const t = (text || "").trim();
  if (!t) return;
  const cur = input.value.replace(/\s+$/, "");
  input.value = cur ? cur + " " + t : t;
  input.dispatchEvent(new Event("input")); // 触发自适应高度
}

micBtn.onclick = async () => {
  if (micState === "idle") {
    try {
      recorder = await startRecorder();
    } catch (e) {
      alert("无法访问麦克风：" + e + "\n请检查系统麦克风权限。");
      return;
    }
    setMicState("recording");
    recStartTs = Date.now();
    recTimer = setInterval(() => {
      const sec = Math.floor((Date.now() - recStartTs) / 1000);
      input.placeholder = `正在聆听 ${fmtSec(sec)}，再点麦克风结束`;
    }, 250);
  } else if (micState === "recording") {
    if (recTimer) clearInterval(recTimer);
    recTimer = null;
    setMicState("recognizing");
    input.placeholder = "识别中…";
    let buf = null;
    try {
      buf = await recorder.stop();
    } catch (e) {
      /* ignore */
    }
    recorder = null;
    try {
      const text = await invoke("asr_one_shot", {
        bytes: Array.from(new Uint8Array(buf)),
      });
      insertRecognized(text);
    } catch (e) {
      alert("语音识别失败：" + e);
    } finally {
      input.placeholder = INPUT_PLACEHOLDER;
      setMicState("idle");
      sendBtn.disabled = false;
      input.focus();
    }
  }
};

// ===== 配置 =====
function fillForm(cfg) {
  F.apiKey.value = cfg.api_key || "";
  F.llmModel.value = cfg.llm_model || "MiniMax-M3";
  F.systemPrompt.value = cfg.system_prompt || "";
  F.userPrefix.value = cfg.user_prompt_prefix || "";
  F.userPrefixEnabled.checked = !!cfg.user_prompt_prefix_enabled;
  F.workspace.value = cfg.workspace_dir || "";
  F.cache.value = cfg.cache_dir || "";
  F.ttsModel.value = cfg.tts_model || "speech-2.8-hd";
  voicesState = (cfg.voices || []).map((v) => ({ id: v.id, label: v.label }));
  renderVoices(cfg.voice_id || "male-qn-qingse");
  F.speed.value = cfg.speed ?? 1;
  F.vol.value = cfg.vol ?? 1;
  F.pitch.value = cfg.pitch ?? 0;
  F.audioFormat.value = cfg.audio_format || "mp3";
  F.baiduAppid.value = cfg.baidu_app_id ?? 0;
  F.baiduApikey.value = cfg.baidu_api_key || "";
  F.baiduSecret.value = cfg.baidu_secret_key || "";
  F.baiduDevpid.value = cfg.baidu_dev_pid ?? 1537;
  F.holdGate.value = cfg.hold_gate_ms ?? 1000;
  F.voiceTrigger.value = cfg.voice_trigger || "hold";
  F.voiceAction.value = cfg.voice_action || "send";
  F.bgEnabled.checked = !!cfg.bg_enabled;
  F.bgPath.value = cfg.bg_path || "";
  const op = cfg.bg_opacity ?? 0.35;
  F.bgOpacity.value = op;
  F.bgOpacityVal.textContent = Number(op).toFixed(2);
  const gop = cfg.glass_opacity ?? 0.85;
  F.glassOpacity.value = gop;
  F.glassOpacityVal.textContent = Number(gop).toFixed(2);
  F.maxAttachmentMb.value = cfg.max_attachment_mb ?? 30;
  F.maxSubagents.value = cfg.max_subagents ?? 4;
  F.dreamIdleSecs.value = cfg.dream_idle_secs ?? 7200;
  F.dreamContextTriggerTokens.value = cfg.dream_context_trigger_tokens ?? 300000;
  F.dreamCapTurns.value = cfg.dream_cap_turns ?? 50;
  F.displayWindowSize.value = cfg.display_window_size ?? 50;
  F.maxToolIters.value = cfg.max_tool_iters ?? 100;
}

function readForm() {
  return {
    api_key: F.apiKey.value.trim(),
    llm_model: F.llmModel.value.trim() || "MiniMax-M3",
    system_prompt: F.systemPrompt.value,
    user_prompt_prefix: F.userPrefix.value,
    user_prompt_prefix_enabled: !!F.userPrefixEnabled.checked,
    workspace_dir: F.workspace.value.trim(),
    cache_dir: F.cache.value.trim(),
    tts_model: F.ttsModel.value,
    voice_id: F.voiceId.value || "male-qn-qingse",
    voices: voicesState,
    speed: Number(F.speed.value) || 1,
    vol: Number(F.vol.value) || 1,
    pitch: Number(F.pitch.value) || 0,
    audio_format: F.audioFormat.value,
    baidu_app_id: Number(F.baiduAppid.value) || 0,
    baidu_api_key: F.baiduApikey.value.trim(),
    baidu_secret_key: F.baiduSecret.value.trim(),
    baidu_dev_pid: Number(F.baiduDevpid.value) || 1537,
    hold_gate_ms: Number(F.holdGate.value) || 1000,
    voice_trigger: F.voiceTrigger.value || "hold",
    voice_action: F.voiceAction.value || "send",
    bg_enabled: !!F.bgEnabled.checked,
    bg_path: F.bgPath.value.trim(),
    bg_opacity: clamp01(Number(F.bgOpacity.value)),
    glass_opacity: clamp01(Number(F.glassOpacity.value)),
    max_attachment_mb: Number(F.maxAttachmentMb.value) || 30,
    max_subagents: F.maxSubagents.value === "" ? 4 : Number(F.maxSubagents.value),
    dream_idle_secs: F.dreamIdleSecs.value === "" ? 7200 : Number(F.dreamIdleSecs.value),
    dream_context_trigger_tokens: F.dreamContextTriggerTokens.value === "" ? 300000 : Number(F.dreamContextTriggerTokens.value),
    dream_cap_turns: F.dreamCapTurns.value === "" ? 50 : Number(F.dreamCapTurns.value),
    display_window_size: F.displayWindowSize.value === "" ? 50 : Number(F.displayWindowSize.value),
    max_tool_iters: F.maxToolIters.value === "" ? 100 : Number(F.maxToolIters.value),
    // 非表单字段：回传上次加载值，避免 save_config 把它们清成默认（media_roots / minimax_region）
    media_roots: (lastConfig && lastConfig.media_roots) || [],
    minimax_region: (lastConfig && lastConfig.minimax_region) || "cn",
  };
}

function clamp01(n) { n = Number(n); if (!isFinite(n)) return 0; return Math.min(1, Math.max(0, n)); }

async function loadConfig() {
  const cfg = await invoke("get_config");
  fillForm(cfg);
  subtitle.textContent = `${cfg.llm_model || "MiniMax-M3"} · ${cfg.tts_model || "speech-2.8-hd"}`;
  holdGateMs = cfg.hold_gate_ms ?? 1000;
  voiceTrigger = cfg.voice_trigger || "hold";
  voiceAction = cfg.voice_action || "send";
  lastConfig = cfg;
  return cfg;
}

// 应用全局背景图：data URL 套到 #bg-layer，遮罩强度注入 --bg-dim。
// 未启用 / 读不到 → 清掉图（回退主题底色），但遮罩强度仍写入（无图时无副作用）。
async function applyBackground() {
  if (!bgLayer) return;
  try {
    const bg = await invoke("get_background");
    if (bg.url) {
      bgLayer.style.backgroundImage = `url("${bg.url}")`;
    } else {
      bgLayer.style.backgroundImage = "none";
    }
    document.documentElement.style.setProperty("--bg-dim", clamp01(bg.opacity));
    document.documentElement.style.setProperty("--glass-alpha", clamp01(bg.glass));
  } catch (e) {
    console.warn("applyBackground 失败:", e);
  }
}

// ===== 视图导航 =====
function showSettings() {
  chatView.hidden = true;
  jobsView.hidden = true;
  const sv = document.getElementById("scheduler-view"); if (sv) sv.hidden = true;
  settingsView.hidden = false;
}
function showChat() {
  settingsView.hidden = true;
  jobsView.hidden = true;
  const sv = document.getElementById("scheduler-view"); if (sv) sv.hidden = true;
  chatView.hidden = false;
  input.focus();
}
settingsBtn.onclick = () => {
  if (settingsView.hidden) {
    loadConfig();
    showSettings();
  } else {
    showChat();
  }
};
settingsBack.onclick = showChat;

function hideAllViews() {
  chatView.hidden = true;
  settingsView.hidden = true;
  if (jobsView) jobsView.hidden = true;
  const sv = document.getElementById("scheduler-view"); if (sv) sv.hidden = true;
  if (tasksView) tasksView.hidden = true;
}

function showTasks() {
  hideAllViews();
  if (tasksView) tasksView.hidden = false;
}

// ===== Jobs 面板（完整任务管理界面）=====
const jobsView = document.getElementById("jobs-view");
const jobsBtn = document.getElementById("jobs-btn");
const jobsBack = document.getElementById("jobs-back");
const jobsRefresh = document.getElementById("jobs-refresh");
const jobsList = document.getElementById("jobs-list");
const jobsEmpty = document.getElementById("jobs-empty");
const jobsStats = document.getElementById("jobs-stats");
const jobsSub = document.getElementById("jobs-sub");

let jobsCache = [];
let jobsTimer = null;

function showJobs() {
  chatView.hidden = true;
  settingsView.hidden = true;
  const sv = document.getElementById("scheduler-view"); if (sv) sv.hidden = true;
  jobsView.hidden = false;
}
function hideJobs() { jobsView.hidden = true; }

function escapeHtml(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function fmtTime(ms) {
  if (!ms) return "—";
  const d = new Date(ms);
  const p = (n) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}
function fmtDur(ms) {
  if (ms == null || ms < 0 || !isFinite(ms)) return "—";
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

// JobStatus 经 serde(externally-tagged, snake_case)：unit 变体 Running/Killed → 字符串；
// struct 变体 Done/Failed → 对象。统一解析为 {key, code, reason}。
function jobState(s) {
  if (typeof s === "string") {
    if (s === "running" || s === "killed") return { key: s, code: null, reason: null };
    return { key: "?", code: null, reason: null };
  }
  if (s && s.done !== undefined) return { key: "done", code: s.done.code, reason: null };
  if (s && s.failed !== undefined) return { key: "failed", code: null, reason: s.failed.reason };
  if (s && s.killed !== undefined) return { key: "killed", code: null, reason: null };
  if (s && s.running !== undefined) return { key: "running", code: null, reason: null };
  return { key: "?", code: null, reason: null };
}
const STATE_LABEL = { running: "运行中", done: "完成", failed: "失败", killed: "已终止", "?": "未知" };

function renderJobsStats() {
  const c = { running: 0, done: 0, failed: 0, killed: 0 };
  for (const j of jobsCache) {
    const k = jobState(j.status).key;
    if (c[k] !== undefined) c[k] += 1;
  }
  const cards = [
    { cls: "st-running", label: "运行中", n: c.running },
    { cls: "st-done", label: "已完成", n: c.done },
    { cls: "st-failed", label: "失败", n: c.failed },
    { cls: "st-killed", label: "已终止", n: c.killed },
  ];
  jobsStats.innerHTML = cards.map((c) =>
    `<div class="stat ${c.cls}"><span class="stat-n">${c.n}</span><span class="stat-l">${c.label}</span></div>`
  ).join("");
  jobsSub.textContent = c.running > 0 ? `${c.running} 个任务进行中` : "管理 agent 触发的长任务";
}

function buildJobCard(j) {
  const st = jobState(j.status);
  const now = Date.now();
  const dur = st.key === "running"
    ? (now - (j.started_at || now))
    : ((j.finished_at || j.started_at || 0) - (j.started_at || 0));
  const meta = `开始 ${fmtTime(j.started_at)}` + (st.key === "running"
    ? ` · <span class="dur-live" data-start="${j.started_at || 0}">已运行 ${fmtDur(dur)}</span>`
    : (j.finished_at ? ` · 用时 ${fmtDur(dur)}` : ""));
  const reason = st.reason ? `<div class="job-reason">${escapeHtml(st.reason)}</div>` : "";

  const card = document.createElement("div");
  card.className = `job-card state-${st.key}`;
  card.dataset.id = j.id;

  // Agent-kind 渲染：子代理标题 + 流式容器 + 最终答案
  if (j.kind === "agent") {
    const title = j.label ? `（${escapeHtml(j.label)}）` : "";
    const chip = `<span class="job-chip"><i class="dot"></i>${STATE_LABEL[st.key]}</span>`;

    const head = document.createElement("div");
    head.className = "job-card-head";
    const idLabel = document.createElement("div");
    idLabel.className = "job-id-label";
    const idSpan = document.createElement("span");
    idSpan.className = "job-id";
    idSpan.textContent = `子代理 #${j.id}`;
    const labelSpan = document.createElement("span");
    labelSpan.className = "job-label";
    labelSpan.textContent = title;
    idLabel.appendChild(idSpan);
    idLabel.appendChild(labelSpan);
    head.appendChild(idLabel);
    head.innerHTML += chip; // 安全：chip 常量
    card.appendChild(head);

    const metaDiv = document.createElement("div");
    metaDiv.className = "job-meta";
    metaDiv.innerHTML = meta; // 安全：meta 已通过 escapeHtml 处理动态内容
    card.appendChild(metaDiv);

    if (reason) {
      const reasonDiv = document.createElement("div");
      reasonDiv.className = "job-reason";
      reasonDiv.innerHTML = reason; // 安全：reason 已通过 escapeHtml 处理
      card.appendChild(reasonDiv);
    }

    // 流式容器：实时追加 subagent-stream 事件内容
    const streamBox = document.createElement("div");
    streamBox.className = "subagent-stream";
    streamBox.dataset.id = String(j.id);
    streamBox.style.maxHeight = "200px";
    streamBox.style.overflow = "auto";
    streamBox.style.marginTop = "8px";
    card.appendChild(streamBox);

    // 终态显示答案
    if (st.key !== "running" && j.answer != null) {
      const ansDiv = document.createElement("div");
      ansDiv.className = "subagent-answer";
      ansDiv.style.marginTop = "8px";
      ansDiv.style.padding = "8px";
      ansDiv.style.background = "rgba(255,255,255,0.1)";
      ansDiv.style.borderRadius = "4px";
      ansDiv.textContent = "最终：" + j.answer;
      card.appendChild(ansDiv);
    }

    // 操作按钮
    const actions = document.createElement("div");
    actions.className = "job-card-actions";
    if (st.key === "running") {
      const killBtn = document.createElement("button");
      killBtn.type = "button";
      killBtn.className = "job-btn danger";
      killBtn.textContent = "终止";
      killBtn.onclick = async () => {
        killBtn.disabled = true;
        killBtn.textContent = "终止中…";
        try { await invoke("kill_job", { id: j.id }); }
        catch (e) { alert(e); killBtn.disabled = false; killBtn.textContent = "终止"; }
      };
      actions.appendChild(killBtn);
    }
    card.appendChild(actions);
  } else {
    // Process-kind（原有渲染）
    card.innerHTML =
      `<div class="job-card-head">
         <div class="job-id-label">
           <span class="job-id">#${j.id}</span>
           <span class="job-label">${escapeHtml(j.label || "(未命名)")}</span>
         </div>
         <span class="job-chip"><i class="dot"></i>${STATE_LABEL[st.key]}${st.code != null ? " · " + st.code : ""}</span>
       </div>
       <div class="job-meta">${meta}</div>
       ${reason}
       <div class="job-card-actions">
         ${st.key === "running" ? `<button type="button" class="job-btn danger" data-k>终止</button>` : ""}
         <button type="button" class="job-btn" data-l>完整日志</button>
       </div>`;

    const killBtn = card.querySelector("[data-k]");
    if (killBtn) {
      killBtn.onclick = async () => {
        killBtn.disabled = true;
        killBtn.textContent = "终止中…";
        try { await invoke("kill_job", { id: j.id }); }
        catch (e) { alert(e); killBtn.disabled = false; killBtn.textContent = "终止"; }
      };
    }
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
  }

  return card;
}

function renderJobsList() {
  const sorted = [...jobsCache].sort((a, b) => (b.started_at || 0) - (a.started_at || 0));
  jobsList.innerHTML = "";
  for (const j of sorted) jobsList.appendChild(buildJobCard(j));
  jobsEmpty.hidden = sorted.length > 0;
}

// 运行中任务每秒刷新耗时（面板打开时）
function startJobsTimer() {
  stopJobsTimer();
  jobsTimer = setInterval(() => {
    const now = Date.now();
    jobsList.querySelectorAll(".dur-live").forEach((el) => {
      const start = Number(el.dataset.start);
      if (start) el.textContent = `已运行 ${fmtDur(now - start)}`;
    });
  }, 1000);
}
function stopJobsTimer() {
  if (jobsTimer) { clearInterval(jobsTimer); jobsTimer = null; }
}

// job-update 增量：更新缓存 + 统计；面板可见时重渲染列表。
function upsertJob(j) {
  if (!j) return;
  const i = jobsCache.findIndex((x) => x.id === j.id);
  if (i >= 0) jobsCache[i] = j; else jobsCache.push(j);
  renderJobsStats();
  if (!jobsView.hidden) renderJobsList();
}

async function refreshJobs() {
  try {
    const jobs = await invoke("list_jobs");
    jobsCache = Array.isArray(jobs) ? jobs : [];
  } catch (e) {
    jobsCache = [];
    jobsList.innerHTML = `<div class="jobs-error">读取任务失败: ${escapeHtml(String(e))}</div>`;
    jobsEmpty.hidden = true;
    renderJobsStats();
    return;
  }
  renderJobsStats();
  renderJobsList();
}

if (jobsBtn) {
  jobsBtn.onclick = async () => {
    if (jobsView.hidden) {
      showJobs();
      await refreshJobs();
      startJobsTimer();
    } else {
      stopJobsTimer();
      showChat();
    }
  };
}
if (jobsRefresh) jobsRefresh.onclick = () => refreshJobs();
if (jobsBack) jobsBack.onclick = () => { stopJobsTimer(); hideJobs(); showChat(); };

// ===== 任务看板（main work memory）=====
async function refreshTasks() {
  try {
    const archived = !!(tasksShowArchived && tasksShowArchived.checked);
    const tasks = await invoke("list_tasks", { archived });
    tasksCache = Array.isArray(tasks) ? tasks : [];
  } catch (e) {
    tasksCache = [];
    tasksBoard.innerHTML = `<div class="jobs-error">读取任务失败: ${escapeHtml(String(e))}</div>`;
    if (tasksEmpty) tasksEmpty.hidden = true;
    return;
  }
  renderTasksBoard();
}

const HORIZON_LABEL = { current: "当前", short: "短期", long: "长期", vision: "愿景" };

function renderTasksBoard() {
  // 清四列
  for (const h of ["current", "short", "long", "vision"]) {
    const list = document.getElementById(`tasks-${h}-list`);
    if (list) list.innerHTML = "";
    const cnt = document.querySelector(`[data-count="${h}"]`);
    if (cnt) cnt.textContent = "0";
  }
  // 分桶（按 horizon 分组；archived 任务 horizon 为空就归到对应列仍渲染）
  const byHorizon = { current: [], short: [], long: [], vision: [] };
  for (const t of tasksCache) {
    const h = byHorizon[t.horizon] ? t.horizon : "current";
    byHorizon[h].push(t);
  }
  let total = 0;
  for (const h of ["current", "short", "long", "vision"]) {
    const list = document.getElementById(`tasks-${h}-list`);
    if (!list) continue;
    byHorizon[h].sort((a, b) => (a.sort_index || 0) - (b.sort_index || 0) || a.id - b.id);
    for (const t of byHorizon[h]) list.appendChild(buildTaskCard(t));
    total += byHorizon[h].length;
    const cnt = document.querySelector(`[data-count="${h}"]`);
    if (cnt) cnt.textContent = String(byHorizon[h].length);
  }
  if (tasksEmpty) tasksEmpty.hidden = total > 0;
}

function buildTaskCard(t) {
  const card = document.createElement("div");
  card.className = `task-card state-${t.status || "todo"}`;
  card.dataset.id = t.id;
  card.dataset.horizon = t.horizon || "current";

  // 阻塞派生：active + 有未解决 blocker
  const blocked = t.status === "active"
    && Array.isArray(t.blockers) && t.blockers.some((b) => !b.resolved);
  if (blocked) card.classList.add("blocked");

  // 徽章
  const badges = [];
  if (blocked) badges.push(`<span class="task-badge blocked">⚠️阻塞</span>`);
  if (t.status === "done" && t.verified) badges.push(`<span class="task-badge verified">✅已验证</span>`);
  else if (t.status === "done" && !t.verified) badges.push(`<span class="task-badge unverified">⚠️待验证</span>`);
  if (t.due_date) badges.push(`<span class="task-badge due">⏰截止</span>`);
  if (t.archived_at != null) badges.push(`<span class="task-badge archived">🗄️归档</span>`);
  if (t.parent_id != null) badges.push(`<span class="task-badge">↳ #${t.parent_id}</span>`);

  // 进度条（acceptance 派生）
  let progressHtml = "";
  const acc = Array.isArray(t.acceptance) ? t.acceptance : [];
  if (acc.length > 0) {
    const done = acc.filter((c) => c.done).length;
    const pct = Math.round((done / acc.length) * 100);
    progressHtml = `<div class="task-progress"><div class="task-progress-fill" style="width:${pct}%"></div></div>`;
    if (badges.length === 0 || true) badges.push(`<span class="task-badge">${done}/${acc.length}</span>`);
  }

  card.innerHTML =
    `<div class="task-card-title">#${t.id} ${escapeHtml(t.title || "(未命名)")}</div>`
    + (t.goal ? `<div class="task-card-goal">${escapeHtml(t.goal)}</div>` : "")
    + (badges.length ? `<div class="task-card-badges">${badges.join("")}</div>` : "")
    + progressHtml
    + (t.last_progress ? `<div class="task-meta" style="margin-top:6px">📍 ${escapeHtml(t.last_progress)}</div>` : "");

  card.onclick = () => openTaskModal(t.id);
  return card;
}

// ===== Scheduler 面板（定时任务）=====
const schedView = document.getElementById("scheduler-view");
const schedBtn = document.getElementById("scheduler-btn");
const schedBack = document.getElementById("sched-back");
const schedRefresh = document.getElementById("sched-refresh");
const schedNewBtn = document.getElementById("sched-new");
const schedGlobal = document.getElementById("sched-global");
const schedList = document.getElementById("scheduler-list");
const schedEmpty = document.getElementById("scheduler-empty");
const schedSub = document.getElementById("sched-sub");
const schedForm = document.getElementById("sched-form");
const schedCancel = document.getElementById("sched-cancel");

let schedCache = { global_enabled: false, tasks: [] };

function showScheduler() {
  chatView.hidden = true; settingsView.hidden = true; jobsView.hidden = true;
  schedView.hidden = false;
}

function trigLabel(t) {
  if (t.mode === "interval" && t.interval) return `每 ${t.interval.value} ${t.interval.unit}`;
  if (t.mode === "schedule" && t.schedule) {
    const s = t.schedule;
    const rep = s.repeat === "daily" ? "每天"
      : s.repeat === "weekly" ? `每周[${(s.weekdays || []).join(",")}]`
      : s.repeat === "monthly" ? `每月${s.day}号`
      : s.repeat === "yearly" ? `每年${s.month}-${s.day}` : s.repeat;
    return `${rep} ${s.time}`;
  }
  return t.mode || "?";
}

function renderSchedulerList() {
  const tasks = schedCache.tasks || [];
  schedEmpty.hidden = tasks.length > 0;
  schedList.innerHTML = "";
  for (const t of tasks) {
    const card = document.createElement("div");
    card.className = "job-card";
    card.innerHTML = `
      <div class="job-card-head">
        <span class="job-id">${escapeHtml(t.task_id)}</span>
        <span class="job-label">${escapeHtml(t.name)}</span>
        <span class="job-chip">${t.enabled ? "✓启用" : "✗停用"}</span>
      </div>
      <div class="job-reason">${escapeHtml(trigLabel(t))} · ${escapeHtml(t.count)}</div>
      <div class="job-tail" style="white-space:pre-wrap;color:var(--muted,#999)">${escapeHtml((t.action || "").slice(0, 200))}</div>
      <div style="margin-top:6px;display:flex;gap:8px">
        <button class="link-btn" data-act="edit">编辑</button>
        <button class="link-btn" data-act="toggle">${t.enabled ? "停用" : "启用"}</button>
        <button class="link-btn" data-act="delete">删除</button>
      </div>`;
    card.querySelector('[data-act="edit"]').onclick = () => fillSchedForm(t);
    card.querySelector('[data-act="toggle"]').onclick = async () => {
      try { await invoke("upsert_timer", { task: { ...t, enabled: !t.enabled } }); await refreshSchedulers(); } catch (e) { alert("切换失败: " + e); }
    };
    card.querySelector('[data-act="delete"]').onclick = async () => {
      if (confirm(`删除定时任务「${t.name}」?`)) {
        try { await invoke("delete_timer", { taskId: t.task_id }); await refreshSchedulers(); } catch (e) { alert("删除失败: " + e); }
      }
    };
    schedList.appendChild(card);
  }
  schedSub.textContent = tasks.length
    ? `${tasks.length} 个任务(全局 ${schedCache.global_enabled ? "开" : "关"})`
    : "到点自动把提示词投递给 agent";
}

async function refreshSchedulers() {
  try {
    schedCache = await invoke("list_timers");
  } catch (e) {
    schedCache = { global_enabled: false, tasks: [] };
    schedList.innerHTML = `<div class="jobs-error">读取定时任务失败: ${escapeHtml(String(e))}</div>`;
    schedEmpty.hidden = true;
    return;
  }
  schedGlobal.checked = !!schedCache.global_enabled;
  renderSchedulerList();
}

function toggleSchedMode() {
  const mode = schedForm.elements["mode"].value;
  schedForm.querySelector("[data-iv]").hidden = mode !== "interval";
  schedForm.querySelector("[data-sc]").hidden = mode !== "schedule";
}

function fillSchedForm(t) {
  const f = schedForm.elements;
  f["task_id"].value = (t && t.task_id) || "";
  f["name"].value = (t && t.name) || "";
  f["mode"].value = (t && t.mode) || "interval";
  f["count"].value = (t && t.count) || "forever";
  f["action"].value = (t && t.action) || "";
  f["context_inject"].value = ((t && t.context_inject) || []).join("\n");
  f["enabled"].checked = t ? !!t.enabled : true;
  f["iv_value"].value = (t && t.interval && t.interval.value) ?? 5;
  f["iv_unit"].value = (t && t.interval && t.interval.unit) || "min";
  const s = (t && t.schedule) || {};
  f["sc_time"].value = s.time || "09:00";
  f["sc_repeat"].value = s.repeat || "daily";
  f["sc_day"].value = s.day ?? 1;
  f["sc_month"].value = s.month ?? 1;
  f["sc_weekdays"].value = (s.weekdays || []).join(",");
  toggleSchedMode();
  schedForm.hidden = false;
}

function readSchedForm() {
  const f = schedForm.elements;
  const mode = f["mode"].value;
  const task = {
    task_id: f["task_id"].value,
    name: f["name"].value.trim(),
    mode, count: f["count"].value,
    action: f["action"].value,
    context_inject: f["context_inject"].value.split("\n").map(s => s.trim()).filter(Boolean),
    enabled: f["enabled"].checked,
    interval: null, schedule: null, last_fired_ts: 0, last_fired_date: "",
  };
  if (mode === "interval") {
    task.interval = { value: Number(f["iv_value"].value) || 0, unit: f["iv_unit"].value };
  } else {
    task.schedule = {
      time: f["sc_time"].value.trim() || "09:00",
      repeat: f["sc_repeat"].value,
      day: Number(f["sc_day"].value) || 0,
      month: Number(f["sc_month"].value) || 0,
      weekdays: f["sc_weekdays"].value.split(",").map(s => Number(s.trim())).filter(n => !isNaN(n) && n >= 0 && n <= 6),
    };
  }
  return task;
}

if (schedBtn) {
  schedBtn.onclick = async () => {
    if (schedView.hidden) { showScheduler(); await refreshSchedulers(); }
    else { schedView.hidden = true; showChat(); }
  };
}
if (schedBack) schedBack.onclick = () => { schedView.hidden = true; showChat(); };
if (schedRefresh) schedRefresh.onclick = () => refreshSchedulers();
if (schedNewBtn) schedNewBtn.onclick = () => fillSchedForm(null);
if (schedCancel) schedCancel.onclick = () => { schedForm.hidden = true; };
if (schedForm) {
  schedForm.elements["mode"].onchange = toggleSchedMode;
  schedForm.addEventListener("submit", async (e) => {
    e.preventDefault();
    const task = readSchedForm();
    if (!task.name) { alert("名称必填"); return; }
    if (!task.action.trim()) { alert("提示词 action 必填"); return; }
    try {
      await invoke("upsert_timer", { task });
      schedForm.hidden = true;
      await refreshSchedulers();
    } catch (err) { alert("保存失败: " + err); }
  });
}
if (schedGlobal) {
  schedGlobal.onchange = async () => {
    try { await invoke("set_scheduler_global", { enabled: schedGlobal.checked }); await refreshSchedulers(); }
    catch (e) { alert("切换全局开关失败: " + e); }
  };
}

// ===== 任务看板按钮接线 =====
if (tasksBtn) {
  tasksBtn.onclick = async () => {
    if (tasksView.hidden) {
      showTasks();
      await refreshTasks();
    } else {
      showChat();
    }
  };
}
if (tasksBack) tasksBack.onclick = () => { showChat(); };
if (tasksRefresh) tasksRefresh.onclick = () => refreshTasks();
if (tasksShowArchived) tasksShowArchived.onchange = () => refreshTasks();
if (tasksNew) tasksNew.onclick = () => openTaskModal(null); // null=新建

configForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  if (!confirmDiscardDirtyEdits()) return;
  const cfg = readForm();
  const btn = document.getElementById("settings-save");
  btn.disabled = true;
  const old = btn.textContent;
  btn.textContent = "保存中…";
  try {
    await invoke("save_config", { cfg });
    // 不再 reset_session：driver 每轮从盘重读 config（见 agent.rs run_one），system_prompt /
    // prefix / dream_* 等下轮自然生效，无需清历史。旧版靠 reset 清 history 来"换 system_prompt"
    // 既无效（driver cfg Arc 不刷新）又毁用户历史。
    // 保存后立即刷新前端运行时配置镜像：lastConfig 是 currentPrefix()(live 气泡前缀拼接) 与
    // readForm 回传非表单字段的数据源。不更新 → 保存后 live 气泡仍拼旧前缀、需刷新页面才反映
    // 新值（渲染 bug 根因）。
    lastConfig = cfg;
    // 语音开关即时生效（不必重启 / 重开设置页）：Rust 侧每次按键沿自读 config，
    // 这里只刷新前端运行时变量（副标题措辞 + 识别后是否自动发送）。
    holdGateMs = cfg.hold_gate_ms ?? 1000;
    voiceTrigger = cfg.voice_trigger || "hold";
    voiceAction = cfg.voice_action || "send";
    subtitle.textContent = `${cfg.llm_model} · ${cfg.tts_model}`;
    applyBackground(); // 背景图/遮罩即时生效
    showChat();
  } catch (err) {
    alert("保存失败: " + err);
  } finally {
    btn.disabled = false;
    btn.textContent = old;
  }
});

// ===== 输入框 =====
input.addEventListener("input", () => {
  input.style.height = "auto";
  input.style.height = Math.min(input.scrollHeight, 160) + "px";
});

input.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    form.requestSubmit();
  }
});

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
  input.value = "";
  input.style.height = "auto";
  chatBusy = true;
  sendBtn.disabled = true;
  interruptBtn.disabled = false;
  updateInterruptTitle();
  interruptBtn.hidden = false;
  addBubble("user", text, { prefix: currentPrefix(), attachments: atts });
  pendingAttachments = [];
  renderAttachmentBar();
  try {
    await invoke("chat", { text, attachments: atts.map(a => ({ staged_path: a.staged_path, kind: a.kind })) });
  } catch (err) {
    addBubble("assistant", "发送失败: " + err);
    chatBusy = false;
    sendBtn.disabled = false;
    interruptBtn.hidden = true;
  }
});

// ===== Agent 事件处理 =====
function appendReasoning(text) {
  if (!text || !activeAssistantWrap) return;
  const body = activeAssistantWrap.querySelector(".reasoning-body");
  if (!body) return;
  const r = activeAssistantWrap.querySelector(".bubble-reasoning");
  r.hidden = false;
  body.textContent += text;
}
function appendContent(text) {
  if (!activeAssistantWrap || !text) return;
  const t = activeAssistantWrap.querySelector(".bubble-text");
  if (!t) return;
  if (t.querySelector(".typing")) t.textContent = ""; // 首次内容清掉打字指示器
  t.classList.remove("md"); // 流式期间用原始文本（pre-wrap）
  t.textContent += text;
  scrollBottom();
}
function appendToolCard(name, args) {
  if (!activeAssistantWrap) return;
  const details = activeAssistantWrap.querySelector(".bubble-tools");
  const inner = activeAssistantWrap.querySelector(".tools-inner");
  if (!details || !inner) return;
  const card = document.createElement("div");
  card.className = "tool-card";
  card.dataset.name = name;
  card.innerHTML = `<div class="tool-head">${taskToolLabel(name) || `🔧 ${name}`}</div><pre class="tool-args"></pre><pre class="tool-result"></pre>`;
  card.querySelector(".tool-args").textContent = args;
  inner.appendChild(card);
  details.hidden = false; // 有工具调用才显工具区（默认折叠，点 summary 展开）
  const n = inner.querySelectorAll(".tool-card").length;
  details.querySelector(".tools-summary").textContent = `工具调用 · ${n}`;
  scrollBottom();
}
function fillToolResult(name, result) {
  if (!activeAssistantWrap) return;
  const cards = activeAssistantWrap.querySelectorAll(`.tool-card[data-name="${name}"]`);
  const card = cards[cards.length - 1]; // 多次同名取最后一个
  if (!card) return;
  card.querySelector(".tool-result").textContent = result;
  scrollBottom();
}

// 右区 drop：用户主动打开的文件 —— 新建一个带 .bubble-media 容器的用户侧气泡，
// 复用 renderMediaCard/renderDocCard/renderEditCard（契约不变），加「本地」标签与 agent 气泡区分（D1）。
function addFileBubble(staged) {
  const wrap = document.createElement("div");
  wrap.className = "bubble user file-bubble";
  const tag = document.createElement("div");
  tag.className = "local-tag"; tag.textContent = "本地";
  wrap.appendChild(tag);
  const media = document.createElement("div");
  media.className = "bubble-media";
  wrap.appendChild(media);
  list.insertBefore(wrap, bottomLoader);
  scrollBottom();
  return wrap;
}

// 按 staged.kind 把卡片渲染进给定 wrap（复用既有渲染函数）。
function renderStagedInto(wrap, staged) {
  const kind = staged.kind;
  const j = { display: true, path: staged.staged_path, kind, caption: staged.original_name };
  if (kind === "image" || kind === "video" || kind === "audio") {
    renderMediaCard(wrap, j);
  } else if (kind === "markdown" || kind === "text") {
    // 文本/markdown：可编辑卡（编辑存到 staged 副本，D2 非破坏性）
    const content = staged.text != null ? staged.text : "";
    renderEditCard(wrap, { edit: true, path: staged.staged_path, kind, content, caption: staged.original_name });
  } else {
    renderDocCard(wrap, j);
  }
}

// display_media 媒体卡：玻璃底 + img/video/audio + 系统查看器打开链接
function renderMediaCard(wrap, j) {
  const tools = wrap.querySelector(".bubble-media");
  const card = document.createElement("div");
  card.className = "media-card";
  const url = convertFileSrc(j.path, "media");
  const filename = String(j.path).split(/[\\/]/).pop();
  addCardHead(card, filename);
  let media;
  if (j.kind === "image") {
    media = document.createElement("img");
    media.src = url; media.alt = j.caption || filename;
    media.loading = "lazy";
    media.addEventListener("click", () => openLightbox(url));
  } else if (j.kind === "video") {
    media = document.createElement("video");
    media.src = url; media.controls = true; media.preload = "metadata";
  } else { // audio
    media = document.createElement("audio");
    media.src = url; media.controls = true; media.preload = "metadata";
  }
  media.addEventListener("error", () => {
    const note = document.createElement("div");
    note.className = "media-error-note";
    note.textContent = "无法加载或解码该文件";
    card.insertBefore(note, media.nextSibling);
  });
  card.appendChild(media);
  if (j.caption) {
    const c = document.createElement("div");
    c.className = "media-caption"; c.textContent = j.caption;
    card.appendChild(c);
  }
  const open = document.createElement("button");
  open.type = "button"; open.className = "media-open link-btn";
  open.textContent = "在系统查看器打开";
  open.addEventListener("click", () => invoke("open_in_system", { path: j.path }).catch((e) => toast("无法用系统查看器打开：" + e, true)));
  card.appendChild(open);
  tools.appendChild(card);
  scrollBottom();
}

function appendDocError(tools, msg) {
  const err = document.createElement("div");
  err.className = "media-card media-error";
  err.textContent = msg || "展示失败";
  tools?.appendChild(err); scrollBottom();
}

// ===== 文档卡（display_doc）：html/pdf 沙箱 iframe / docx / csv / markdown =====
function labelFor(kind) {
  return ({ html: "HTML", pdf: "PDF", docx: "Word", csv: "CSV", markdown: "Markdown", text: "文本" })[kind] || "文档";
}
async function mediaExists(path) {
  try {
    const r = await fetch(convertFileSrc(path, "media"), { headers: { Range: "bytes=0-0" } });
    return r.ok; // 200/206 存在；404 不存在（media 处理器对缺失文件返 404）
  } catch { return false; }
}
async function fetchText(path) {
  const r = await fetch(convertFileSrc(path, "media"));
  if (!r.ok) throw new Error("文件不存在或不可读");
  return await r.text();
}
// HTML 文档卡：相对资源重写。media: 协议把绝对路径整段塞进 URL path（\ → %5C，
// 不是 URL 分隔符），iframe 内 <img src="x.png"> 按 URL 标准解析会丢掉全部目录信息
// → GET media.localhost/x.png → 服务端按相对路径找文件 → 404（图片/css 全挂）。
// 修：取 HTML 文本，把相对 src/href 重写为 convertFileSrc(基于 html 目录的绝对路径)，
// 经 srcdoc 渲染——资源 URL 直接指向 media://绝对路径，必命中 scope（html 本身在
// workspace/render 下，同目录资源天然同 root 内）。
function rewriteHtmlAssets(html, htmlPath) {
  const dir = String(htmlPath).replace(/[\\/][^\\/]+$/, ""); // html 所在目录
  const toAbs = (ref) => {
    // 只动协议/数据/锚点之外的相对引用（含 ./ ../）；正/反斜杠都认
    if (/^(?:[a-z]+:|\/\/|#|data:)/i.test(ref)) return null;
    // 统一反斜杠 + 剥当前目录段（./ 与中段 .\ 都清），再拼 html 所在目录
    const joined = ref.replace(/\//g, "\\").replace(/\\\.\//g, "").replace(/^\.\\/, "");
    const abs = dir + "\\" + joined;
    // ../ 逐级回退
    let out = abs;
    while (out.includes("\\..\\")) {
      out = out.replace(/\\[^\\]+\\\.\./, "");
    }
    return convertFileSrc(out, "media");
  };
  return html
    .replace(/(\ssrc\s*=\s*)(["'])([^"']+)\2/gi, (m, p, q, ref) => {
      const u = toAbs(ref); return u ? p + q + u + q : m;
    })
    .replace(/(\shref\s*=\s*)(["'])([^"']+)\2/gi, (m, p, q, ref) => {
      const u = toAbs(ref); return u ? p + q + u + q : m;
    })
    .replace(/url\(\s*(["']?)([^"')]+)\1\s*\)/gi, (m, q, ref) => {
      const u = toAbs(ref); return u ? `url(${q}${u}${q})` : m;
    });
}
function showDocError(body, msg) {
  body.innerHTML = "";
  const e = document.createElement("div");
  e.className = "media-error"; e.textContent = msg;
  body.appendChild(e);
}
// docx：fetch arrayBuffer → docx-preview 渲染进容器
async function renderDocx(body, path) {
  const r = await fetch(convertFileSrc(path, "media"));
  if (!r.ok) throw new Error("文件不存在或不可读");
  const buf = await r.arrayBuffer();
  const wrap = document.createElement("div");
  wrap.className = "docx-container";
  body.appendChild(wrap);
  await window.docx.renderAsync(buf, wrap, null, { className: "docx-body" });
}
// csv：fetch 文本 → PapaParse → <table>（首 1000 行 + 显示更多）
async function renderCsv(body, path) {
  const text = await fetchText(path);
  const parsed = window.Papa.parse(text, { skipEmptyLines: true });
  const rows = parsed.data || [];
  if (!rows.length) { showDocError(body, "CSV 为空"); return; }
  const region = document.createElement("div");
  region.className = "csv-wrap"; region.setAttribute("role", "region"); region.setAttribute("aria-label", "CSV 表格");
  const table = document.createElement("table"); table.className = "csv-table";
  const headRow = rows[0];
  const thead = document.createElement("thead"); const tr = document.createElement("tr");
  headRow.forEach((c) => { const th = document.createElement("th"); th.scope = "col"; th.textContent = c == null ? "" : String(c); tr.appendChild(th); });
  thead.appendChild(tr); table.appendChild(thead);
  const tbody = document.createElement("tbody");
  table.appendChild(tbody);
  const RENDER = 1000;
  let drawn = 1; // 跳过表头
  function draw(n) {
    const end = Math.min(drawn + n, rows.length);
    for (let i = drawn; i < end; i++) {
      const r = document.createElement("tr");
      (rows[i] || []).forEach((c) => { const td = document.createElement("td"); td.textContent = c == null ? "" : String(c); r.appendChild(td); });
      tbody.appendChild(r);
    }
    drawn = end;
  }
  draw(RENDER - 1);
  region.appendChild(table);
  body.appendChild(region);
  if (drawn < rows.length) {
    const more = document.createElement("button");
    more.type = "button"; more.className = "link-btn csv-more";
    more.textContent = `显示更多（共 ${rows.length} 行）`;
    more.addEventListener("click", () => {
      draw(RENDER);
      if (drawn >= rows.length) more.remove();
      else more.textContent = `显示更多（共 ${rows.length} 行，已显示 ${drawn}）`;
    });
    body.appendChild(more);
  }
}
// 文档卡全屏：clone .doc-body 进覆层（不搬原节点，避免 iframe 重载/docx 脱挂）
function openDocFullscreen(card) {
  const last = document.activeElement;
  const box = document.createElement("div");
  box.className = "media-lightbox doc-lightbox";
  box.setAttribute("role", "dialog"); box.setAttribute("aria-modal", "true");
  const inner = document.createElement("div"); inner.className = "doc-lightbox-inner";
  inner.appendChild(card.querySelector(".doc-body").cloneNode(true));
  box.appendChild(inner); box.tabIndex = -1;
  // 全屏铺满后没有遮罩边缘可点关闭 → 加一个 × 按钮（Esc 仍可关）
  const closeBtn = document.createElement("button");
  closeBtn.type = "button"; closeBtn.className = "doc-lightbox-close";
  closeBtn.setAttribute("aria-label", "关闭全屏"); closeBtn.textContent = "×";
  const close = () => { box.remove(); document.removeEventListener("keydown", onKey); last?.focus?.(); };
  closeBtn.addEventListener("click", close);
  const onKey = (e) => { if (e.key === "Escape") close(); };
  box.addEventListener("click", (e) => { if (e.target === box) close(); });
  document.addEventListener("keydown", onKey);
  box.appendChild(closeBtn);
  document.body.appendChild(box); box.focus();
}
// toast（保存反馈）
function toast(msg, isError) {
  const t = document.createElement("div");
  t.className = "toast" + (isError ? " toast-error" : "");
  t.textContent = msg;
  document.body.appendChild(t);
  requestAnimationFrame(() => t.classList.add("show"));
  setTimeout(() => { t.classList.remove("show"); setTimeout(() => t.remove(), 200); }, 2400);
}
// 未保存保护（轻量）：有 dirty 编辑卡时 reset 前确认
function confirmDiscardDirtyEdits() {
  const dirty = document.querySelectorAll('.edit-card[data-dirty="1"]');
  if (dirty.length && !confirm("有未保存的编辑改动，放弃？")) return false;
  return true;
}
// 卡片头：标题 + 最小化/展开切换（默认展开=最大化；点 − 折叠只留标题条，点 + 还原）
function addCardHead(card, title) {
  const head = document.createElement("div");
  head.className = "card-head";
  const t = document.createElement("span");
  t.className = "card-title"; t.textContent = title;
  const min = document.createElement("button");
  min.type = "button"; min.className = "card-min";
  min.setAttribute("aria-label", "最小化"); min.textContent = "−";
  min.addEventListener("click", () => {
    const collapsed = card.classList.toggle("collapsed");
    min.textContent = collapsed ? "+" : "−";
    min.setAttribute("aria-label", collapsed ? "展开" : "最小化");
  });
  head.appendChild(t); head.appendChild(min);
  card.insertBefore(head, card.firstChild);
}
// 文档卡公共：系统打开 + 全屏 按钮（复用既有 .doc-actions，避免与 appendEditButton 重复建条）
function appendDocActions(card, j) {
  let bar = card.querySelector(".doc-actions");
  if (!bar) { bar = document.createElement("div"); bar.className = "doc-actions"; card.appendChild(bar); }
  if (j.path) {
    const open = document.createElement("button");
    open.type = "button"; open.className = "link-btn";
    open.textContent = "在系统查看器打开";
    open.addEventListener("click", () => invoke("open_in_system", { path: j.path }).catch((e) => toast("无法用系统查看器打开：" + e, true)));
    bar.appendChild(open);
  }
  const fs = document.createElement("button");
  fs.type = "button"; fs.className = "link-btn";
  fs.textContent = "全屏";
  fs.addEventListener("click", () => openDocFullscreen(card));
  bar.appendChild(fs);
}
async function renderDocCard(wrap, j, scroll = true) {
  const tools = wrap.querySelector(".bubble-media");
  const card = document.createElement("div");
  card.className = "media-card doc-card";
  card.dataset.kind = j.kind;
  const filename = j.path ? String(j.path).split(/[\\/]/).pop() : (j.caption || "片段");
  const title = `${labelFor(j.kind)}：${filename}`;
  card.setAttribute("aria-label", title);
  addCardHead(card, title);
  const body = document.createElement("div"); body.className = "doc-body";
  card.appendChild(body);
  if (j.caption) { const c = document.createElement("div"); c.className = "media-caption"; c.textContent = j.caption; card.appendChild(c); }
  tools.appendChild(card); if (scroll) scrollBottom();
  try {
    if (j.kind === "html") {
      if (j.path && !await mediaExists(j.path)) throw new Error("文件不存在");
      const ifr = document.createElement("iframe");
      ifr.setAttribute("sandbox", "allow-scripts");
      ifr.setAttribute("referrerpolicy", "no-referrer");
      ifr.className = "doc-iframe"; ifr.title = title;
      if (j.path) {
        // 相对资源重写后走 srcdoc（iframe.src 的 URL base 解析会丢目录 → 图片 404，见 rewriteHtmlAssets）
        const raw = await fetchText(j.path);
        ifr.srcdoc = rewriteHtmlAssets(raw, j.path);
      } else {
        ifr.srcdoc = j.html || "";
      }
      body.appendChild(ifr);
    } else if (j.kind === "pdf") {
      // PDF：不加 sandbox——Chromium PDF 阅读器(PDFium)在 sandbox iframe 里不渲染。
      // PDF 由 PDFium 渲染（跨域 media.localhost + PDFium 自身沙箱，触不到 parent.__TAURI__）。
      if (!await mediaExists(j.path)) throw new Error("文件不存在");
      const ifr = document.createElement("iframe");
      ifr.className = "doc-iframe"; ifr.title = title;
      ifr.src = convertFileSrc(j.path, "media");
      body.appendChild(ifr);
    } else if (j.kind === "docx") {
      await renderDocx(body, j.path);
    } else if (j.kind === "csv") {
      await renderCsv(body, j.path);
    } else if (j.kind === "markdown") {
      const md = j.content != null ? j.content : await fetchText(j.path);
      const d = document.createElement("div"); d.className = "md-body md";
      d.innerHTML = renderMarkdown(md);
      body.appendChild(d);
      appendEditButton(card, { path: j.path, kind: "markdown", content: md, wrap });
    } else if (j.kind === "text") {
      // 纯文本/代码只读查看（编辑卡「查看」按钮切回；content 优先用传入=保留未保存编辑）
      const txt = j.content != null ? j.content : await fetchText(j.path);
      const pre = document.createElement("pre"); pre.className = "text-body"; pre.textContent = txt;
      body.appendChild(pre);
      appendEditButton(card, { path: j.path, kind: "text", content: txt, wrap });
    }
    card.classList.add("doc-ready");
  } catch (e) {
    showDocError(body, `加载失败：${(e && e.message) || e}`);
  }
  // iframe 高度同步 doc-body：定高兜底防空白，再用像素值覆盖以填满并随右下角拖拽同步
  const ifrEl = body.querySelector("iframe.doc-iframe");
  if (ifrEl) {
    const syncH = () => { ifrEl.style.height = body.clientHeight + "px"; };
    syncH();
    new ResizeObserver(syncH).observe(body);
  }
  appendDocActions(card, j);
}
// 编辑卡：左 textarea + 右实时预览 + 保存→write_file；dirty 守卫
function renderEditCard(wrap, j, scroll = true) {
  const tools = wrap.querySelector(".bubble-media");
  const card = document.createElement("div");
  card.className = "media-card edit-card";
  const fname = String(j.path).split(/[\\/]/).pop();
  card.setAttribute("aria-label", `编辑 ${fname}`);
  addCardHead(card, `编辑：${fname}`);
  const isMd = j.kind === "markdown";
  const split = document.createElement("div");
  split.className = "edit-split";
  const ta = document.createElement("textarea");
  ta.className = "edit-textarea"; ta.value = j.content || ""; ta.setAttribute("aria-label", "编辑内容");
  split.appendChild(ta);
  // P-2026-002：只有 markdown 需要右侧实时预览（双栏）；text/code 纯单栏——不建 preview DOM、不加按钮，
  // textarea 作 .edit-split(flex) 唯一子 + .edit-textarea{flex:1 1 0} → 自然撑满宽度。
  let prev = null;
  if (isMd) {
    prev = document.createElement("div");
    prev.className = "edit-preview md";   // .md → 与 .bubble-text.md / .md-body.md 对齐，共享 P-2026-001 紧凑排版
    split.appendChild(prev);
  }
  card.appendChild(split);
  const actions = document.createElement("div"); actions.className = "edit-actions";
  const view = document.createElement("button");
  view.type = "button"; view.className = "link-btn"; view.textContent = "查看";
  view.title = "切到只读查看（保留当前未保存内容）";
  view.addEventListener("click", () => {
    // 用当前 textarea 内容渲染查看卡（markdown→渲染视图，其余→只读文本），未保存编辑随之保留
    const kind = isMd ? "markdown" : "text";
    const content = ta.value;
    // 原位保留 + 高度保持：记锚点（防跳队尾）+ 记原高（新卡 minHeight ≥ 原高，切换不缩）
    const anchor = card.nextElementSibling;
    const oldH = card.offsetHeight;
    card.remove();
    renderDocCard(wrap, { display: true, kind, path: j.path, content }, false);
    const fresh = tools.lastElementChild;
    if (fresh) fresh.style.setProperty("--switch-min-h", oldH + "px");
    if (anchor && fresh && fresh !== anchor) tools.insertBefore(fresh, anchor);
  });
  const save = document.createElement("button");
  save.type = "button"; save.className = "primary-btn"; save.textContent = "保存";
  actions.appendChild(view);
  actions.appendChild(save);
  card.appendChild(actions);
  tools.appendChild(card); if (scroll) scrollBottom();
  let dirty = false; let timer = null;
  const refresh = () => {
    if (!prev) return;   // P-2026-002：text/code 无预览，跳过（prev 只 markdown 才建）
    prev.innerHTML = renderMarkdown(ta.value);
  };
  ta.addEventListener("input", () => {
    dirty = true; card.dataset.dirty = "1";
    clearTimeout(timer); timer = setTimeout(refresh, 200);
  });
  refresh();
  save.addEventListener("click", async () => {
    save.disabled = true;
    try {
      const msg = await invoke("write_file", { path: j.path, content: ta.value });
      dirty = false; delete card.dataset.dirty;
      toast(msg);
    } catch (e) { toast("保存失败：" + (e || ""), true); }
    finally { save.disabled = false; }
  });
}
// 查看卡（markdown）右上"编辑"按钮：切编辑态（同 wrap 内新建编辑卡，预填当前内容）
function appendEditButton(card, ctx) {
  const edit = document.createElement("button");
  edit.type = "button"; edit.className = "link-btn edit-btn";
  edit.textContent = "编辑";
  edit.addEventListener("click", async () => {
    // 重读盘拿最新内容（文件可能在别处被改过）；读失败降级用查看卡的快照
    let content = ctx.content;
    try { content = await fetchText(ctx.path); } catch { /* 文件被移走等，降级快照 */ }
    // 原位保留 + 高度保持（同 view click）
    const t = card.parentNode;   // .bubble-media（查看卡所在）
    const anchor = card.nextElementSibling;
    const oldH = card.offsetHeight;
    card.remove();
    renderEditCard(ctx.wrap || activeAssistantWrap, { edit: true, path: ctx.path, kind: ctx.kind, content }, false);
    const fresh = t && t.lastElementChild;
    if (fresh) fresh.style.setProperty("--switch-min-h", oldH + "px");
    if (anchor && fresh && fresh !== anchor) t.insertBefore(fresh, anchor);
  });
  const bar = card.querySelector(".doc-actions") || (() => { const b = document.createElement("div"); b.className = "doc-actions"; card.appendChild(b); return b; })();
  bar.appendChild(edit);
}

// 图片放大：全屏遮罩，点遮罩/Esc 关，焦点进出
let lightboxLast = null;
function openLightbox(src) {
  lightboxLast = document.activeElement;
  const box = document.createElement("div");
  box.className = "media-lightbox"; box.setAttribute("role", "dialog"); box.setAttribute("aria-modal", "true");
  const img = document.createElement("img");
  img.src = src; img.alt = "";
  box.appendChild(img);
  box.tabIndex = -1;
  const close = () => { box.remove(); document.removeEventListener("keydown", onKey); lightboxLast?.focus?.(); };
  const onKey = (e) => { if (e.key === "Escape") close(); };
  box.addEventListener("click", (e) => { if (e.target === box) close(); });
  img.addEventListener("click", close);
  document.addEventListener("keydown", onKey);
  document.body.appendChild(box);
  box.focus();
}

// ===== display 滑动窗口（§13.5）=====
// history 尾段的懒加载视图。DOM 至多挂 ~W*2 条（一头装一头卸）；[lo,hi] = 当前挂载的可见事件
// seq 区间；atBottom = 是否贴底（用于实时新事件 append 决策）。
// marker/agent:N 已被后端过滤（Task 11），前端无需 marker 分支。
let displayWindow = { lo: null, hi: null, atBottom: true };
let loadingUp = false;     // 上划加载中（重入守卫 + top loader 显隐）
let loadingDown = false;   // 下划加载中
let reachedBottom = true;  // 已加载到最新（阻止下划在底部反复空加载）
const SCROLL_PRELOAD_PX = 180; // 距顶/底 ≤ 此值即预加载（不等像素级撞边 → 灵敏跟手）
// 贴底判定阈值：distBottom ≤ 此值视为"贴底"。旧值 2px 太严——流式期异步布局竞态会让
// distBottom 短暂 >2（内容已 append、scrollTop 同帧没跟上）→ atBottom 假性 false → scrollBottom
// 停摆（128 行 early-return 反馈锁死）→ turn-end 时 1517 行把整个 live wrap（工具+最终文本）
// 误删。放宽到 32px（~1-2 行）容忍竞态，贴底时 atBottom 稳定为 true。#226 C1 候选。
const AT_BOTTOM_PX = 32;

// 顶/底加载指示器：absolute overlay，挂 list 内但不占流（不计入 scrollHeight/scrollTop）。
// 始终为 list 首尾子；内容 insertBefore 在它们之间，trim 仅移除带 data-seq 的历史气泡。
const topLoader = document.createElement("div");
topLoader.className = "display-loader top";
topLoader.textContent = "加载更早";
const bottomLoader = document.createElement("div");
bottomLoader.className = "display-loader bottom";
bottomLoader.textContent = "加载更新";

// F.displayWindowSize 由 Task 14 加入设置页（不存在时回退 50）；空串/非数/<=0 也回退。
const displayW = () => {
  const raw = F.displayWindowSize?.value;
  const n = parseInt((raw == null ? "50" : String(raw)) || "50", 10);
  return Number.isFinite(n) && n > 0 ? n : 50;
};

// 取 HistoryEvent 的 data 字段：HistoryEvent 在 Rust 端 #[serde(flatten)] data，
// 故 text/content/attachments 等字段在 JS 端是平铺在 ev 顶层的，不在 ev.data 下。
// 同时兼容 ev.data.xxx（防御历史/未来字段嵌套写法），取不到返回 undefined。
function evField(ev, key) {
  if (ev && typeof ev === "object") {
    if (Object.prototype.hasOwnProperty.call(ev, key)) return ev[key];
    if (ev.data && typeof ev.data === "object") return ev.data[key];
  }
  return undefined;
}

// 当前 config 应拼的前缀文本（对齐后端 maybe_snapshot_prefix 的启用判断：enabled && 非空）。
// 前端气泡用它做 live 显示的前缀来源（buildUserBubble 独立渲染前缀折叠区，不与原文混拼）；
// 后端发给 LLM 的拼接（<prefix>...</prefix> 包裹）在 context.rs::with_prefix，两端各管各的。
// loadConfig 前发送的极早期 → lastConfig=null → 返回 ""（buildUserBubble 不渲染前缀区），不会误拼。
function currentPrefix() {
  if (!lastConfig || !lastConfig.user_prompt_prefix_enabled) return "";
  const p = (lastConfig.user_prompt_prefix || "").trim();
  return p || "";
}

// 把一段 history 事件聚合成气泡：一个 user → 下一个 user 之前的 assistant+tool_result 聚合成
// 「一个」assistant 气泡（与 live chat-turn-start 创建的 wrap 结构一致），避免回放时把同 turn 的
// 工具调用与回答拆成多个气泡（live 一个 turn 一个气泡）。marker/agent:N 已被后端过滤。
// 返回 [{ node, seqStart, seqEnd }]：seqStart/seqEnd 为该气泡首/末事件 seq——一个气泡跨多事件，
// lo 取首、hi 取末，确保 history_tail beforeSeq=lo / history_head afterSeq=hi 不撞同 turn 事件重复加载。
// 历史重渲染：display/edit_card 的 tool_result 落成媒体/文档/编辑卡（复用 live 渲染器，落 .bubble-media）。
// 文件可能已不存在（重启后临时文件被清/用户删除）→ 渲染器内 fetch 检查 + 媒体 onerror 降级为异常态。
// 后台任务回调折叠卡：summary 带 job_id/类型，默认关（<details> 无 open），
// 点开 <pre> 看 body（传给 agent 的原文）。history（buildHistoryBubbles）+ live（job-callback listener）共用。
// T5c: job-callback 暂存（live 路径）。driver 在 chat-turn-start 之前 emit job-callback，
// 这里暂存，等 chat-turn-start 创建 assistant wrap 后注入气泡顶部（类似 bubble-tools 折叠区）。
let pendingCallback = null;

function buildCallbackCard(kind, jobId, body) {
  const det = document.createElement("details");
  det.className = "callback-card";
  const label = kind === "agent" ? `子代理 #${jobId}` : `后台任务 #${jobId}`;
  const sum = document.createElement("summary");
  sum.textContent = `↩ ${label} 回调`;
  det.appendChild(sum);
  const pre = document.createElement("pre");
  pre.textContent = body || "(无内容)";
  pre.style.cssText =
    "white-space:pre-wrap;word-break:break-all;margin:4px 0 0;padding:6px 8px;" +
    "background:rgba(0,0,0,0.2);border-radius:4px;font-size:12px;";
  det.appendChild(pre);
  return det;
}

function renderHistoryToolCard(wrap, name, resultStr) {
  const tools = wrap.querySelector(".bubble-media");
  let j;
  try { j = JSON.parse(resultStr || "{}"); } catch { appendDocError(tools, "展示结果解析失败"); return; }
  const ok = name === "edit_card" ? j.edit : j.display;
  if (!ok) { appendDocError(tools, j.error || "展示失败"); return; }
  // edit_card 历史重渲染默认走「查看卡」（只读 + 自带编辑按钮），不直接进编辑态；
  // 与 display 文档类同路径。live 路径（agent 刚调 edit_card）才直接 renderEditCard。
  if (j.kind === "image" || j.kind === "video" || j.kind === "audio") renderMediaCard(wrap, j);
  else renderDocCard(wrap, j, false); // async；文件缺失 → 内部 fetch 抛错 → showDocError
  // 历史 card 默认折叠（重启/滚动不一次性铺开所有大卡）；点 card-head 的 + 展开恢复。
  // 错误态（appendDocError / showDocError）不折叠——错误信息要可见。
  const card = tools.lastElementChild;
  if (card && card.classList.contains("media-card")) {
    card.classList.add("collapsed");
    const min = card.querySelector(".card-min");
    if (min) { min.textContent = "+"; min.setAttribute("aria-label", "展开"); }
  }
}

function buildHistoryBubbles(events) {
  _suppressScroll = true; // 渲染器自带 scrollBottom 会跳底，历史重建期间抑制
  const groups = [];
  let asst = null;
  let pendingCallback = null; // 本批暂存的回调卡（external/subagent_result），注入下个 assistant 气泡顶部 // 当前聚合中的 assistant 气泡描述；遇 user 类事件即关闭（下个 turn）

  const openAssistant = (seq) => {
    const node = document.createElement("div");
    node.className = "bubble assistant";
    node.innerHTML =
      '<details class="bubble-reasoning" hidden><summary>思考过程</summary><div class="reasoning-body"></div></details>'
      + '<details class="bubble-tools" hidden><summary class="tools-summary">工具调用</summary><div class="tools-inner"></div></details>'
      + '<div class="bubble-media"></div>'
      + '<div class="bubble-text"></div>';
    const g = { node, seqStart: seq, seqEnd: seq, textBuf: "", lastUsage: null };
    groups.push(g);
    return g;
  };
  const addToolCard = (g, name, args, result) => {
    const inner = g.node.querySelector(".tools-inner");
    const details = g.node.querySelector(".bubble-tools");
    const card = document.createElement("div");
    card.className = "tool-card";
    card.dataset.name = name || "?";
    const _label = taskToolLabel(name) || `🔧 ${name || "?"}`;
    card.innerHTML = args !== undefined
      ? `<div class="tool-head">${_label}</div><pre class="tool-args"></pre><pre class="tool-result"></pre>`
      : `<div class="tool-head">${_label}</div><pre class="tool-result"></pre>`;
    if (args !== undefined) card.querySelector(".tool-args").textContent = args;
    if (result !== undefined) card.querySelector(".tool-result").textContent = result;
    inner.appendChild(card);
    details.hidden = false;
    details.querySelector(".tools-summary").textContent = `工具调用 · ${inner.querySelectorAll(".tool-card").length}`;
  };

  for (const ev of events) {
    const k = ev.kind;
    if (k === "user") {
      asst = null;
      const atts = evField(ev, "attachments") || [];
      // 历史气泡拼上落盘时的 prefix 快照（与发给 LLM 的 content 一致）；无 prefix 字段则原样。
      const wrap = buildUserBubble(evField(ev, "prefix") || "", evField(ev, "text") || "", atts);
      // image attachment 额外渲染完整图卡（用户拖图 + 系统注入统一；让图对用户可见）
      const imgs = atts.filter((a) => a && a.kind === "image");
      if (imgs.length) {
        const media = document.createElement("div");
        media.className = "bubble-media";
        wrap.appendChild(media);
        for (const a of imgs) {
          renderMediaCard(wrap, { display: true, path: a.staged_path, kind: "image", caption: "" });
        }
      }
      groups.push({ node: wrap, seqStart: ev.seq, seqEnd: ev.seq });
    } else if (k === "subagent_result") {
      // 子代理回调：暂存，待下个 assistant 事件注入气泡顶部。asst=null 关闭聚合。
      asst = null;
      pendingCallback = { kind: "agent", jobId: evField(ev, "agent_id"), body: evField(ev, "summary") || "" };
    } else if (k === "external") {
      // external 两种：Process JobDone（[后台任务 #N 完成/失败]）暂存注入气泡顶部；ContextNote 不渲染。asst=null。
      asst = null;
      const what = evField(ev, "what") || "";
      const m = what.match(/^\[后台任务 #(\d+) (完成|失败)\]/);
      if (m) pendingCallback = { kind: "process", jobId: m[1], body: what };
    } else if (k === "assistant") {
      if (!asst) asst = openAssistant(ev.seq);
      if (pendingCallback) {
        const cb = buildCallbackCard(pendingCallback.kind, pendingCallback.jobId, pendingCallback.body);
        asst.node.insertBefore(cb, asst.node.firstChild);
        pendingCallback = null;
      }
      asst.seqEnd = ev.seq;
      const tcs = evField(ev, "tool_calls");
      if (Array.isArray(tcs) && tcs.length) {
        for (const tc of tcs) {
          const fn = (tc && tc.function) || {};
          const name = fn.name || "?";
          if (name === "display" || name === "edit_card") continue; // 这些落 .bubble-media 卡，不进工具调用折叠（与 live 一致）
          const args = typeof fn.arguments === "string" ? fn.arguments : JSON.stringify(fn.arguments || {});
          addToolCard(asst, name, args);
        }
      }
      const content = evField(ev, "content") || "";
      if (content.trim()) asst.textBuf += content;
      const u = evField(ev, "usage");
      if (u) asst.lastUsage = u; // 覆盖取最后（最终 text event 带 turn 聚合 usage）
    } else if (k === "tool_result") {
      if (!asst) asst = openAssistant(ev.seq); // 防御：孤儿 tool_result（无前置 assistant tool_calls）
      asst.seqEnd = ev.seq;
      const name = evField(ev, "name") || "?";
      const result = evField(ev, "result") || "";
      if (name === "display" || name === "edit_card") {
        // 历史重渲染：复用 live 卡渲染器落进 .bubble-media（与 live 一致）。
        // 文件可能已不存在（重启/清理）→ 渲染器内 fetch/onerror 降级为异常态，不崩。
        renderHistoryToolCard(asst.node, name, result);
      } else {
        const cards = asst.node.querySelectorAll(`.tool-card[data-name="${name}"]`);
        const card = cards[cards.length - 1];
        if (card) card.querySelector(".tool-result").textContent = result;
        else addToolCard(asst, name, undefined, result); // 无对应 tool_calls 前导：补一张只含 result 的卡
      }
    }
    // marker/edit：后端已过滤，跳过
  }

  // 收尾：assistant 气泡文本渲染 markdown + 朗读（与 chat-turn-end 收尾一致）
  for (const g of groups) {
    if (g.node.classList.contains("assistant")) {
      const t = g.node.querySelector(".bubble-text");
      const txt = (g.textBuf || "").trim();
      if (txt) {
        t.classList.add("md");
        t.innerHTML = renderMarkdown(txt);
        enhanceMarkdown(t);
        attachSpeak(g.node);
        attachTokenBadge(g.node, g.lastUsage);
      }
      // 无文本的 assistant 气泡:若有工具/媒体/回调内容,补占位说明(常为被中断的工具循环轮次),
      // 否则气泡只剩折叠的「工具调用 · N」、正文一片空白,Reload 看像渲染坏了。
      refreshToolOnlyPlaceholder(g.node);
    }
  }
  _suppressScroll = false;
  return groups;
}

// 评估/补 tool-only 占位:assistant 气泡无真实文本但含工具/媒体/回调时,在 .bubble-text 写一行
// 说明(仅工具调用 · 共 N 次 · 未生成文本回复 — 可能被中断)。有真实文本则清占位类。
// .tool-only-note 标记的文本在 mergeAssistantInto 合并时视作空(不污染跨批次文本拼接)。
function refreshToolOnlyPlaceholder(node) {
  const t = node.querySelector(".bubble-text");
  if (!t) return;
  const realText = t.classList.contains("tool-only-note") ? "" : t.textContent.trim();
  if (realText) { t.classList.remove("tool-only-note"); return; }
  const nTools = node.querySelectorAll(".tools-inner .tool-card").length;
  const hasMedia = node.querySelector(".bubble-media")?.children.length;
  const hasCb = node.querySelector(".callback-card");
  if (nTools || hasMedia || hasCb) {
    t.classList.remove("md");
    t.classList.add("tool-only-note");
    t.textContent = nTools > 1
      ? `（仅工具调用 · 共 ${nTools} 次，未生成文本回复 — 可能被中断）`
      : nTools === 1
        ? `（仅工具调用，未生成文本回复 — 可能被中断）`
        : `（未生成文本回复 — 可能被中断）`;
  }
}

// 跨滚动批次合并:相邻 assistant 气泡且 seq 连续(prev.seqEnd+1==next.seqStart)必属同一轮
// (buildHistoryBubbles 单批次内已把一轮聚成一气泡;跨批次相邻连续 == 批次边界把一轮切开了)。
// 把后一个并入前一个,工具/媒体/文本/思考/回调卡全合并,刷新 seqEnd 与占位。重复到稳定。
function coalesceAssistantBubbles() {
  let changed = true;
  while (changed) {
    changed = false;
    const all = [...list.querySelectorAll(".bubble.assistant[data-seq-start]")];
    for (let i = 0; i < all.length - 1; i++) {
      const a = all[i], b = all[i + 1];
      if (!a.isConnected || !b.isConnected) continue;
      if (Number(a.dataset.seqEnd) + 1 !== Number(b.dataset.seqStart)) continue;
      mergeAssistantInto(a, b);
      changed = true;
      break; // 合并改了 DOM,重新扫描(一轮被切成 K 段需 K-1 次)
    }
  }
}

// 把 assistant 气泡 b 并入 a(a 在上、seq 更小)。b.remove()。
function mergeAssistantInto(a, b) {
  // 工具卡:b 的全部 append 到 a,重算 summary 计数
  const aInner = a.querySelector(".tools-inner"), bInner = b.querySelector(".tools-inner");
  if (aInner && bInner) {
    while (bInner.firstChild) aInner.appendChild(bInner.firstChild);
    const n = aInner.querySelectorAll(".tool-card").length;
    const dt = a.querySelector(".bubble-tools");
    if (n) dt.hidden = false;
    const sum = a.querySelector(".tools-summary");
    if (sum) sum.textContent = `工具调用 · ${n}`;
  }
  // 媒体卡
  const aM = a.querySelector(".bubble-media"), bM = b.querySelector(".bubble-media");
  if (aM && bM) while (bM.firstChild) aM.appendChild(bM.firstChild);
  // 思考过程
  const aR = a.querySelector(".reasoning-body"), bR = b.querySelector(".reasoning-body");
  if (aR && bR && bR.textContent.trim()) aR.textContent += "\n" + bR.textContent;
  // 回调卡:b 顶部的 callback-card 移到 a 的工具区之前(保持「回调在气泡顶部」)
  const aTools = a.querySelector(".bubble-tools");
  b.querySelectorAll(".callback-card").forEach((cb) => a.insertBefore(cb, aTools));
  // 文本:占位文本视作空,合并真实文本后重渲染 markdown
  const aT = a.querySelector(".bubble-text"), bT = b.querySelector(".bubble-text");
  const aTxt = aT.classList.contains("tool-only-note") ? "" : aT.textContent.trim();
  const bTxt = bT.classList.contains("tool-only-note") ? "" : bT.textContent.trim();
  const combined = (aTxt + "\n\n" + bTxt).trim();
  aT.classList.remove("tool-only-note");
  if (combined) {
    aT.classList.add("md");
    aT.innerHTML = renderMarkdown(combined);
    enhanceMarkdown(aT);
    attachSpeak(a);
  } else {
    aT.classList.remove("md");
    aT.textContent = "";
  }
  a.dataset.seqEnd = b.dataset.seqEnd;
  b.remove();
  refreshToolOnlyPlaceholder(a);
}

// 给聚合气泡打 seqStart/seqEnd 标记（trim 游标 + 跳过 loader/live 气泡）。
const tagSeq = (g) => {
  g.node.dataset.seqStart = String(g.seqStart);
  g.node.dataset.seqEnd = String(g.seqEnd);
};

// 启动：渲染最近 W 条可见事件（history 尾段），聚合成气泡。空 history → list 仅留 loader。
async function loadDisplayInitial() {
  console.trace("[diag] loadDisplayInitial");
  const W = displayW();
  let tail = [];
  try { tail = await invoke("history_tail", { limit: W, beforeSeq: null }); }
  catch (e) { console.warn("[display] history_tail 失败", e); }
  if (!Array.isArray(tail)) tail = [];
  const groups = buildHistoryBubbles(tail);
  for (const g of groups) tagSeq(g);
  list.replaceChildren(topLoader, ...groups.map((g) => g.node), bottomLoader);
  if (groups.length) {
    displayWindow.lo = groups[0].seqStart;
    displayWindow.hi = groups[groups.length - 1].seqEnd;
  } else {
    displayWindow.lo = null;
    displayWindow.hi = null;
  }
  reachedBottom = true; // 初始即渲染最新尾段
  displayWindow.atBottom = true;
  await fillViewportIfNeeded();
  scrollBottom();
}

// 视口没填满时持续往上装 older，直到填满或 history 耗尽。
// 必要性：最近内容若是一个矮气泡（如工具被折叠的死循环轮次 ~40px，远小于视口 ~600px），
// 内容矮于视口 → 没有滚动条 → scroll 事件不触发 → 上划预加载永不启动 → 用户卡住翻不到老历史。
// 填满后 scrollBottom 回到底部（看最新），老内容在上方可上划。
async function fillViewportIfNeeded() {
  const W = displayW();
  loadingUp = true; // 占住：防 replaceChildren 抖动触发的 scroll 事件与 fill 抢着 prepend 同一批
  try {
    let guard = 0;
    while (displayWindow.lo != null
           && list.scrollHeight - list.clientHeight <= SCROLL_PRELOAD_PX
           && guard++ < 80) {
      let older = [];
      try { older = await invoke("history_tail", { limit: W, beforeSeq: displayWindow.lo }); }
      catch (e) { console.warn("[display] fill history_tail 失败", e); break; }
      if (!Array.isArray(older) || !older.length) break; // history 耗尽
      const gs = buildHistoryBubbles(older);
      for (const g of gs) tagSeq(g);
      for (let i = gs.length - 1; i >= 0; i--) list.insertBefore(gs[i].node, topLoader.nextSibling);
      displayWindow.lo = gs[0].seqStart;
      coalesceAssistantBubbles();
      if (older.length < W) break; // 这批不满 = 已到最老
    }
  } finally {
    loadingUp = false;
  }
}

// 装一头卸一头：保持「带 seqStart 的历史气泡」数 ≤ W*2。
// 只动带 data-seqStart 的节点——loader（无 seq）与 live 气泡（无 seq，尚未落 history）都不被 trim。
function trimOldestBubbles(W) {
  const kids = [...list.children].filter((n) => n.dataset.seqStart != null);
  let i = 0;
  while (kids.length - i > W * 2) { list.removeChild(kids[i]); i++; }
  if (i < kids.length) displayWindow.lo = Number(kids[i].dataset.seqStart);
}
function trimNewestBubbles(W) {
  const kids = [...list.children].filter((n) => n.dataset.seqStart != null);
  let j = kids.length;
  while (j > W * 2) { j--; list.removeChild(kids[j]); }
  if (j > 0) displayWindow.hi = Number(kids[j - 1].dataset.seqEnd);
}

function setupDisplayScroll() {
  list.addEventListener("scroll", async () => {
    const W = displayW();
    const distTop = list.scrollTop;
    const distBottom = list.scrollHeight - list.scrollTop - list.clientHeight;

    // 上划预加载：距顶 ≤ 阈值且有更老。prepend 后 scrollTop += 新增高度（scroll anchoring：
    // 原视口内容视觉位置不动 → 无抖动）。
    if (!loadingUp && !loadingDown && distTop <= SCROLL_PRELOAD_PX && displayWindow.lo != null) {
      loadingUp = true;
      topLoader.classList.add("show");
      try {
        let older = [];
        try { older = await invoke("history_tail", { limit: W, beforeSeq: displayWindow.lo }); }
        catch (e) { console.warn("[display] history_tail(scroll up) 失败", e); }
        if (!Array.isArray(older)) older = [];
        if (older.length) {
          const groups = buildHistoryBubbles(older);
          for (const g of groups) tagSeq(g);
          const prevH = list.scrollHeight;
          // older 升序 → groups 升序；seq 小的在更上方，倒序 prepend
          for (let i = groups.length - 1; i >= 0; i--) list.insertBefore(groups[i].node, topLoader.nextSibling);
          displayWindow.lo = groups[0].seqStart;
          trimNewestBubbles(W);
          // 视口补偿：prepend 上推了 Δ高度，scrollTop 同步下移 Δ高度 → 原内容不动
          list.scrollTop = list.scrollTop + (list.scrollHeight - prevH);
          // 合并跨批次切开的同一轮(older 末气泡若与本就有的首气泡 seq 连续 → 同一轮被批次切开)
          coalesceAssistantBubbles();
          displayWindow.atBottom = false;
          reachedBottom = false; // 上划离开最新区
        }
      } finally {
        topLoader.classList.remove("show");
        loadingUp = false;
      }
      return;
    }

    // 下划预加载：距底 ≤ 阈值、未到最新、非回答中。append 后 scrollTop 保持不动（scroll anchoring：
    // 新内容在视口下方，原内容位置不变 → 无抖动；用户继续下划渐进入新内容，配合阈值持续加载）。
    if (!loadingUp && !loadingDown && distBottom <= SCROLL_PRELOAD_PX
        && !reachedBottom && displayWindow.hi != null && !chatBusy) {
      loadingDown = true;
      bottomLoader.classList.add("show");
      try {
        let newer = [];
        try { newer = await invoke("history_head", { limit: W, afterSeq: displayWindow.hi }); }
        catch (e) { console.warn("[display] history_head(scroll down) 失败", e); }
        if (!Array.isArray(newer)) newer = [];
        if (newer.length) {
          const groups = buildHistoryBubbles(newer);
          for (const g of groups) tagSeq(g);
          for (const g of groups) list.insertBefore(g.node, bottomLoader);
          // 合并跨批次切开的同一轮(既有末气泡若与 newer 首气泡 seq 连续 → 同一轮被批次切开)
          coalesceAssistantBubbles();
          displayWindow.hi = groups[groups.length - 1].seqEnd;
          trimOldestBubbles(W);
          if (newer.length < W) reachedBottom = true; // 这批不满 = 已到最新
          // 不动 scrollTop：scroll anchoring（视口保持，新内容在下方渐进入）
        } else {
          reachedBottom = true; // 无更新 = 已到最新
        }
      } finally {
        bottomLoader.classList.remove("show");
        loadingDown = false;
      }
      return;
    }

    // 非加载帧：按视口实际位置刷新 atBottom（live stream 据此决定是否贴底跟随）。
    displayWindow.atBottom = distBottom <= AT_BOTTOM_PX;
  });
}

async function setupAgentEvents() {
  await listen("llm-thinking", (e) => appendReasoning(typeof e.payload === "string" ? e.payload : (e.payload && e.payload.text) || ""));
  await listen("llm-tool-call", (e) => {
    const p = e.payload || {};
    if (p.name === "display" || p.name === "edit_card") {
      // 占位卡：玻璃底 + 文件名 + 准备中（result 到再换）
      const tools = activeAssistantWrap?.querySelector(".bubble-media");
      if (tools) {
        const ph = document.createElement("div");
        ph.className = "media-card media-loading";
        const fname = String(p.args || "").replace(/.*"path"\s*:\s*"([^"]*)".*/, "$1").split(/[\\/]/).pop();
        ph.textContent = `${p.name === "edit_card" ? "编辑中" : "准备中"}… ${fname || ""}`;
        tools.appendChild(ph); scrollBottom();
      }
      return;
    }
    appendToolCard(p.name || "?", p.args || "");
  });
  await listen("llm-tool-result", (e) => {
    const p = e.payload || {};
    if (p.name === "display" || p.name === "edit_card") {
      if (!activeAssistantWrap) return;
      const tools = activeAssistantWrap.querySelector(".bubble-media");
      const ph = tools?.querySelector(".media-loading");
      if (ph) ph.remove();
      let j; try { j = JSON.parse(p.result || "{}"); } catch { j = {}; }
      const ok = j.display !== undefined ? j.display : j.edit;
      if (p.name === "edit_card") {
        ok ? renderEditCard(activeAssistantWrap, j) : appendDocError(tools, j.error);
      } else { // display：统一工具，按 kind 落媒体卡 / 文档卡（类型由后端按扩展名判）
        if (!ok) appendDocError(tools, j.error);
        else if (j.kind === "image" || j.kind === "video" || j.kind === "audio") renderMediaCard(activeAssistantWrap, j);
        else renderDocCard(activeAssistantWrap, j);
      }
      return;
    }
    fillToolResult(p.name || "?", p.result || "");
  });
  await listen("llm-content", (e) => appendContent(typeof e.payload === "string" ? e.payload : (e.payload && e.payload.text) || ""));

  // chat-turn-start：新建 assistant 三段气泡（按 turn 边界定气泡，G1）
  await listen("chat-turn-start", () => {
    console.log("[diag] turn-start", { atBottom: displayWindow.atBottom, hadWrap: !!activeAssistantWrap, chatBusy });
    const wrap = document.createElement("div");
    wrap.className = "bubble assistant";
    wrap.innerHTML =
      '<details class="bubble-reasoning" hidden><summary>思考过程</summary><div class="reasoning-body"></div></details>'
      + '<details class="bubble-tools" hidden><summary class="tools-summary">工具调用</summary><div class="tools-inner"></div></details>'
      + '<div class="bubble-media"></div>'
      + '<div class="bubble-text"><span class="typing"><i></i><i></i><i></i></span></div>';
    list.insertBefore(wrap, bottomLoader);
    if (!displayWindow.atBottom) reachedBottom = false; // 贴底（atBottom）时保持 reachedBottom=true：新 turn 内容由 live wrap 直接渲染，下划分支不重载（否则用尚未更新的旧 hi 重载会把上一 turn 的 assistant 重复渲染，#226）；仅上划时设 false 让下划可加载到本轮
    // 与 chat-turn-end 一致加 atBottom 守卫：用户上划翻历史（atBottom=false）时不强拉视口，
    // 否则 scrollBottom 触发 scroll 事件命中下划加载分支，把本回合刚落 history 的事件重复装回（bug #2）。
    if (displayWindow.atBottom) scrollBottom();
    activeAssistantWrap = wrap;
    // T5c: 注入暂存的回调卡到气泡顶部（job-callback 在 chat-turn-start 之前 emit）
    if (pendingCallback) {
      const cb = buildCallbackCard(pendingCallback.kind, pendingCallback.job_id, pendingCallback.body);
      wrap.insertBefore(cb, wrap.firstChild);
      pendingCallback = null;
    }
  });

  // chat-usage：本轮 token 用量 → 附到正在渲染的 assistant 气泡（turn_end 前到达，wrap 还在）
  await listen("chat-usage", (e) => {
    if (activeAssistantWrap) attachTokenBadge(activeAssistantWrap, e.payload);
  });

  // chat-turn-end：收尾渲染 markdown + 朗读按钮 + 解锁 send
  await listen("chat-turn-end", () => {
    console.log("[diag] turn-end", { atBottom: displayWindow.atBottom, hadWrap: !!activeAssistantWrap, chatBusy });
    if (activeAssistantWrap) {
      const t = activeAssistantWrap.querySelector(".bubble-text");
      const txt = t.textContent.trim();
      if (txt) {
        t.classList.add("md");
        t.innerHTML = renderMarkdown(txt);
        enhanceMarkdown(t);
        attachSpeak(activeAssistantWrap);
      }
      // 用户上划翻历史（atBottom=false）时移除本 turn 的 live wrap：下划/重载会从 history 聚合重建
      // 该 turn（buildHistoryBubbles），留着会与历史聚合气泡同 turn 重复显示。
      if (!displayWindow.atBottom) activeAssistantWrap.remove();
      activeAssistantWrap = null;
    }
    chatBusy = false;
    sendBtn.disabled = false;
    interruptBtn.hidden = true;
    updateInterruptTitle(); // 恢复默认 title（队列此时已 flush 或为空）
    input.focus();
    scheduleQueueFlush(); // 队列非空时 200ms 后合并发送（边沿触发）
    // 仅当用户贴底（没错过 turn 产出）时同步 hi 到最新 + 跟随滚动 + 标记已到最新。
    // 上划翻历史（atBottom=false）时保留旧 hi 与 reachedBottom=false，让用户下划时
    // 能加载上划期间错过/被 trim 的事件（否则 hi 跳最新 → 下划"已最新"不加载）。
    if (displayWindow.atBottom) {
      invoke("history_tail", { limit: 1, beforeSeq: null }).then((tail) => {
        if (Array.isArray(tail) && tail.length) displayWindow.hi = tail[0].seq;
      }).catch(() => {});
      reachedBottom = true;
      scrollBottom();
    } else {
      reachedBottom = false;
    }
  });

  // chat-error：显错 + 解锁 send
  await listen("chat-error", (e) => {
    const msg = (e.payload && e.payload.message) || "出错";
    if (activeAssistantWrap) {
      activeAssistantWrap.querySelector(".bubble-text").textContent = "出错: " + msg;
      activeAssistantWrap.classList.add("error");
      activeAssistantWrap = null;
    } else {
      addBubble("assistant", "出错: " + msg);
    }
    chatBusy = false;
    sendBtn.disabled = false;
    interruptBtn.hidden = true;
    scheduleQueueFlush(); // 出错也是 idle 边沿：队列照发（下轮重试语义）
  });

  // chat-retry：LLM 瞬时重试（网络/限流/风控净化）轻提示——不落 history、不打断聊天流。
  await listen("chat-retry", (e) => {
    const p = e.payload || {};
    toast(`重试中（第${p.attempt || "?"}次）：${p.reason || ""}`, false);
  });

  // chat-reset：后端 reset_session 后触发。reset 只清 live context，history 永不删；
  // display 仍可翻老历史 → 清屏后从 history 尾段重新渲染（同 loadDisplayInitial）。
  await listen("chat-reset", () => {
    list.replaceChildren(topLoader, bottomLoader); // 清内容、保 loader 锚点
    pendingQueue = [];              // 排队气泡已随 DOM 清掉，队列同步清（消息未发过，直接丢）
    if (queueFlushTimer !== null) { clearTimeout(queueFlushTimer); queueFlushTimer = null; }
    displayWindow = { lo: null, hi: null, atBottom: true };
    reachedBottom = true;
    loadDisplayInitial().then(() => {
      addBubble("assistant", "（已重置）");
    }).catch((e) => console.warn("[display] reset 后重载失败", e));
  });

  // job-update：Jobs 面板增量更新
  await listen("job-update", (e) => upsertJob(e.payload));

  // job-callback：后台任务回调（T5c）。driver JobDone 时 emit（在 chat-turn-start 之前）。
  // 暂存到 pendingCallback，等 chat-turn-start 创建 assistant wrap 后注入气泡顶部。
  await listen("job-callback", (e) => {
    pendingCallback = e.payload || null;
  });

  // injected-attachment：系统注入图片附件（tool_attach image 后端 emit）
  await listen("injected-attachment", (e) => {
    const p = e.payload || {};
    const caption = p.caption || "";
    const text = caption
      ? `[系统：助手纳入图片：${caption}]`
      : "[系统：助手纳入图片]";
    const wrap = document.createElement("div");
    wrap.className = "bubble user";
    const body = document.createElement("div");
    body.className = "bubble-text";
    body.textContent = text;
    wrap.appendChild(body);
    if (p.staged_path) {
      const media = document.createElement("div");
      media.className = "bubble-media";
      wrap.appendChild(media);
      renderMediaCard(wrap, { display: true, path: p.staged_path, kind: "image", caption });
    }
    if (typeof bottomLoader !== "undefined" && list && bottomLoader) {
      list.insertBefore(wrap, bottomLoader);
    } else if (list) {
      list.appendChild(wrap);
    }
    if (typeof scrollBottom === "function") scrollBottom();
  });

  // subagent-stream：子代理任务流式事件（content/tool_call/tool_result/done）
  await listen("subagent-stream", (e) => {
    const p = e.payload || {};
    const box = document.querySelector(`.subagent-stream[data-id="${p.id}"]`);
    if (!box) return;
    const line = document.createElement("div");
    line.className = "sa-stream-line sa-" + (p.kind || "");
    if (p.kind === "content") line.textContent = p.text || "";
    else if (p.kind === "tool_call") line.textContent = `🔧 ${p.name || ""} ${p.brief || ""}`;
    else if (p.kind === "tool_result") line.textContent = `↳ ${p.brief || ""}`;
    else return; // "done" 等由 job-update 重新渲染处理，此处忽略
    box.appendChild(line);
    box.scrollTop = box.scrollHeight;
  });

  // 关闭确认：后端 CloseRequested 检测到 running job → emit confirm-close
  await listen("confirm-close", async (e) => {
    const jobs = (e.payload && e.payload.jobs) || [];
    if (!jobs.length) return;
    const list = jobs.map((j) => `  · #${j.id}（${j.kind === "agent" ? "子代理" : "后台任务"}）${j.label || ""}`).join("\n");
    const ok = confirm(`还有 ${jobs.length} 个任务在跑：\n${list}\n\n确定退出？退出后这些任务判失败（forced_exit）。`);
    if (ok) {
      try { await invoke("force_quit"); }
      catch (err) { alert("强制退出失败: " + err); }
    }
    // 取消则不调 force_quit——后端已 prevent_close，窗保持开
  });

  // 任务板有变（agent 或用户改）——若 tasks 视图打开则重查（payload 空，收到就重查）
  await listen("task-changed", () => {
    if (tasksView && !tasksView.hidden) refreshTasks();
  });

  // ask_user 快速问答面板：driver emit "await-user" → 弹 modal
  await listen("await-user", (e) => openAskUserModal(e.payload));
}

// 高亮光标所在拖放区（Over/Enter 实时；Leave/Drop 清除）
function highlightDropZone(x, y) {
  form.classList.remove("dropzone-attach");
  list.classList.remove("dropzone-render");
  if (x == null) return;
  const el = document.elementFromPoint(x, y);
  if (form.contains(el)) form.classList.add("dropzone-attach");
  else if (list.contains(el)) list.classList.add("dropzone-render");
}

// 消息区 drop：立刻本地渲染 + 静默上下文（不发送）
async function renderDropped(paths) {
  let items;
  try { items = await invoke("stage_attachments", { paths }); }
  catch (e) { toast("打开失败：" + e, true); return; }
  for (const it of items) {
    if (!it.ok) { toast((it.error || "打开失败") + (it.original_name ? `：${it.original_name}` : ""), true); continue; }
    const wrap = addFileBubble(it);
    renderStagedInto(wrap, it);
    invoke("append_context_note", { text: `[上下文] 用户将 ${it.staged_path}（${it.kind}）拖入渲染区查看/编辑` })
      .catch(() => {});
  }
}

function setupDropzones() {
  const dpr = window.devicePixelRatio || 1;
  listen("dnd-hover", ({ payload }) => {
    if (!payload) { highlightDropZone(null, null); return; }
    highlightDropZone(payload.x / dpr, payload.y / dpr);
  });
  listen("dnd-drop", ({ payload }) => {
    highlightDropZone(null, null);
    const el = document.elementFromPoint(payload.x / dpr, payload.y / dpr);
    const paths = payload.paths || [];
    if (!paths.length) return;
    if (form.contains(el)) stageFiles(paths);          // 附件：复用 Task 5 stageFiles
    else if (list.contains(el)) renderDropped(paths);  // 本地渲染 + 静默上下文
  });
  form.setAttribute("role", "group");
  form.setAttribute("aria-label", "输入框区：拖放文件作为附件发送");
  list.setAttribute("role", "region");
  list.setAttribute("aria-label", "消息区：拖放文件本地查看/编辑");

  // 空消息区提示
}

// 设置页额外交互：工作目录文件夹选择弹窗、自定义音色添加。
function setupSettingsExtras() {
  F.workspacePick.addEventListener("click", async () => {
    try {
      const opts = { directory: true };
      const cur = F.workspace.value.trim();
      if (cur) opts.defaultPath = cur; // 记住上次路径：从当前工作目录打开
      const picked = await invoke("plugin:dialog|open", { options: opts });
      if (picked) F.workspace.value = picked;
    } catch (e) {
      alert("选择文件夹失败: " + e);
    }
  });
  F.cachePick.addEventListener("click", async () => {
    try {
      const opts = { directory: true };
      const cur = F.cache.value.trim();
      if (cur) opts.defaultPath = cur;
      const picked = await invoke("plugin:dialog|open", { options: opts });
      if (picked) F.cache.value = picked;
    } catch (e) {
      alert("选择文件夹失败: " + e);
    }
  });
  // 背景图：文件选择器（图片过滤）/ 清除 / 透明度实时预览
  F.bgPick.addEventListener("click", async () => {
    try {
      const opts = {
        multiple: false,
        filters: [{ name: "图片", extensions: ["png", "jpg", "jpeg", "webp", "gif", "bmp", "svg"] }],
      };
      const cur = F.bgPath.value.trim();
      if (cur) opts.defaultPath = cur;
      const picked = await invoke("plugin:dialog|open", { options: opts });
      if (picked) F.bgPath.value = picked;
    } catch (e) {
      alert("选择图片失败: " + e);
    }
  });
  F.bgClear.addEventListener("click", () => { F.bgPath.value = ""; });
  F.bgOpacity.addEventListener("input", () => {
    const v = clamp01(F.bgOpacity.value);
    F.bgOpacityVal.textContent = v.toFixed(2);
    document.documentElement.style.setProperty("--bg-dim", v); // 拖动即时预览明暗
  });
  F.glassOpacity.addEventListener("input", () => {
    const v = clamp01(F.glassOpacity.value);
    F.glassOpacityVal.textContent = v.toFixed(2);
    document.documentElement.style.setProperty("--glass-alpha", v); // 拖动即时预览透感
  });

  F.addVoiceBtn.addEventListener("click", () => {
    const id = F.voiceIdCustom.value.trim();
    if (!id) { F.voiceIdCustom.focus(); return; }
    const label = F.voiceLabelCustom.value.trim() || id;
    if (!voicesState.some((v) => v.id === id)) voicesState.push({ id, label });
    F.voiceIdCustom.value = "";
    F.voiceLabelCustom.value = "";
    renderVoices(id);
  });
}

// ===== 全局推说话（双击 Ctrl 按住录音 → 松开自动发送）=====
// 由 Rust 侧手势状态机驱动：emit voice-state(arming/recording/recognizing/idle)
// 与 voice-result(文本) / voice-error。前端只负责反馈与自动发送。
let baseSubtitle = "";
let holdGateMs = 1000;
let voiceTrigger = "hold"; // 仅影响录音中副标题措辞（Rust 侧自己读 config 决定手势）
let voiceAction = "send"; // 识别后 send=立即发送 / insert=只填入输入框

async function setupVoiceEvents() {
  await listen("voice-state", (e) => {
    const phase = e.payload && e.payload.phase;
    if (phase === "arming") {
      const secs = holdGateMs / 1000;
      subtitle.textContent = `已检测到双击 Ctrl，继续按住约 ${secs} 秒后开始录音…`;
    } else if (phase === "recording") {
      micBtn.classList.add("recording");
      micBtn.classList.remove("busy");
      subtitle.textContent = voiceTrigger === "toggle"
        ? "● 正在录音（再按一次 Ctrl 停止）"
        : "● 正在录音（松开 Ctrl 结束）";
    } else if (phase === "recognizing") {
      micBtn.classList.remove("recording");
      micBtn.classList.add("busy");
      subtitle.textContent = "识别中…";
    } else if (phase === "idle") {
      micBtn.classList.remove("recording", "busy");
      subtitle.textContent = baseSubtitle;
    }
  });

  await listen("voice-result", (e) => {
    const text = ((e.payload && e.payload.text) || "").trim();
    micBtn.classList.remove("recording", "busy");
    subtitle.textContent = baseSubtitle;
    if (!text) return;
    if (chatBusy) return; // chat 进行中，丢弃本次语音结果避免重入
    input.value = text;
    input.dispatchEvent(new Event("input")); // 自适应高度
    input.focus();
    if (voiceAction !== "insert") form.requestSubmit(); // send=自动发送 / insert=只填框
  });

  await listen("voice-error", (e) => {
    micBtn.classList.remove("recording", "busy");
    subtitle.textContent = baseSubtitle;
    alert("语音识别失败：" + e.payload);
  });
}

// ===== 初始化 =====
loadConfig().then(async () => {
  baseSubtitle = subtitle.textContent;
  setupVoiceEvents();
  setupAgentEvents();
  setupDropzones();
  setupSettingsExtras();
  applyBackground();
  bindLinkOpener(list); // 消息内 http(s) 链接 → 系统浏览器打开
  // display 滑动窗口：滚动加载 + 启动渲染最近 W 条可见事件（§13.5）。
  // 在 loadDisplayInitial 之前绑 scroll，确保翻页请求就绪；loadingUp/loadingDown 守卫 +
  // atBottom=true 默认值保证初始 scrollBottom 触发的 scroll 事件不会误装新事件。
  setupDisplayScroll();
  await loadDisplayInitial();
  refreshJobs(); // T5c: Reload 后 Jobs 面板也拉数据（不必点按钮）
  // 历史 non-empty 时已被 history_tail(W) 渲染覆盖；空历史（新 workspace）才放欢迎语。
  // 注：list 始终含 topLoader/bottomLoader 两个锚点子，故用 hi==null 判空（而非 children.length）。
  if (displayWindow.hi == null) {
    addBubble(
      "assistant",
      "你好！我是 MiniMax-M3。打字、点麦克风，或「双击 Ctrl 并按住」全局说话——松开即自动发送。"
    );
  }
});

// ===== 任务编辑弹层 =====
function openTaskModal(id) {
  editingTaskId = id;
  const t = id != null ? tasksCache.find((x) => x.id === id) : null;
  document.getElementById("task-modal-title").textContent = t ? `编辑 #${t.id}` : "新建任务";
  document.getElementById("task-f-title").value = t ? (t.title || "") : "";
  document.getElementById("task-f-goal").value = t ? (t.goal || "") : "";
  document.getElementById("task-f-detail").value = t ? (t.detail || "") : "";
  document.getElementById("task-f-horizon").value = t ? (t.horizon || "current") : "current";
  document.getElementById("task-f-status").value = t ? (t.status || "todo") : "todo";
  document.getElementById("task-f-parent").value = t && t.parent_id != null ? t.parent_id : "";
  document.getElementById("task-f-due").value = t && t.due_date != null ? t.due_date : "";
  document.getElementById("task-f-tags").value = t && Array.isArray(t.tags) ? t.tags.join(",") : "";
  document.getElementById("task-f-acceptance").value = t && Array.isArray(t.acceptance)
    ? t.acceptance.map((c) => c.text).join("\n") : "";

  // 编辑模式显示归档/删除 + meta
  const archiveBtn = document.getElementById("task-modal-archive");
  const deleteBtn = document.getElementById("task-modal-delete");
  const meta = document.getElementById("task-f-meta");
  if (t) {
    archiveBtn.hidden = false; deleteBtn.hidden = false;
    archiveBtn.textContent = t.archived_at != null ? "恢复" : "归档";
    meta.hidden = false;
    meta.textContent = `创建 ${fmtTime(t.created_at)} · 更新 ${fmtTime(t.updated_at)}${t.completed_at ? " · 完成 " + fmtTime(t.completed_at) : ""}`;
  } else {
    archiveBtn.hidden = true; deleteBtn.hidden = true; meta.hidden = true;
  }

  if (taskModal) taskModal.hidden = false;
}

function closeTaskModal() {
  if (taskModal) taskModal.hidden = true;
  editingTaskId = null;
}

async function saveTaskModal() {
  const title = document.getElementById("task-f-title").value.trim();
  if (!title) { alert("标题不能为空"); return; }
  const horizon = document.getElementById("task-f-horizon").value;
  const status = document.getElementById("task-f-status").value;
  const parentRaw = document.getElementById("task-f-parent").value.trim();
  const dueRaw = document.getElementById("task-f-due").value.trim();
  const tags = document.getElementById("task-f-tags").value.split(",").map((s) => s.trim()).filter(Boolean);
  const accLines = document.getElementById("task-f-acceptance").value.split("\n").map((s) => s.trim()).filter(Boolean);

  try {
    if (editingTaskId != null) {
      // 编辑：update_task
      // 按 text 匹配 tasksCache 旧项，保留 agent 的 done/evidence；
      // 新行（改名/新增）默认 undone。仅改标题等不会擦进度。
      const existing = tasksCache.find((x) => x.id === editingTaskId);
      const oldBy = {};
      if (existing && Array.isArray(existing.acceptance)) {
        for (const c of existing.acceptance) {
          if (c && c.text) oldBy[c.text] = c;
        }
      }
      const acceptance = accLines.map((text) => ({
        text,
        done: oldBy[text] ? !!oldBy[text].done : false,
        evidence: oldBy[text] && oldBy[text].evidence ? oldBy[text].evidence : undefined,
      }));
      const patch = {
        title,
        goal: document.getElementById("task-f-goal").value,
        detail: document.getElementById("task-f-detail").value,
        horizon, status, tags,
        acceptance,
      };
      if (parentRaw) patch.parent_id = Number(parentRaw);
      if (dueRaw) patch.due_date = Number(dueRaw);
      await invoke("update_task", { id: editingTaskId, patch });
    } else {
      // 新建：add_task
      const input = {
        title,
        goal: document.getElementById("task-f-goal").value,
        detail: document.getElementById("task-f-detail").value,
        horizon, status, tags,
        acceptance: accLines.map((text) => ({ text, done: false })),
        created_by: "user",
      };
      if (parentRaw) input.parent_id = Number(parentRaw);
      if (dueRaw) input.due_date = Number(dueRaw);
      await invoke("add_task", { input });
    }
    closeTaskModal();
    await refreshTasks();
  } catch (e) {
    alert("保存失败: " + e);
  }
}

// 弹层按钮接线
if (document.getElementById("task-modal-close")) {
  document.getElementById("task-modal-close").onclick = closeTaskModal;
}
if (document.getElementById("task-modal-cancel")) {
  document.getElementById("task-modal-cancel").onclick = closeTaskModal;
}
if (document.getElementById("task-modal-save")) {
  document.getElementById("task-modal-save").onclick = saveTaskModal;
}
if (document.getElementById("task-modal-archive")) {
  document.getElementById("task-modal-archive").onclick = async () => {
    if (editingTaskId == null) return;
    const t = tasksCache.find((x) => x.id === editingTaskId);
    const willArchive = !(t && t.archived_at != null);
    if (!confirm(willArchive ? "归档此任务？" : "恢复此任务？")) return;
    try {
      await invoke("archive_task", { id: editingTaskId, archived: willArchive });
      closeTaskModal();
      await refreshTasks();
    } catch (e) { alert((willArchive ? "归档" : "恢复") + "失败: " + e); }
  };
}
if (document.getElementById("task-modal-delete")) {
  document.getElementById("task-modal-delete").onclick = async () => {
    if (editingTaskId == null) return;
    if (!confirm("硬删除此任务？此操作不可恢复（归档可恢复，硬删不行）。")) return;
    try {
      await invoke("delete_task", { id: editingTaskId });
      closeTaskModal();
      await refreshTasks();
    } catch (e) { alert("删除失败: " + e); }
  };
}

// ===== ask_user 快速问答面板 =====
//
// driver emit "await-user" → openAskUserModal(payload) → 渲染题目/选项/输入框/倒计时
// 用户点「确认提交」→ invoke("user_answered", { question_id, answer })
// 用户点「跳过」    → invoke("user_answered", { question_id, skipped: true })
// 倒计时归零（driver 端）→ driver 自动选推荐项或视为用户未答,不依赖前端行为

let askTimerHandle = null;
let askTimerRemaining = 0;
let askTimerIdleSince = 0;
let askCurrentQuestionId = null;
let askRequireConfirm = false;       // 默认一步式（点选项直接提交）
let askRecommendedLabel = null;
let askOptions = [];
let askDefaultSelectedIdx = -1;      // 推荐项 idx（默认高亮）,-1=无

function openAskUserModal(payload) {
  closeAskUserModal(); // 防御
  askCurrentQuestionId = payload.question_id;
  askRequireConfirm = payload.require_confirm === true; // 严格 true 才走两步（向后兼容）
  askOptions = Array.isArray(payload.options) ? payload.options : [];
  // 找推荐项 idx（用于默认高亮 + 超时兜底）
  const recIdx = askOptions.findIndex(o => o.recommended);
  askRecommendedLabel = recIdx >= 0 ? (askOptions[recIdx].label || null) : null;
  askDefaultSelectedIdx = recIdx; // -1 表示无推荐项

  const modal = document.getElementById("ask-user-modal");
  document.getElementById("ask-modal-question").textContent = payload.question || "";
  const ctxEl = document.getElementById("ask-modal-context");
  if (payload.context) { ctxEl.textContent = payload.context; ctxEl.hidden = false; }
  else { ctxEl.hidden = true; }
  document.getElementById("ask-modal-custom").hidden = true;
  const customInput = document.getElementById("ask-modal-custom-input");
  customInput.value = "";

  // 渲染选项
  const optsBox = document.getElementById("ask-modal-options");
  optsBox.innerHTML = "";
  askOptions.forEach((o, idx) => {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "ask-modal__opt";
    if (o.recommended) btn.classList.add("is-recommended");
    if (o.is_custom) btn.classList.add("is-custom");
    const labelEl = document.createElement("span");
    labelEl.className = "ask-modal__opt-label";
    labelEl.textContent = o.label || (o.is_custom ? "自定义" : "（无文字）");
    btn.appendChild(labelEl);
    if (o.description && !o.is_custom) {
      const descEl = document.createElement("span");
      descEl.className = "ask-modal__opt-desc";
      descEl.textContent = o.description;
      btn.appendChild(descEl);
    }
    btn.dataset.idx = String(idx);
    btn.dataset.label = o.label || "";
    // 推荐项默认高亮选中(视觉锚点)
    if (idx === askDefaultSelectedIdx) btn.classList.add("is-selected");
    btn.onmouseenter = onAskActivity;
    btn.onclick = () => onOptionClick(o);
    optsBox.appendChild(btn);
  });

  // 倒计时
  startAskTimer(payload.timeout_secs || 15, payload.pause_on_activity !== false, payload.resume_after_idle_secs || 5);

  // 显示 modal（用 display 切换,避开 [hidden] 属性陷阱;清掉可能残留的淡出动画类）
  modal.classList.remove("is-leaving");
  modal.style.display = "flex";
  modal.removeAttribute("hidden");

  // 自定义输入框（常驻）:活动暂停倒计时 + Enter 提交;allow_custom=false 时隐藏
  const customBox = document.getElementById("ask-modal-custom");
  if (payload.allow_custom === false) {
    customBox.hidden = true;
  } else {
    customBox.hidden = false;
    customInput.oninput = onAskActivity;
    customInput.onfocus = onAskActivity;
    customInput.onkeydown = (e) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        const v = customInput.value.trim();
        if (v) submitAskUser(v);
      }
    };
  }
}

function closeAskUserModal() {
  stopAskTimer();
  const modal = document.getElementById("ask-user-modal");
  if (!modal) return;
  // 干净利落淡出: 加 .is-leaving 类触发 CSS transition,150ms 后真隐藏。
  if (modal.classList.contains("is-leaving")) return;
  modal.classList.add("is-leaving");
  setTimeout(() => {
    // 重开保护:若 150ms 内 openAskUserModal 已开了新问题(防御性 close 会走到这),
    // 不能把新弹窗藏掉 — 只清动画类。
    if (askCurrentQuestionId) { modal.classList.remove("is-leaving"); return; }
    modal.style.display = "none";
    modal.setAttribute("hidden", "");
    modal.classList.remove("is-leaving");
  }, 150);
  askCurrentQuestionId = null;
}

function onOptionClick(opt) {
  if (!askCurrentQuestionId) return;
  onAskActivity();
  if (opt.is_custom) {
    // 自定义输入框已常驻,LLM 若仍发 is_custom 选项,点击只聚焦输入框
    document.getElementById("ask-modal-custom-input").focus();
    return;
  }
  // 一步式：点候选项直接提交 + 关 modal
  submitAskUser(opt.label);
}

// 倒计时：pause_on_activity=true 时活动触发暂停
function startAskTimer(timeoutSecs, pauseOnActivity, resumeAfterIdleSecs) {
  stopAskTimer();
  askTimerRemaining = Math.max(1, timeoutSecs);
  const timerEl = document.getElementById("ask-modal-timer");
  function tick() {
    if (!askCurrentQuestionId) return;
    if (pauseOnActivity && askTimerIdleSince > 0) {
      // 处于暂停状态,等 resume
      const idle = Date.now() - askTimerIdleSince;
      if (idle < resumeAfterIdleSecs * 1000) {
        timerEl.textContent = "暂停中…";
        timerEl.classList.add("is-paused");
        askTimerHandle = setTimeout(tick, 250);
        return;
      }
      // 到时,续倒
      askTimerIdleSince = 0;
      timerEl.classList.remove("is-paused");
    }
    askTimerRemaining--;
    if (askTimerRemaining <= 0) {
      timerEl.textContent = "已超时";
      timerEl.classList.add("is-paused");
      // 超时主动兜底：前端立即 invoke user_answered,driver 解冻 oneshot。
      // 优先级:输入框里已打未提交的文本 > 推荐项 > fallback。
      // 用户打了字哪怕没按 Enter,也是比推荐项更强的意图信号——不能被推荐项吞掉。
      const typed = (document.getElementById("ask-modal-custom-input").value || "").trim();
      const qid = askCurrentQuestionId;
      if (qid) {
        if (typed) {
          invoke("user_answered", {
            questionId: qid, answer: typed,
            skipped: false, autoSubmitted: false, fallbackNoRecommended: false,
          }).catch(e => console.error("[ask_user] timeout submit typed:", e));
        } else if (askRecommendedLabel) {
          invoke("user_answered", {
            questionId: qid, answer: askRecommendedLabel,
            skipped: false, autoSubmitted: true, fallbackNoRecommended: false,
          }).catch(e => console.error("[ask_user] timeout submit failed:", e));
        } else {
          invoke("user_answered", {
            questionId: qid, answer: null,
            skipped: false, autoSubmitted: false, fallbackNoRecommended: true,
          }).catch(e => console.error("[ask_user] timeout fallback failed:", e));
        }
      }
      stopAskTimer();
      // 干净利落:立即关 modal (淡出动画 150ms 由 CSS 处理),不等
      closeAskUserModal();
      return;
    }
    timerEl.textContent = askTimerRemaining + "s";
    askTimerHandle = setTimeout(tick, 1000);
  }
  timerEl.classList.remove("is-paused");
  timerEl.textContent = askTimerRemaining + "s";
  askTimerHandle = setTimeout(tick, 1000);
}
function stopAskTimer() {
  if (askTimerHandle) { clearTimeout(askTimerHandle); askTimerHandle = null; }
  askTimerIdleSince = 0;
}
function onAskActivity() {
  // 暂停倒计时（如果开启）
  askTimerIdleSince = Date.now();
}

// 提交 / 跳过
async function submitAskUser(answer) {
  if (!askCurrentQuestionId) return;
  const qid = askCurrentQuestionId;
  closeAskUserModal();
  try {
    await invoke("user_answered", { questionId: qid, answer: answer || null, skipped: false, autoSubmitted: false, fallbackNoRecommended: false });
  } catch (e) { console.error("[ask_user] submit failed:", e); }
}
async function skipAskUser() {
  if (!askCurrentQuestionId) return;
  const qid = askCurrentQuestionId;
  closeAskUserModal();
  try {
    await invoke("user_answered", { questionId: qid, answer: null, skipped: true, autoSubmitted: false, fallbackNoRecommended: false });
  } catch (e) { console.error("[ask_user] skip failed:", e); }
}

// 按钮接线（init 完后再绑）
document.addEventListener("DOMContentLoaded", () => {
  const skip = document.getElementById("ask-modal-skip");
  if (skip) skip.onclick = skipAskUser;
});
