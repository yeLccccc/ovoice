// 把 LLM 输出渲染成「安全的 markdown HTML」。
// 依赖三个全局（由 index.html 的 vendor <script> 注入，无打包器）：
//   markdownit  —— markdown-it 14，解析
//   DOMPurify   —— 输出进 innerHTML 前消毒（禁脚本/事件属性/javascript: URI）
//   hljs        —— highlight.js 11，代码块按语言染色
const { invoke } = window.__TAURI__.core;

const md = window.markdownit({
  html: false, // 不解析原始 HTML（额外防线，DOMPurify 还会再过一遍）
  breaks: true, // 单换行 → <br>，贴合聊天习惯
  linkify: true, // 裸 URL 自动成链接
  typographer: false,
  highlight(code, lang) {
    const hljs = window.hljs;
    if (!hljs) return ""; // 回退：markdown-it 自行转义并包 <pre><code>
    try {
      if (lang && hljs.getLanguage(lang)) {
        return hljs.highlight(code, { language: lang }).value;
      }
      return hljs.highlightAuto(code).value;
    } catch {
      return "";
    }
  },
});

// DOMPurify 默认白名单已足够安全（放行 markdown 常规标签 + img/表格/链接，
// 禁 script、事件属性、javascript: URI）。这里仅显式补上 hljs 的 class 与表格属性，
// 并禁掉与展示无关的 style/form 等元素。
const PURIFY_CFG = {
  ADD_ATTR: ["class", "target", "rel"],
  FORBID_TAGS: ["style", "form", "input", "button", "iframe", "object", "embed"],
  FORBID_ATTR: ["style", "srcset", "onerror", "onload"],
};

/// text → 安全 HTML 字符串（markdown + 高亮 + 消毒）。
export function renderMarkdown(text) {
  const dirty = md.render(text || "");
  return window.DOMPurify.sanitize(dirty, PURIFY_CFG);
}

/// 给一个已渲染的 markdown 容器追加交互：代码块「复制」按钮。
export function enhanceMarkdown(root) {
  if (!root) return;
  for (const pre of root.querySelectorAll("pre")) {
    if (pre.querySelector(".code-copy")) continue; // 避免重复
    const code = pre.querySelector("code");
    if (!code) continue;
    pre.classList.add("has-copy");
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "code-copy";
    btn.textContent = "复制";
    btn.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(code.textContent || "");
        btn.textContent = "已复制";
      } catch {
        btn.textContent = "失败";
      } finally {
        setTimeout(() => (btn.textContent = "复制"), 1200);
      }
    });
    pre.appendChild(btn);
  }
}

/// 在消息容器上做一次事件委托：点击 http(s) 链接 → 用系统浏览器打开。
export function bindLinkOpener(container) {
  container.addEventListener("click", (e) => {
    const a = e.target.closest && e.target.closest("a");
    if (!a) return;
    const href = a.getAttribute("href") || "";
    if (!href) return;
    e.preventDefault(); // 不在 WebView 内跳转
    if (/^https?:\/\//i.test(href)) {
      invoke("plugin:opener|open_url", { url: href }).catch(() => {
        /* 静默：opener 失败不阻塞阅读 */
      });
    }
  });
}
