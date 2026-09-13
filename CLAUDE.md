# ovoice 项目开发约定

> 本文件随项目走，每次会话自动加载；与全局 `~/.claude/CLAUDE.md` 一并生效。

## 分支策略（强制）

- **所有新功能开发必须在 `feat/<功能名>` 分支上进行，禁止直接往 `master` 提交功能代码。**
- `master` 是干净基线，只通过合并 feat 分支前进；任何时刻都应能作为可回滚的起点。
- bug 修复走 `fix/<问题名>` 或在对应 feat 分支内进行，同样不直接落 master。
- 切 feat 分支前先从最新 `master` 拉；长时开发期间定期把 master 合回 feat 保持同步、减少最终合并冲突。
- 实验性 / 被推翻的分支可保留作存档（例如 `feat/context-management` 存了 v1 实现），但不要在其上继续叠新功能；新功能另开分支。

## 历史注记

- **2026-07-25**：context-management v1 曾被直接堆在 master 上（14 个 commit，`ea2dc0f..88df93a`）。已整体移到 `feat/context-management` 分支存档，master 回退到干净基线 `1917934`。v1 设计被推翻（trace 只记 4 类 SessionEvent、对话历史纯内存重启即丢、consolidate 乱触发、workspace 埋在 `%APPDATA%` 违备份份初衷），将在新的 feat 分支上重做（v2）。

## 绿色打包 release 前置（spec §10.3/§13.4）

dev 下 mem CLI 已可用：`tool_bash`（前台）与 `jobs::spawn_process_job`（后台）在 spawn 前运行时注入 `PATH`，前置 `current_exe()` 父目录（`tools::path_with_exe_dir()`），不改系统环境。dev 下 `mem` 与 `ovoice.exe` 同在 `target/debug/`，故 bash 直接可调。

**release 构建前置（CI/release 脚本职责，不属本仓库源码）**：

1. 先 `cargo build --release --manifest-path src-tauri/Cargo.toml --bin ovoice --bin mem`，确认两个 exe 都生成。
2. 取 host triple：`for /f "tokens=*" %%i in ('rustc -vV ^| findstr host') do set TRIPLE=%%i`（Windows bat；或 `rustc -vV | grep host | cut -d' ' -f2` 于 *sh）。
3. 把 `src-tauri/target/release/mem.exe` 复制成 `src-tauri/binaries/mem-<TRIPLE>.exe`（Tauri externalBin 约定：源侧带 triple 后缀，bundle 时去后缀落主 exe 同目录）。
4. 在 `src-tauri/tauri.conf.json` 的 `bundle` 段加 `"externalBin": ["binaries/mem"]`，再 `tauri build`。

**重要：externalBin 必须在 release 前一步加（不能常驻 main 分支的 tauri.conf.json）**——Tauri 2 的 `tauri-build` 在每次 cargo check/build/test 都会校验 `binaries/mem-<TRIPLE>.exe` 存在，文件缺失时编译直接失败（包括 `cargo check --tests` 与 dev server 热重载）。所以 externalBin 项不放常规源码；release 脚本动态注入或单独 release-only 提交。`src-tauri/binaries/` 应进 `.gitignore`（target-triple 特定 + 二进制 scratch）。
