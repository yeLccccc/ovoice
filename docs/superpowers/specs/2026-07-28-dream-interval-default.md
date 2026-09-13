# dream 闲置触发间隔默认值 10min → 120min（P-2026-003）

- **日期**：2026-07-28
- **关联提案**：`workspace/projects/ovoice-test/proposals/P-2026-003_dream间隔可配置.md`
- **优先级**：P2
- **状态**：实现中

## 背景

提案 P-2026-003：自动 dream 触发间隔固定 10min，太频繁——打断思路 + 每次 dream 重写上下文缓存层，频繁更新抖动缓存、影响会话连贯。建议默认改 120min 并可配。

## 现状（已确认，无需新增基建）

| 提案诉求 | 现状 | 结论 |
|---|---|---|
| ① 默认 10min → 120min | `config.rs:102 d_dream_idle() = 600` | **改**（唯一实质改动） |
| ② 暴露为可配参数（设置页） | `dream_idle_secs` 字段已存在（`config.rs:91`）；设置页 input 已存在（`index.html:125`）；driver 已读 `cfg.dream_idle_secs`（`agent.rs:286`） | **已满足，不动** |
| ③ 配置变更即时生效（不重启） | driver 启动时 `config::load` 一次入 `Arc<Config>`（`agent.rs:125/137`），`save_config`（`lib.rs:165`）只写盘不通知 driver | **延后**（见下） |

## 决策

### 本次改
- **默认值 600 → 7200**（秒；= 120min）：`d_dream_idle()`。
- **前端兜底常量同步**：`main.js` 两处 `?? 600` / `? 600` → 7200（清空 input 时回退到新默认，保持一致）。
- **测试**：断言默认值的处（`config.rs` `v2_fields_have_defaults`、`load_from_missing_file_uses_defaults`）+ roundtrip 字面量同步 7200。

### 延后（③ 不重启）
`dream_idle_secs` 当前与**所有** config 字段一致——启动快照、改设置后下次 session 生效（`max_tool_iters`、`dream_cap_turns` 等同样如此）。单独给 `dream_idle_secs` 加 live-reload（AtomicU64 / 重读 config）会破坏一致性、且超出「一个参数」范围。**通用 config 热重载**是独立任务，不在本 spec 内。本次接受「改设置后重启生效」，与其它字段行为对齐。

## 改动清单

| 文件:行 | 改动 |
|---|---|
| `src-tauri/src/config.rs:102` | `fn d_dream_idle() -> u64 { 600 }` → `7200` |
| `src-tauri/src/config.rs:308` | roundtrip 字面量 `dream_idle_secs: 600` → `7200`（与默认保持一致；该测试不 assert 此字段） |
| `src-tauri/src/config.rs:391` | `v2_fields_have_defaults`: `assert_eq!(c.dream_idle_secs, 600)` → `7200` |
| `src-tauri/src/config.rs:428` | `load_from_missing_file_uses_defaults`: `assert_eq!(c.dream_idle_secs, 600)` → `7200` |
| `src/main.js:423` | `cfg.dream_idle_secs ?? 600` → `?? 7200` |
| `src/main.js:456` | `=== "" ? 600 :` → `? 7200 :` |

**不动**：`config.rs:400/406`（roundtrip 测的是用户值 1200，非默认）、`dream.rs` 各 `check(...,600,50)`（600 是 trigger 逻辑测试输入，非默认）、`index.html`（label 无默认值文案、bounds `min=0 max=86400` 已覆盖提案建议的 30~480min）、`agent.rs`（已读 `cfg.dream_idle_secs`）。

## 验证

- `cargo test --lib --manifest-path src-tauri/Cargo.toml config::tests`（默认值断言过）。
- `node --check src/main.js`（兜底常量改后语法过）。
- 手测（重启 dev 后）：设置页 dream idle 显示 7200；清空 input 回退 7200。

## 约束

- 分支 `feat/dream-interval-default`（从 master），严禁直接落 master；merge 用 `--no-ff`。
- dev server 锁 exe → 用 `cargo test --lib`（不 build/run debug exe）。
- 提交信息以 `Co-Authored-By: Claude <noreply@anthropic.com>` 结尾。
