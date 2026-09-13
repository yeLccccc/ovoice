# 聊天多模态渲染（图/视频/音频）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: 用 superpowers:subagent-driven-development（每任务一个全新 implementer + 任务间 review）实现。步骤用 `- [ ]` 复选框跟踪。

**Goal:** 在聊天对话内渲染 agent 产出的本地媒体（图片显示 + 点击放大、视频播放可拖进度、音频播放），走 `display_media` 工具 + `media://` 协议。

**Architecture:** agent 调 `display_media({path,kind?,caption?})` → 后端解析路径/校验/返结构化 JSON → 前端在 tool-call 管线特判渲染媒体卡（玻璃底 img/video/audio）。媒体文件经自定义 `media://` 协议喂给 webview，handler 自实现 Range/206（视频可拖进度）+ scope 限定 workspace/app_data。

**Tech Stack:** Tauri v2（`register_uri_scheme_protocol`）、Rust（`percent-encoding` crate 解码路径）、原生 `<img>/<video>/<audio>`、`plugin:opener`（系统打开）。无打包器，前端 vendor 脚本（markdown-it/DOMPurify/hljs）。

## Global Constraints

- 平台 Windows，Tauri v2，CSP=null（自定义 scheme 可用）。窗口 900×720，min 600×500。
- 复用既有视觉：`--glass-alpha`（透明度滑块）、`--radius-bubble`、炭黑文字变量（`--text`/`--text-secondary`/`--text-tertiary`）。
- 媒体卡视觉 = 玻璃（rgba via `--glass-alpha`），随透明度滑块变。
- 媒体卡位置 = `.bubble-tools`（正文上方），按 tool-call 顺序。
- 路径相对 workspace 解析（`resolve_path`）；绝对路径原样。
- 图片点击 → 应用内 lightbox（全屏遮罩，Esc/点遮罩关，焦点进出，aria-modal）。
- 视频**不自动播放**，`controls` + `preload="metadata"`，max-height 360px。
- 不新增 npm 依赖；后端新增 `percent-encoding` crate。
- TDD：后端纯函数单测；前端无 DOM 测试框架，手测（smoke 清单）。
- `src-tauri/.env` 不可提交（gitignored）。config.json 用户已改（speed:1.5 等）勿回退。

---

## File Structure

| 文件 | 职责 |
|---|---|
| `src-tauri/Cargo.toml` | 加 `percent-encoding` 依赖 |
| `src-tauri/src/lib.rs` | `MediaKind` + `media_kind_from_ext` + 扩 `mime_from_ext`；`is_within_roots`/`parse_range`/`read_range`；注册 `media://` 协议 handler |
| `src-tauri/src/tools.rs` | `tool_display_media` + `schemas` 加项 + `dispatch` 路由 |
| `src/main.js` | tool-call/result 特判 display_media → `renderMediaCard`；`openLightbox`；交互状态/打开链接；a11y |
| `src/styles.css` | `.media-card`（玻璃）+ img/video/audio 尺寸 + `.media-lightbox` + 错误态 |

依赖关系：Task 1（协议+helpers）+ Task 2（工具）独立可并行；Task 3 依赖 1+2；Task 4/5 依赖 3；Task 6 依赖 3/4。

---

## Task 1: `media://` 协议 + helpers + 测试

**Files:**
- Modify: `src-tauri/Cargo.toml`（`[dependencies]` 加 `percent-encoding = "2"`）
- Modify: `src-tauri/src/lib.rs`（helpers + 注册协议）

**Interfaces:**
- Produces（pub(crate)，供 tools.rs 与 handler 用）：
  - `enum MediaKind { Image, Video, Audio, Unsupported }`
  - `fn media_kind_from_ext(path: &Path) -> MediaKind`
  - `fn mime_from_ext(path: &Path) -> &'static str`（扩 video/audio）
  - `fn is_within_roots(path: &Path, roots: &[PathBuf]) -> bool`
  - `fn parse_range(header: &str, total: u64) -> Option<(u64, u64)>`（返 start, end_inclusive）

- [ ] **Step 1: 写失败测试（helpers）**

`src-tauri/src/lib.rs` 末尾 `#[cfg(test)] mod tests`（若无则新建）追加：

```rust
#[cfg(test)]
mod media_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn kind_image_video_audio_unsupported() {
        assert!(matches!(media_kind_from_ext(Path::new("a.png")), MediaKind::Image));
        assert!(matches!(media_kind_from_ext(Path::new("a.JPG")), MediaKind::Image));
        assert!(matches!(media_kind_from_ext(Path::new("a.mp4")), MediaKind::Video));
        assert!(matches!(media_kind_from_ext(Path::new("a.webm")), MediaKind::Video));
        assert!(matches!(media_kind_from_ext(Path::new("a.mp3")), MediaKind::Audio));
        assert!(matches!(media_kind_from_ext(Path::new("a.wav")), MediaKind::Audio));
        assert!(matches!(media_kind_from_ext(Path::new("a.txt")), MediaKind::Unsupported));
        assert!(matches!(media_kind_from_ext(Path::new("noext")), MediaKind::Unsupported));
    }

    #[test]
    fn mime_covers_media() {
        assert_eq!(mime_from_ext(Path::new("a.mp4")), "video/mp4");
        assert_eq!(mime_from_ext(Path::new("a.webm")), "video/webm");
        assert_eq!(mime_from_ext(Path::new("a.mp3")), "audio/mpeg");
        assert_eq!(mime_from_ext(Path::new("a.wav")), "audio/wav");
        assert_eq!(mime_from_ext(Path::new("a.flac")), "audio/flac");
    }

    #[test]
    fn is_within_roots_allows_inside_blocks_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let inside = root.join("sub/a.mp4"); std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(&inside, b"x").unwrap();
        let inside = inside.canonicalize().unwrap();
        assert!(is_within_roots(&inside, &[root.clone()]));
        // 绝对路径但不在 root 下
        let other = tempfile::tempdir().unwrap();
        let outside = other.path().canonicalize().unwrap();
        assert!(!is_within_roots(&outside, &[root]));
    }

    #[test]
    fn parse_range_forms() {
        assert_eq!(parse_range("bytes=0-1023", 2000), Some((0, 1023)));
        assert_eq!(parse_range("bytes=500-", 2000), Some((500, 1999)));   // 开放右端→到 total-1
        assert_eq!(parse_range("bytes=-500", 2000), Some((1500, 1999)));  // 后缀：最后500字节
        assert_eq!(parse_range("bytes=0-", 2000), Some((0, 1999)));
        assert_eq!(parse_range("garbage", 2000), None);
        assert_eq!(parse_range("bytes=9999-", 2000), None); // 起点越界
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

`cargo test --manifest-path src-tauri/Cargo.toml --lib media_tests`
Expected: 编译失败（函数未定义）。

- [ ] **Step 3: 实现 helpers**

`src-tauri/src/lib.rs`（在 `mime_from_ext` 附近）：

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum MediaKind { Image, Video, Audio, Unsupported }

pub(crate) fn media_kind_from_ext(path: &std::path::Path) -> MediaKind {
    match path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("png") | Some("jpg") | Some("jpeg") | Some("webp") | Some("gif") | Some("bmp") | Some("svg") | Some("ico") => MediaKind::Image,
        Some("mp4") | Some("webm") | Some("mov") | Some("mkv") | Some("m4v") => MediaKind::Video,
        Some("mp3") | Some("wav") | Some("flac") | Some("ogg") | Some("m4a") | Some("aac") | Some("opus") => MediaKind::Audio,
        _ => MediaKind::Unsupported,
    }
}

// 扩 mime：覆盖图/视频/音频；未知→application/octet-stream
pub(crate) fn mime_from_ext(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("mkv") => "video/x-matroska",
        Some("m4v") => "video/x-m4v",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("flac") => "audio/flac",
        Some("ogg") => "audio/ogg",
        Some("m4a") => "audio/mp4",
        Some("aac") => "audio/aac",
        Some("opus") => "audio/ogg",
        _ => "application/octet-stream",
    }
}

/// 路径是否落在任一 root 下（先 canonicalize 双方；文件需存在才能 canonicalize）。
pub(crate) fn is_within_roots(path: &std::path::Path, roots: &[std::path::PathBuf]) -> bool {
    let canon = match path.canonicalize() { Ok(p) => p, Err(_) => return false };
    roots.iter().any(|r| r.canonicalize().map(|rc| canon.starts_with(rc)).unwrap_or(false))
}

/// 解析 `bytes=start-end` / `bytes=start-` / `bytes=-suffix`；返 (start, end_inclusive)。
pub(crate) fn parse_range(header: &str, total: u64) -> Option<(u64, u64)> {
    let h = header.strip_prefix("bytes=")?.trim();
    let (s, e) = h.split_once('-')?;
    if s.is_empty() {
        // 后缀：最后 N 字节
        let n: u64 = e.trim().parse().ok()?;
        if n == 0 { return None; }
        let start = total.saturating_sub(n);
        return Some((start, total - 1));
    }
    let start: u64 = s.trim().parse().ok()?;
    if start >= total { return None; }
    let end = if e.trim().is_empty() { total - 1 } else {
        let v: u64 = e.trim().parse().ok()?;
        v.min(total - 1)
    };
    if end < start { return None; }
    Some((start, end))
}
```

注：旧的 `mime_from_ext` 若已在 lib.rs（bg 用），整体替换为上面这版（bg 调用处签名不变）。

- [ ] **Step 4: 跑测试确认通过**

`cargo test --manifest-path src-tauri/Cargo.toml --lib media_tests`
Expected: 4 passed。

- [ ] **Step 5: 注册 `media://` 协议 handler**

`src-tauri/Cargo.toml` `[dependencies]` 加：
```toml
percent-encoding = "2"
```

`src-tauri/src/lib.rs` 顶部 `use`：
```rust
use std::path::PathBuf;
```

在 `run()` 的 `tauri::Builder::default()` 链上（`.setup(...)` 之前或之后均可，链式）插入：

```rust
.register_uri_scheme_protocol("media", |app, request| {
    use tauri::http::Response;
    // 1. 取路径：URI path 去掉前导 '/'，percent-decode
    let raw = request.uri().path();
    let encoded = raw.trim_start_matches('/');
    let decoded = percent_encoding::percent_decode_str(encoded).decode_utf8_lossy();
    let path = PathBuf::from(decoded.as_ref());
    // 2. scope 校验：必须在 workspace 或 app_data 下
    let ws = PathBuf::from(crate::config::load(app).workspace_dir);
    let appdata = app.path().app_data_dir().unwrap_or_else(|_| PathBuf::from("."));
    let canon = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => return Response::builder().status(404).body(b"not found".to_vec().into()).unwrap(),
    };
    if !is_within_roots(&canon, &[ws, appdata]) {
        return Response::builder().status(403).body(b"forbidden".to_vec().into()).unwrap();
    }
    // 3. 读文件大小 + mime
    let total = match std::fs::metadata(&canon).map(|m| m.len()) {
        Ok(n) => n,
        Err(_) => return Response::builder().status(404).body(b"not found".to_vec().into()).unwrap(),
    };
    let mime = mime_from_ext(&canon);
    // 4. Range 处理
    let range = request.headers().get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(|h| parse_range(h, total));
    let (status, body, extra) = match range {
        Some((s, e)) => {
            let len = (e - s + 1) as usize;
            let mut f = match std::fs::File::open(&canon) {
                Ok(f) => f, Err(_) => return Response::builder().status(404).body(vec![].into()).unwrap(),
            };
            use std::io::{Read, Seek, SeekFrom};
            let _ = f.seek(SeekFrom::Start(s));
            let mut buf = vec![0u8; len];
            let read = f.read(&mut buf).unwrap_or(0);
            buf.truncate(read);
            (206, buf, format!("bytes {s}-{e}/{total}"))
        }
        None => {
            let buf = std::fs::read(&canon).unwrap_or_default();
            (200, buf, String::new())
        }
    };
    let clen = body.len().to_string();
    let mut b = Response::builder().status(status)
        .header("content-type", mime)
        .header("accept-ranges", "bytes")
        .header("content-length", clen);
    if status == 206 { b = b.header("content-range", extra); }
    b.body(body.into()).unwrap()
})
```

> 实现期确认：`register_uri_scheme_protocol` 在当前 tauri 2.x 的确切签名（`Fn(&AppHandle, Request<Vec<u8>>) -> Response<Cow<[u8]>>`）。`body.into()` 需 `Vec<u8>→Cow<[u8]>`（Owned）。`cargo build` 会暴露签名差异，按编译器提示修。

- [ ] **Step 6: 构建确认**

`cargo build --manifest-path src-tauri/Cargo.toml`
Expected: 零 error/零 warning（签名不符则按编译器修）。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/Cargo.toml src-tauri/src/lib.rs
git commit -m "feat(media): media:// 协议 + Range/206 + scope 校验 + kind/mime helpers

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 2: `display_media` 工具

**Files:**
- Modify: `src-tauri/src/tools.rs`（`tool_display_media` + `schemas` + `dispatch`）

**Interfaces:**
- Consumes: `crate::media_kind_from_ext` / `MediaKind`（Task 1 产出，pub(crate)）、`resolve_path`。
- Produces: `tool_display_media(args, workspace) -> String`（JSON）；schema 加 `display_media`；dispatch 路由。

- [ ] **Step 1: 写失败测试**

`src-tauri/src/tools.rs` `#[cfg(test)] mod tests` 追加：

```rust
#[test]
fn display_media_missing_path_errors() {
    let dir = tempfile::tempdir().unwrap();
    let r = tool_display_media(&serde_json::json!({}), dir.path());
    assert!(r.contains("\"display\":false"), "got: {r}");
    assert!(r.contains("缺少 path"));
}

#[test]
fn display_media_missing_file_errors() {
    let dir = tempfile::tempdir().unwrap();
    let r = tool_display_media(&serde_json::json!({"path":"nope.mp4"}), dir.path());
    assert!(r.contains("文件不存在"));
}

#[test]
fn display_media_unsupported_ext_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
    let r = tool_display_media(&serde_json::json!({"path":"a.txt"}), dir.path());
    assert!(r.contains("不支持"));
}

#[test]
fn display_media_ok_returns_json() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("cat.mp4"), b"x").unwrap();
    let r = tool_display_media(&serde_json::json!({"path":"cat.mp4","caption":"夕阳猫"}), dir.path());
    let v: serde_json::Value = serde_json::from_str(&r).unwrap();
    assert_eq!(v["display"], true);
    assert_eq!(v["kind"], "video");
    assert!(v["path"].as_str().unwrap().ends_with("cat.mp4"));
    assert_eq!(v["caption"], "夕阳猫");
}

#[test]
fn schemas_has_four_tools() {
    let names: Vec<&str> = schemas().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["write", "read", "bash", "display_media"]);
}
```

- [ ] **Step 2: 跑测试确认失败**

`cargo test --manifest-path src-tauri/Cargo.toml --lib tools::tests`
Expected: 编译失败（`tool_display_media` 未定义 / schemas 仍 3 项）。

- [ ] **Step 3: 实现 `tool_display_media`**

`src-tauri/src/tools.rs`（`tool_read` 附近）：

```rust
/// 展示本地媒体（图/视频/音频）：解析路径、校验存在/类型，返结构化 JSON 供前端渲染媒体卡。
pub fn tool_display_media(args: &Value, workspace: &Path) -> String {
    let p = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return serde_json::json!({ "display": false, "error": "缺少 path 参数" }).to_string(),
    };
    let path = resolve_path(p, workspace);
    if !path.exists() {
        return serde_json::json!({ "display": false, "error": format!("文件不存在: {}", path.display()) }).to_string();
    }
    let kind = crate::media_kind_from_ext(&path);
    let kind_str = match kind {
        crate::MediaKind::Image => "image",
        crate::MediaKind::Video => "video",
        crate::MediaKind::Audio => "audio",
        crate::MediaKind::Unsupported => {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            return serde_json::json!({ "display": false, "error": format!("不支持的媒体类型: .{ext}") }).to_string();
        }
    };
    let abs = path.canonicalize().unwrap_or(path).to_string_lossy().to_string();
    let caption = args.get("caption").and_then(|v| v.as_str()).unwrap_or("");
    serde_json::json!({ "display": true, "path": abs, "kind": kind_str, "caption": caption }).to_string()
}
```

- [ ] **Step 4: schemas 加 display_media + dispatch 路由**

`schemas()` 末尾（`bash` 之后）追加：

```rust
serde_json::json!({
    "type":"function",
    "function":{
        "name":"display_media",
        "description":"在对话中展示本地媒体文件（图片/视频/音频）。路径相对工作目录；长任务（视频/音乐）产物完成后调用以展示给用户。",
        "parameters":{
            "type":"object",
            "properties":{
                "path":{"type":"string","description":"媒体文件路径，相对工作目录或绝对路径"},
                "kind":{"type":"string","enum":["image","video","audio"],"description":"可选；缺省按扩展名推断"},
                "caption":{"type":"string","description":"可选；媒体下方说明文字"}
            },
            "required":["path"]
        }
    }
}),
```

`dispatch()` match 加：

```rust
"display_media" => tool_display_media(&args, &ctx.workspace),
```

（在 `"read" => ...` 之后、`"bash" => ...` 任意位置）

- [ ] **Step 5: 跑测试确认通过**

`cargo test --manifest-path src-tauri/Cargo.toml --lib tools::tests`
Expected: 全过（含新 5 项，schemas 4 项）。

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/tools.rs
git commit -m "feat(media): display_media 工具 + schema + dispatch

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 3: 前端媒体卡渲染 + CSS

**Files:**
- Modify: `src/main.js`（tool-call/result 管线特判 + `renderMediaCard`）
- Modify: `src/styles.css`（`.media-card` + img/video/audio 尺寸）

**Interfaces:**
- Consumes: `window.__TAURI__.core.convertFileSrc`（构建 media:// URL）；`llm-tool-call`/`llm-tool-result` 事件。
- Produces: `renderMediaCard(wrap, j)` 建媒体卡；tool 管线特判 display_media。

- [ ] **Step 1: 实现媒体卡渲染 + 管线特判**

`src/main.js`：

(1) 顶部拿 `convertFileSrc`（在已有的 `const { invoke }` / `const { listen }` 附近）：
```js
const { convertFileSrc } = window.__TAURI__.core;
```

(2) 新增 `renderMediaCard`（放在 `fillToolResult` 附近）：
```js
// display_media 媒体卡：玻璃底 + img/video/audio + 系统查看器打开链接
function renderMediaCard(wrap, j) {
  const tools = wrap.querySelector(".bubble-tools");
  const card = document.createElement("div");
  card.className = "media-card";
  const url = convertFileSrc(j.path, "media");
  const filename = String(j.path).split(/[\\/]/).pop();
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
  card.appendChild(media);
  if (j.caption) {
    const c = document.createElement("div");
    c.className = "media-caption"; c.textContent = j.caption;
    card.appendChild(c);
  }
  const open = document.createElement("button");
  open.type = "button"; open.className = "media-open link-btn";
  open.textContent = "在系统查看器打开";
  open.addEventListener("click", () => invoke("plugin:opener|open_path", { path: j.path }).catch(() => {}));
  card.appendChild(open);
  tools.appendChild(card);
  scrollBottom();
}
```

(3) 改 `setupAgentEvents` 里的 tool-call / tool-result 处理，特判 display_media：
```js
await listen("llm-tool-call", (e) => {
  const p = e.payload || {};
  if (p.name === "display_media") {
    // 占位卡：玻璃底 + 文件名 + 准备中（result 到再换）
    const tools = activeAssistantWrap?.querySelector(".bubble-tools");
    if (tools) {
      const ph = document.createElement("div");
      ph.className = "media-card media-loading";
      const fname = String(p.args || "").replace(/.*"path"\s*:\s*"([^"]*)".*/, "$1").split(/[\\/]/).pop();
      ph.textContent = `准备中… ${fname || ""}`;
      tools.appendChild(ph); scrollBottom();
    }
    return;
  }
  appendToolCard(p.name || "?", p.args || "");
});
await listen("llm-tool-result", (e) => {
  const p = e.payload || {};
  if (p.name === "display_media") {
    if (!activeAssistantWrap) return;
    const tools = activeAssistantWrap.querySelector(".bubble-tools");
    const ph = tools?.querySelector(".media-loading");
    if (ph) ph.remove();
    let j; try { j = JSON.parse(p.result || "{}"); } catch { j = {}; }
    if (j.display) renderMediaCard(activeAssistantWrap, j);
    else {
      const err = document.createElement("div");
      err.className = "media-card media-error";
      err.textContent = j.error || "展示失败";
      tools?.appendChild(err); scrollBottom();
    }
    return;
  }
  fillToolResult(p.name || "?", p.result || "");
});
```

> 注：`args` 里的 path 解析用保守正则（前端不必精确解析 JSON，仅展示文件名占位）；正式渲染以 tool-result 的 `j.path` 为准。

- [ ] **Step 2: CSS**

`src/styles.css` 末尾追加：
```css
/* ===== 媒体卡（玻璃，随 --glass-alpha）===== */
.media-card {
  align-self: stretch;
  max-width: 100%;
  margin-top: 6px;
  padding: 8px;
  border-radius: var(--radius-bubble);
  background: rgba(255, 255, 255, var(--glass-alpha));
  display: flex; flex-direction: column; gap: 6px;
}
.media-card.media-loading { color: var(--text-secondary); font-size: 13px; padding: 10px 12px; }
.media-card.media-error { color: var(--error); font-size: 13px; padding: 10px 12px; word-break: break-all; }
.media-card img,
.media-card video {
  max-width: 100%; max-height: 360px; object-fit: contain;
  border-radius: 10px; display: block; cursor: zoom-in;
}
.media-card video { cursor: default; }
.media-card audio { width: 100%; }
.media-card img { cursor: zoom-in; }
.media-caption { font-size: 13px; color: var(--text-secondary); }
.media-open { align-self: flex-start; font-size: 12px; }
```

- [ ] **Step 3: 手测（smoke）**

`pnpm tauri dev` → 让 agent 跑 `生成一段视频：夕阳下猫` → job 完成 → agent 调 display_media → 玻璃视频卡出现在工具区、可播放/拖进度。临时造图：在工作目录放 `t.png`，让 agent 调 `display_media({path:"t.png"})` → 图片卡显示、点击放大。
Expected: 卡片玻璃感、随透明度滑块变、视频可拖进度。

- [ ] **Step 4: 提交**

```bash
git add src/main.js src/styles.css
git commit -m "feat(media): display_media 媒体卡渲染（玻璃底 img/video/audio）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 4: 应用内 lightbox

**Files:** `src/main.js`、`src/styles.css`

- [ ] **Step 1: 实现 `openLightbox`**

`src/main.js`：
```js
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
```

- [ ] **Step 2: CSS**

`src/styles.css` 追加：
```css
.media-lightbox {
  position: fixed; inset: 0; z-index: 9999;
  background: rgba(0, 0, 0, 0.85);
  display: flex; align-items: center; justify-content: center;
  cursor: zoom-out;
}
.media-lightbox img { max-width: 92vw; max-height: 92vh; object-fit: contain; border-radius: 6px; cursor: zoom-out; }
```

- [ ] **Step 3: 手测** — 点图片 → 全屏放大 → Esc/点遮罩/点图关闭，焦点归还。

- [ ] **Step 4: 提交**

```bash
git add src/main.js src/styles.css
git commit -m "feat(media): 图片点击应用内 lightbox（Esc/焦点/aria-modal）

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 5: 交互状态收尾 + 打开链接

> 大部分已在 Task 3 落地（占位卡、error 卡、打开链接）。本任务补：图片/视频字节加载失败的兜底显示 + 损坏/编解码失败的"打开"提示。

**Files:** `src/main.js`

- [ ] **Step 1: 媒体元素 error 事件 → 兜底**

`renderMediaCard` 里给 img/video/audio 加 error 监听（建媒体后追加）：
```js
  media.addEventListener("error", () => {
    const note = document.createElement("div");
    note.className = "media-error-note";
    note.textContent = "无法加载或解码该文件";
    card.insertBefore(note, media.nextSibling);
  });
```
CSS（styles.css 追加）：`.media-error-note { font-size: 12px; color: var(--error); }`

- [ ] **Step 2: 手测** — 指向一个损坏/不支持的文件（如把 .mp4 改成乱码内容）→ 显示错误提示 + 打开链接仍可调系统播放器。

- [ ] **Step 3: 提交**

```bash
git add src/main.js src/styles.css
git commit -m "feat(media): 加载/解码失败兜底提示

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Task 6: 全量验证 + 文档

**Files:** `docs/superpowers/smoke/2026-07-24-chat-media-rendering.md`（新建）

- [ ] **Step 1: 跑全量测试 + 构建**

```bash
cargo test --manifest-path src-tauri/Cargo.toml --lib
cargo build --manifest-path src-tauri/Cargo.toml
```
Expected: 全过、零 warning。

- [ ] **Step 2: 写 smoke 清单**（`docs/superpowers/smoke/2026-07-24-chat-media-rendering.md`）：图片显示+放大、视频播放拖进度、音频播放、缺失文件降级、不支持扩展名降级、损坏文件兜底、系统查看器打开、媒体卡随透明度滑块变、媒体卡在正文上方按序排列、lightbox Esc/焦点。

- [ ] **Step 3: 提交**

```bash
git add docs/superpowers/smoke/2026-07-24-chat-media-rendering.md
git commit -m "docs(smoke): chat-media 渲染冒烟清单

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

## Self-Review

1. **Spec 覆盖**：图/视频/音频渲染 ✓、display_media 工具 ✓、media:// 协议+Range/scope ✓、媒体卡位置/玻璃/lightbox/状态表/a11y ✓、打开兜底 ✓。
2. **占位符扫描**：无 TBD/TODO；Step 5 的"确认签名"是版本相关核实点，非逻辑占位。
3. **类型一致**：`MediaKind`/`media_kind_from_ext` 在 lib.rs 定义、tools.rs 通过 `crate::` 用；`renderMediaCard(j)` 字段 `{display,path,kind,caption}` 与 tool 返回 JSON 一致；前端 `convertFileSrc(path,"media")` ↔ handler `media` 协议名一致。
4. **依赖序**：T1↔T2 可并行；T3 依赖 T1+T2；T4/T5 依赖 T3；T6 收尾。

## Execution Handoff

计划已保存到 `docs/superpowers/plans/2026-07-24-chat-media-rendering.md`。两种执行方式：

**1. Subagent-Driven（推荐）** — 每任务一个全新 implementer + 任务间 review，迭代快（上次的 agent-session-jobs 就是这套）。REQUIRED SUB-SKILL: superpowers:subagent-driven-development。

**2. Inline 执行** — 本会话内 batch 执行 + checkpoint。
