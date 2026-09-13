# ovoice 路径管理

> 给人看的参考。agent 行为规范见 `cache/AGENT.md` 的「路径与存放」段（每轮 pinned）。

## 两个根（config.json 可配）

| 根 | 字段 | 默认 | 用途 |
|---|---|---|---|
| **workspace** | `workspace_dir` | `Documents/ovoice/` | 用户文件根。write/read/bash 相对路径都解析到此；bash cwd = 此处。可见、可备份。 |
| **cache** | `cache_dir` | `%APPDATA%/com.ovoice.app/cache/` | ovoice 内部产物。agent 不直接写，由各子系统写；agent 只通过 `mem` CLI 读 memory+history。 |

两个根都空 → 走默认；非空 → 绝对路径原样 / 相对路径拼 app_data_dir（见 `config::resolve_*`）。便携版可把两者都指到 exe 同级，备份一棵即全部。

## workspace 结构（首启自动建）

```
workspace/
├── projects/   用户的项目；agent 做"某个项目"时在 projects/<项目名>/ 下干活
├── scripts/    临时脚本（要 bash 跑的 .ps1/.py/.sh）—— scratch，任务收尾清理
├── datas/      临时数据（中间产物、下载、转换结果）—— scratch，任务收尾清理
├── render/     要 display 渲染的 html/markdown（write 后再 display）—— scratch，任务收尾清理
└── <其他>      用户要保留的产物直接放根或自建子目录
```

`projects/ scripts/ datas/` 由 `spawn_session`（agent.rs）首启 `create_dir_all`，结构一开始就可见。agent 写文件时 `tool_write` 会自动补父目录，故未预建也能写。

## cache 结构（ovoice 内部，agent 勿直接动）

```
cache/
├── memory/{Y}/{M}/{date}.md    日事件段（dream 写，mem 读）—— append-only 真相
├── history/{date}.jsonl        原始逐字对话（单写）—— mem drill 的 seq 指针指向这里
├── dialogues/                  对话落盘（v2 JsonlWriter）
├── subagent_logs/              子代理完整对话日志
├── attachments/                用户拖入的附件（按内容 hash 去重）
├── .ovoice-jobs/{id}.log       后台 job 日志（唯一持久日志）
├── MEMORY.md                   记忆索引（日/月/年三层，pinned）
├── SOUL.md                     灵魂/风格（用户写，agent 永不改）
└── AGENT.md                    工具规范与路径规则（本文件内容的 agent 版，pinned）
```

## 日志

- **后台 job 日志**：`cache/.ovoice-jobs/<id>.log`（前台 `tool_bash` 起 background job 时写；`read_job_log(id)` 命令读）。
- **debug 日志**：全 `eprintln!`（stderr）——dev 控制台可见，release 即丢。无文件日志、无 tracing。

## ovoice 内部 ephemeral（不入表）

一次性、process 私有的中间文件走 OS `%TEMP%`（如 `extract.rs` 抽 docx 的临时副本），用完即删，agent 不接触、不感知。

## mem CLI 如何定位 cache

`mem` 是零 LLM 只读 drill，cache 路径解析链（`mem.rs::resolve_cache`）：

```
1. --cache <path>          显式覆盖（几乎不用）
2. $OVOICE_CACHE           agent bash spawn 时 ovoice 注入（= config.cache_dir 解析值）
3. next-to-exe config.json 便携 / 终端手动
4. %APPDATA% config.json   标准安装 / 终端手动
5. %APPDATA%/cache         兜底
```

config.json 的 `cache_dir` 是**单源真理**；`OVOICE_CACHE` 只是 ovoice 把已解析的路径递给 mem，不是第二份配置。agent 调 `mem` 零参数即可。

## 存放规则（agent 必守，见 AGENT.md）

1. 要保留的产物 → workspace 根或 `projects/<项目>/`。
2. **临时文件必须优先落到固定子目录，勿散落 workspace 根**：临时脚本 → `scripts/`；临时数据 → `datas/`；要 display 渲染的 html/markdown → `render/`（先 write 再 display）。三者 scratch，任务收尾清理（不自动删）。
3. 不直接写 `cache/`——memory/history/attachments 等由 ovoice 子系统管；只通过 `mem` 读 memory+history。
4. 不写 workspace 之外（系统目录、别的盘）；相对路径都在 workspace 内。
