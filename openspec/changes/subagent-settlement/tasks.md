# Tasks · subagent-settlement

## 1. 结算三态诚实化（subagents.rs + history.rs + agent.rs）

- [x] 1.1 `history.rs::subagent_result()` 加 `ok: bool` 参数，data 插入 `"ok"` 字段；同步既有调用与测试
- [x] 1.2 `subagents.rs` 结算块重写：`Err(e)` → answer=`失败原因：{e}`（partial 非空附「部分产出：{partial}」）；`Ok("")` → answer=`(无输出)`；cancelled 分支不变；JobOutcome.ok 同步
- [x] 1.3 `agent.rs:115` JobDone→事件构造传 `o.ok`
- [x] 1.4 测试：subagents.rs 失败 round → answer 含原因+partial；`Ok("")` → `(无输出)`；cancelled → partial（新增 2 例，改 1 例）

## 2. 渲染层诚实化 + 护栏（context.rs）

- [x] 2.1 `context.rs:192-198` subagent_result 渲染臂：按 `data.ok`（缺省 true）选「完成」/「失败」
- [x] 2.2 同臂加 128KB 护栏（新 const `SUBAGENT_SUMMARY_RENDER_CAP`）：超限 `tools::truncate` + 显式标记「（已截断，完整产出可用 subagent status 查看）」
- [x] 2.3 测试：ok=false 渲染「失败」；无 ok 旧事件渲染「完成」；>128KB 截断带标记且落盘事件全文不变；prefix 组合不被破坏

## 3. 迭代上限配置化（config.rs + subagents.rs）

- [x] 3.1 `config.rs` 加 `subagent_max_iters: u64`（serde 默认 `d_subagent_max_iters() -> 5000`），Default 同步
- [x] 3.2 `subagents.rs` 删 `SUBAGENT_MAX_ITERS` 常量，spawn 处从 cfg 读；`0` → `usize::MAX`（不限制）
- [ ] 3.3 测试：默认 5000；cfg 传入生效；达到上限时 answer 携带「已达工具调用上限」兜底文案（run_turn 既有行为）

## 4. 提示词对齐 pi（config.rs + tools.rs）

- [x] 4.1 重写 `d_subagent_sys()`：受众意识 + workspace 约定 + 动手原则 + 四段骨架（## 结论/## 改动与产出/## 关键发现/## 遗留）+ 具体性要求（按 design.md D5 文稿）
- [x] 4.2 `tools.rs` subagent schema description 增补：spawn prompt 须自包含（背景/目标/涉及文件/验收标准，子代理看不到对话历史）
- [x] 4.3 测试：默认 prompt 含四段标题与受众声明；schema description 含自包含引导

## 5. 回归与收尾

- [x] 5.1 全量 `cargo test`（src-tauri）通过
- [x] 5.2 `openspec validate subagent-settlement` 通过
- [ ] 5.3 人工冒烟：spawn 一个会失败的子代理（如读不存在文件后仍要继续的任务）→ 主上下文收到「失败」+原因；jobs 面板卡片同步
