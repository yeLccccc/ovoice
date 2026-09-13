## ADDED Requirements

### Requirement: 结算三态诚实化
子代理结算 SHALL 产生诚实的终态：当 LLM 循环以错误结束时，`answer` SHALL 携带失败原因文本（前缀「失败原因：」），且当部分产出（partial）非空时 SHALL 附加「部分产出：」段；当循环正常结束但最终内容为空白时，`answer` SHALL 为字面量 `(无输出)`；当任务被用户取消时，`answer` SHALL 为部分产出（现状不变）。`JobOutcome.ok` SHALL 准确反映 ok/失败/取消三态。

#### Scenario: 子代理失败时原因与部分产出回传
- **WHEN** 子代理循环以 Err("连接超时") 结束且 partial = "已扫描 80%"
- **THEN** 结算 answer 包含「失败原因：连接超时」与「部分产出：已扫描 80%」
- **AND** JobOutcome.ok == false

#### Scenario: 成功但空产出有字面兜底
- **WHEN** 子代理循环 Ok("") 结束
- **THEN** answer == "(无输出)"，ok == true
- **AND** 主 agent 注入文本不再出现空正文

#### Scenario: 取消路径维持现状
- **WHEN** 子代理被 kill 且未 suppress
- **THEN** answer == partial，note == 被用户终止，ok == false

### Requirement: 结算事件携带状态字段
`HistoryEvent::subagent_result` 的 data SHALL 包含 `ok` 布尔字段。渲染时按 `ok` 输出「完成」或「失败」；**data 缺少 `ok` 的存量事件 SHALL 按 true（完成）处理**，不做历史迁移。

#### Scenario: 失败事件在主上下文渲染为失败
- **WHEN** build_messages 处理 ok=false 的 subagent_result 事件
- **THEN** 注入消息为「[子代理 #N 失败] {summary}」格式

#### Scenario: 存量事件向后兼容
- **WHEN** build_messages 处理无 ok 字段的旧 subagent_result 事件
- **THEN** 按「完成」渲染，与修复前显示一致

### Requirement: 迭代上限配置化
`SUBAGENT_MAX_ITERS` 定值常量 SHALL 删除，新增 config 字段 `subagent_max_iters`（u64，serde 默认 5000，0 表示不限制）；spawn 子代理 SHALL 从该配置读取迭代上限。失控保护 SHALL 继续由螺旋熔断（SPIRAL_LIMIT）承担。

#### Scenario: 默认值覆盖长任务
- **WHEN** config.json 未写 subagent_max_iters
- **THEN** 子代理迭代上限为 5000

#### Scenario: 配置为 0 时不限制
- **WHEN** subagent_max_iters == 0
- **THEN** 子代理循环不受迭代数上限约束（仍受螺旋熔断与中断约束）

### Requirement: 渲染层护栏与真身完整性
落盘层（jsonl summary、registry answer、dream 蒸馏输入）SHALL 永远保存全文，任何截断 SHALL NOT 发生在落盘路径。渲染层（build_messages → 主上下文）SHALL 对 summary 施加 128KB 定值护栏：超限时截断并附显式标记「（已截断，完整产出可用 subagent status 查看）」。

#### Scenario: 正常产出不受护栏影响
- **WHEN** summary ≤ 128KB
- **THEN** 主上下文注入全文，无截断标记

#### Scenario: 病态洪水被护栏拦截
- **WHEN** summary > 128KB
- **THEN** 主上下文注入截断文本 + 显式标记
- **AND** jsonl 落盘、status 工具、dream 蒸馏输入仍为全文

### Requirement: 子代理提示词规范
默认 `subagent_system_prompt` SHALL 包含：受众意识声明（最终总结是主代理唯一能看到的东西）、workspace 归档约定（projects/scripts/datas/render）、动手原则（read 先于盲写、长任务阶段性收敛）、四段结构化输出骨架（## 结论 / ## 改动与产出 / ## 关键发现 / ## 遗留）、具体性要求（精确路径、不贴大段原文）。`subagent` 工具 description SHALL 引导 spawn 的 prompt 自包含（背景/目标/涉及文件/验收标准）。

#### Scenario: 默认提示词包含四段骨架
- **WHEN** config 未自定义 subagent_system_prompt
- **THEN** 默认值包含「## 结论」「## 改动与产出」「## 关键发现」「## 遗留」四段与受众意识声明

#### Scenario: 工具描述引导自包含任务描述
- **WHEN** LLM 查看 subagent 工具 schema
- **THEN** description 含「子代理看不到对话历史，prompt 须自包含」语义的引导
