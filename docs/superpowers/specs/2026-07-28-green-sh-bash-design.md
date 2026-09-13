# 绿色 sh：跨平台 bash 工具重设计（v2）

> 日期：2026-07-28 · 状态：spec v2（busybox 实测全过；§11 双 spike ✅ PASS：kill_tree + CREATE_NO_WINDOW）
> 实现分支：`feat/bash-green-sh`（未开）· 二进制已 stage：`src-tauri/binaries/busybox64u.exe`（SHA256 验）
> 前序调研：claude-code/codex/cursor bash 对照、bash GPL 可行性、busybox-w32 真机实测（mmx/mem/ls/UTF-8/引号全过）。

## 0. 实测验证（2026-07-28，bash 侧全过）

busybox-w32 `busybox64u.exe`（v1.38.0-FRP-6075-g169694ebd, 2026-05-06, Unicode build, 675,840 B）已下载到 `src-tauri/binaries/`，SHA256 `6e263d154d8548d1eb936f65d1d8312c80df31c45974e48d6335e4dcc0f4f34c` = 官方 SHA256SUM。下载源 `https://frippery.org/files/busybox/busybox64u.exe`（**是 `/files/busybox/`，不是 `/busybox/`**——踩过的坑）。

bash 侧（直接 `busybox64u.exe sh -c "..."`）：

| 项 | 结果 |
|---|---|
| sh 基本 + 算术 / applet 全齐（sh/grep/sed/awk/find/tar…） | ✓ |
| 中文 UTF-8（echo + mem 读出真事件） | ✓ 原生 → chcp 可删 |
| Windows 路径正/反斜杠 / env 继承（APPDATA/OVOICE_CACHE/PATH） | ✓ |
| PATH 调真工具链（git 2.41 / cargo 1.95 / node 24.12） | ✓ |
| **mmx** auth status（真调用，key 自脱敏 `sk-c...HiWY`） | ✓ |
| **mem** ls / index 日（读 ovoice 真记忆，中文完整） | ✓ |
| **#01 引号压力**（grep 中文 / 嵌套引号 / 特殊字符字面 / 变量展开 / for / 多引号参数） | ✓ 全过 → cmd 引号地狱根治 |

Rust 侧（见 §11）**双 spike 全 PASS**：①**kill_tree ✅**（`examples/spike_kill_tree.rs`：`busybox sh "sleep 30 & sleep 30"` → during=3 = sh+2 独立子进程，kill_tree 杀 3、after=0、零孤儿 → D6 成立）；②**CREATE_NO_WINDOW ✅**（`examples/spike_create_no_window.rs`：windows-subsystem 父进程 spawn busybox，no-flag 时 busybox 自己名下冒出可见控制台窗口、带 `0x0800_0000` 时无可见窗口 → D4 成立；另验 `DETACHED_PROCESS` 无控制台 + 输出捕获（含 mmx）正常作 terminal-independent 备选）。常量 `0x0800_0000` = windows-rs 官方 `CREATE_NO_WINDOW`（134217728，跨 0.45–0.61 一致）。

## 1. 背景（Context）

ovoice 现有 bash 工具硬绑 Windows cmd：

- 前台 `tool_bash`（`tools.rs:441`）：spawn `cmd /C "chcp 65001>nul && <cmd>"`。
- 后台 `spawn_process_job`（`jobs.rs:402`）：同样 `cmd /C`，整个函数 `#[cfg(windows)]`。

痛点：

- **#001 引号**：cmd 解析器把内嵌引号拆碎（`findstr /C:"OS 名称"` 闭合失败）。
- **#002 黑窗**：release 是 GUI 子系统（`main.rs:2` `windows_subsystem="windows"`），spawn cmd 未传 `CREATE_NO_WINDOW` → 慢命令（mmx）弹黑窗。echo/hostname 因毫秒级结束看不见。
- **非 Windows 不通**：`Command::new("cmd")` 无 Unix 分支，`spawn_process_job` 整个 cfg(windows)。
- **环境泄漏**：父进程 env 全量透传给子进程（潜在 MiniMax key 泄漏）。
- **fg/bg 重复**：`tool_bash` 与 `spawn_process_job` 各自构造 Command，硬化要同步两处。

调研结论（2026-07-28）：

- bash 是 **GPL-3+** 开源；通过 spawn 调用独立 GPL 程序**不构成衍生作品**（FSF mere-aggregation FAQ），闭源 app 可合法自带，仅需附源码/声明。
- **busybox-w32**（GPL-2，单 exe ~3MB，无 DLL，零安装）是「绿色 sh」标准答案，提供 POSIX sh(ash) + coreutils。
- Codex/Claude Code/Cursor 跨平台 bash 共性：`std::process::Command` + 平台 shell + 跨平台杀树原语。
- OS 沙箱 Windows 原生不可行（Cursor 都走 WSL2），**本项目不做沙箱**。

## 2. 目标（Goal）

用 **一套 POSIX sh** 通吃三平台，彻底脱离 cmd：

- **Windows**：自带 busybox-w32，调 `busybox.exe sh -c "<cmd>"`。
- **macOS / Linux**：系统 `/bin/sh -c "<cmd>"`。
- 同一份执行器 body（spawn/wait/timeout/drain/结果塑形/env scrub）三平台共用。
- 顺带修：#001（sh 单引号整串传参，无 cmd 再解析）、#002（CREATE_NO_WINDOW）、env scrub、fg/bg 统一、结果结构化。

## 3. 非目标（Out of scope）

- **ovoice 在 mac/Linux 发行**（Tauri mac bundle / 签名 / notarization、voice_hotkey、构建流水线）——bash 工具只是「不再是阻塞点」。
- **OS 沙箱**（Seatbelt/Landlock）——Windows 原生不可行；Unix 侧将来可做，本重写不含。
- **cmd 回退**——故意不做（见 D2）。busybox 在 Windows 上是硬依赖。
- **切 PowerShell**——不做（LLM 现有命令风格 + 冷启动成本不划算）。

## 4. 架构

```
shell/mod.rs        ShellKind + pick()
                      ├─ Windows → BusyboxSh
                      └─ Unix    → SystemSh
shell/busybox.rs    busybox.exe sh -c  + raw_arg + CREATE_NO_WINDOW
shell/sh.rs         /bin/sh -c  + process_group(0)

bash.rs             build_command(cmd, env, cwd, stdio) -> Command   // 唯一构造点
                    run_foreground(...) -> BashResult   // spawn + wait + timeout + kill_tree
                    run_background(...)  -> JobId        // spawn + drain + register + guardian
                    env_clear + 白名单重建
killtree            kill_tree crate（替 win_job；跨平台杀树；父死兜底 kill_on_drop）
BashResult          { exit_code, stdout, stderr, truncated, timed_out, shell }
```

平台差异全收在 `shell/*` 两个 impl；`bash.rs` 的 body 平台无关。与 Codex `process_exec_tool_call` 路由 + `spawn_child_async` 平台分支同构。

## 5. 决策记录

- **D1 shell**：busybox-w32 sh（Win）+ 系统 `/bin/sh`（Unix）。一套 POSIX sh 通吃。
- **D2 无 cmd 回退**：busybox 是 Windows 硬依赖。缺失 → 明确报错（dev：把 `busybox.exe` 放 ovoice.exe 同目录；release：随包自带）。符合「不吊死在 cmd」。
- **D3 删 chcp**：实测 sh + MSYS 层 UTF-8 原生（中文 echo + mem 读出均正常），不再需要 `chcp 65001`。
- **D4 CREATE_NO_WINDOW**：Windows 必加（`0x0800_0000`，= windows-rs 官方 `CREATE_NO_WINDOW` 134217728），与 shell 选择无关。Unix 无控制台概念 → no-op。**已验（§11 #1）**：windows-subsystem 父进程 spawn busybox 带 CNW → 零可见窗口（no-flag 则 busybox 自己名下冒可见控制台窗）。备选 `DETACHED_PROCESS(0x8)`（无控制台、terminal-independent）已验输出捕获含 mmx 正常；CNW 首选（保 console handle）。
- **D5 env scrub**：`env_clear()` + 白名单重建（`OVOICE_CACHE` / `OVOICE_WORKSPACE` / `PATH` / `MINIMAX_REGION` + 用户显式 env），防凭证泄漏（Codex 金标准）。
- **D6 杀树**：引 `kill_tree` crate（替现有 `win_job` 模块），跨平台 Win/Mac/Linux。父崩溃孤儿兜底：`kill_on_drop(true)` + drop 时 `kill_tree`。
- **D7 结果结构化**：`BashResult` JSON 给 LLM（机器可读）+ 兼容字符串渲染给前端（替代现 `"退出码 N\n..."`）。
- **D8 GPL 合规**：随附 busybox-w32 二进制 + `THIRD_PARTY.md`（GPL-2 全文 + 源码链接 + busybox-w32 上游）+ About 框声明。复用 `mem.exe` 的 release 自带机制（`externalBin` 或 `current_exe()` 同目录 + `tools::path_with_exe_dir()` 注入 PATH）。
- **D9 工具签名不变**：`tool_bash {command, background, env}` 保持，只换内部。dispatch / 前端 / 历史渲染零改。
- **D10 LLM 适配**：system prompt 注入 `{os, shell:"sh", cwd, 提示:"POSIX sh 语法，勿用 cmd/bash 专属语法"}`；`AGENT.md` 同步更新。

## 6. 已知取舍 / 风险

- **ash ≠ bash**：`${var,,}`、`shopt`、`declare -a` 数组、`<(...)` 进程替换等 bash-ism 不支持。契约 = POSIX sh。`AGENT.md` 列出不支持语法。
- **路径翻译**：实测 Windows 路径在 busybox sh 下正/反斜杠均可用（原顾虑 cleared，见 §0）。`current_dir` 仍用 Rust `Path`（平台无关）。
- **kill_tree「遍历杀」≠ Job Object「句柄关即杀」**：功能等价（超时杀整树），父崩溃孤儿靠 `kill_on_drop` 补，行为接近但不完全等同 Job Object 的强保证。
- **包体 +660KB**：`busybox64u.exe` 随包（实测 675,840 B，非早期估算的 3MB）。
- **GPL 归属**：必须随附（D8），合规成本≈零但不能漏。

## 7. 迁移

- `AGENT.md`：加 / 改「shell 环境」段（POSIX sh、平台、不支持的 bash-ism、自带 busybox 说明）。
- system prompt：注入 shell / 平台（`context.rs` 拼装 system 时追加）。
- 历史 jsonl：不受影响（工具签名不变）。
- 现有 cmd 专属示例（若有）：改 sh 等价。

## 8. 测试（TDD，纯逻辑先行）

- env 白名单重建：`env_clear` 后只剩白名单 + 用户 env。
- `BashResult` 塑形：stdout/stderr 合并、截断、`timed_out` 标志。
- shell `pick`：`cfg(windows)` → BusyboxSh / else → SystemSh。
- `build_command`：各平台命令行拼装正确（sh -c 单引号整串传一个 arg）。
- `kill_tree` 集成：起会 spawn 子进程的命令（`sh -c "sleep 30 & sleep 30"`），超时后整树死（验无孤儿）。
- busybox.exe 缺失报错路径单测（Windows）。
- 回归：`tool_bash` 签名、fg/bg、subagent ctx 强制前台。
- 约束：dev server 锁 exe → `cargo check --tests` / `cargo test --lib`，不 `cargo build/run`（[[ovoice-dev-server-cargo-lock]]）。

## 9. 分支

`feat/bash-green-sh`（从 master 新开）。严禁直接落 master；本 spec 可先进 master（项目惯例），实现走 feat 分支。

## 10. 已决（原开放问题）

- **busybox 版本/源**：锁定 `busybox64u.exe` FRP-6075-g169694ebd（2026-05-06），源 `https://frippery.org/files/busybox/`，SHA256 已验（§0）。用官方预编译，不自编译。
- **`lib.rs:206` `cmd /C start`**（打开文件夹，非 bash 工具）：保留，不在本重写范围。
- **命令门禁**（catastrophic 黑名单）：另开 feat，本重写不含。先让 sh 迁移稳定再加固。

## 11. 预开发验证门（Rust spike，必须先过再写执行器）

bash 侧已全过（§0）。以下两项是 Rust 集成未知，写执行器前用**最小 Rust spike** 验证（一次性验证程序，非执行器本体）：

1. **CREATE_NO_WINDOW spike**：`#![windows_subsystem = "windows"]` 的小程序，spawn `busybox64u.exe sh -c "echo hi; sleep 1"`，分别带 / 不带 `creation_flags(0x0800_0000)`，目视确认带标志时无黑窗、stdout 仍正常捕获。这是 #02 的最终验证（bash 侧无法测，因无 GUI 上下文）。
   - ✅ **已验 PASS（2026-07-28，`examples/spike_create_no_window.rs`）**：windows-subsystem 父进程（模拟 release ovoice）spawn busybox，A/B 对照。检测法演进：全局 conhost 计数（v1，噪声大）→ per-process conhost 子进程（v2，干净：CNW 下 busybox 名下仍有 conhost 子=控制台被分配）→ DETACHED 判别（CNW 分配控制台、DETACHED 不分配；busybox **不**强 AllocConsole）→ **可见顶层窗口 pid 集 delta（终判）**：no-flag 时 spawn 后多出一个可见顶层窗口、归属 **busybox 自己**（pid = 所 spawn busybox），CNW 时**零新增可见窗口** → CREATE_NO_WINDOW 确实隐藏窗口（conhost 在但无可见窗）。
   - **常量核对**：`CREATE_NO_WINDOW = 0x0800_0000` = windows-rs 官方值 134217728（跨 0.45–0.61 一致）；`lib.rs:204` 已用此值（reveal 路径），正确。
   - **备选**：`DETACHED_PROCESS(0x8)` 验证为 terminal-independent 兜底——不分配控制台（必无窗），且 busybox + mmx 输出捕获正常（exit 0，stdout 含 auth JSON）。CNW 是首选（保留 console handle，app-compat 更稳）；若某机型 release 实测仍现窗，退到 DETACHED。
2. **kill_tree spike**：spawn `busybox64u.exe sh -c "sleep 30 & sleep 30 & echo pids"`，拿 pid 调 `kill_tree::kill_tree(pid, signal)`，确认两个 sleep 后代都被杀（无孤儿）。对照 Windows `taskkill /T`。
   - ✅ **已验 PASS（2026-07-28，`examples/spike_kill_tree.rs`）**：实测 before=0 / during=3 / after=0，kill_tree 报告杀 3 个（2 子 + sh 本身，parent_pid 链正确），零孤儿。证明 busybox-w32 的 `&` 确实 fork 独立 Windows 进程（非 in-process 协程），kill_tree 树遍历能杀净 → D6 成立。crate 版本锁定 0.2.4（`blocking` feature，Windows 走 TerminateProcess，signal 被忽略）。

两项均已过（2026-07-28）。spike 产物 `examples/spike_kill_tree.rs` + `examples/spike_create_no_window.rs` 留作集成测试。**预开发验证门全开 → 可写真执行器（§4）。**
