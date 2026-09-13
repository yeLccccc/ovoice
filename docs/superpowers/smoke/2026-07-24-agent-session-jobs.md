# agent-session-jobs 冒烟记录

> 日期：2026-07-24 · 分支 `feat/agent-session-jobs`（base master `bed02d1`..HEAD `ef85abb`）
> 自动化部分由 controller 完成；交互式 mmx 真机 smoke 需人工运行 `pnpm tauri dev` 后执行下表。

## 1. 自动化（controller 已跑，PASS）

| 检查 | 命令 | 结果 |
|---|---|---|
| 单元/回归 | `cargo test --manifest-path src-tauri/Cargo.toml --lib` | **67 passed; 0 failed**（config 8 + jobs 9 + tools 18 + llm 18 + agent 4 + 其它 10） |
| 全量构建（含前端嵌入） | `cargo build --manifest-path src-tauri/Cargo.toml` | ** Finished，零 warning** |
| 跨模块事件/命令契约 | 人工核对 lib.rs emit ↔ main.js listen | 9 事件全对齐（llm-thinking/content/tool-call/tool-result · chat-turn-start/end · chat-error · chat-reset · job-update）；命令 chat/reset_session/list_jobs/kill_job/read_job_log 全在 invoke_handler |

关键并发路径单测（jobs::tests）均通过：echo 完成 / 超时判 Failed 且杀树 / >4KB stdout 不死锁（pipe-drain）/ 达 MAX_RUNNING(8) 拒绝 / kill 不投递 JobDone。

## 2. 交互式 mmx 真机 smoke（人工执行，待回填）

前置：`taskkill //F //IM ovoice.exe`（释放 exe 占用）→ `pnpm tauri dev`。

| # | 操作 | 预期 | 实测 | 备注 |
|---|---|---|---|---|
| 1 | 发普通消息「你好」 | chat-turn-start 建气泡 → llm-content 流式增量 → chat-turn-end 渲染 markdown + 朗读按钮 | ☐ 待填 | 验证 turn 边界 |
| 2 | 让模型调前台 bash（如「列出工作目录文件」） | 工具卡出现 + 结果回填 + 最终答复 | ☐ 待填 | 前台 bash 不变 |
| 3 | 让模型生成视频：「生成一段视频：夕阳下猫坐窗边」 | 模型调 `bash{background:true,"command":"mmx video generate --prompt ... --download cat.mp4"}`；立即返回 job_id、助手气泡「生成中…」；面板 Running → Done；完成后新气泡汇报；文件落 workspace/minimax-output/ 或 cat.mp4 | ☐ 待填 | **核心**：验证触发即走 + 回调唤醒。记录耗时（验证 600s 后台超时够用） |
| 4 | 两个后台任务背靠背完成 | 各自独立气泡，不串 | ☐ 待填 | 验证 turn 边界防串气泡 |
| 5 | 点顶栏「任务」→ 面板列 job → 点「终止」 | 状态变「已终止」、不弹完成消息、进程树被杀 | ☐ 待填 | 验证 kill_job 不唤醒模型 + KILL_ON_JOB_CLOSE |
| 6 | 设置改 system_prompt 保存 | 对话清空 +「（已重置）」气泡 | ☐ 待填 | 验证 reset_session 重注入 system |
| 7 | 关 app（进程型 job 运行中） | 无 mmx 孤儿进程残留 | ☐ 待填 | 验证 Job 句柄随 app 退出关闭杀树 |

## 3. 已知限制 / 后续

- **不持久化**：Session + JobRegistry 内存态，app 重启即清（长视频随重启丢失，按设计）。
- **给模型的 job 管理工具**：v1 fire-and-forget，取消/查看走面板（无模型侧 list/kill 工具）。
- **子 agent（SubAgent 型 Job）**：二期，复用本期 Job/callback/Session/面板。
- **历史 summarization**：v1 仅折叠旧 tool 结果（window_messages，>40 条触发）；完整 summarization 进 TODO。
- **昂贵命令确认门**：后台+全自动烧 mmx quota；v1 面板可见可终止兜底，确认开关留 TODO。
- **`--async`+轮询**：v1 不教；视频走同步 `mmx video generate` 当后台 job 跑。`--async`+poll 留二期。

## 4. 回滚

`git revert <commit>` 或整支回退 `git reset --hard bed02d1`（master 合并前）。分支未推送。
