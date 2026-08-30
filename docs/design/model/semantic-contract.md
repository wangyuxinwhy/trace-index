---
title: "Trace Index Item 语义契约"
description: "定义 SemanticRole、每种 SemanticValue 的完整字段、角色专属目标引用，以及 structural 与 heuristic 的选择规则。"
---

# Trace Index Item 语义契约

> [!IMPORTANT]
> 不同 Runtime 会用不同结构记录同一种 Agent 行为。Item 的 `Semantic` 表示把这些原生差异转换成 Trace Index 可以统一查询的语义。其中，`role` 说明 Item 在 Agent 程序中表示什么，`value` 保存这种语义下可以一致读取的字段，`evidence_strength` 说明整项判断来自结构证据还是启发式规则。

> [!NOTE]
> 本页是 Trace Index 当前生效、仍会继续迭代的领域契约。当前实现应当遵守它，但它不是不可修改的终局设计。实现和真实 Trace 如果出现现有角色无法准确表达的稳定语义，或者证明现有 `SemanticRole`、`SemanticValue` 结构不够清晰，可以修改本契约。修改时应先说明新的领域含义和必要性，再同步角色、Value 类型、约束、示例以及相应的 Adapter 代码与测试。单个 Runtime 的字段或版本差异仍由 Adapter 处理，不会仅因实现变化自动成为领域语义。

[Item 领域模型](/design/model/item) 定义 Item、`Semantic` 与 `record_ids` 的完整关系。本页定义 Trace Index 当前承认的 `SemanticRole`、每个角色的判断含义，以及对应 `SemanticValue` 的字段。Runtime 的原生 `kind/value` 和具体 Adapter 映射不属于本页；尚未形成稳定语义的原始内容继续由 Record 保存。

## Semantic 是判别联合

```text
Semantic {
  role: SemanticRole
  value: SemanticValue
  evidence_strength: structural | heuristic
}
```

每个 Item 恰好有一个 `semantic` 字段。`Semantic` 是判别联合，也就是说，先由 `semantic.role` 确定语义类别，再由这个类别确定 `semantic.value` 必须使用哪一种结构。两者不能独立组合。例如，`role=tool.output` 时，`value` 必须是 `ToolOutput`，不能改用 `ToolCall`。本页类型中的 `?` 表示字段可以不存在。

`role` 采用分层名称。前两段是 `<producer>.<purpose>`，例如 `human.request`、`agent.final_answer` 和 `tool.output`；已经有真实跨 Runtime 语义需要进一步区分时，可以增加第三段 subtype，例如 `agent.tool_call.shell`。subtype 细化的是同一程序用途下的稳定语义类型，不是 Runtime 工具名、原生事件名或根据内容猜出的意图。

判断遵守以下规则：

- 只依据 Runtime 明示结构、结构关系或已确认的回退规则，不根据文本主题猜测。
- Runtime 原生的 `role=user`、`role=assistant`、通道名称或事件类型只参与判断，不能直接照抄为 `semantic.role`。
- 符合同一语义定义的 Item，无论来自哪个 Runtime，都使用同一个角色。
- `human.request` 与 `human.steering` 只依据人的输入和 Loop 的结构关系区分：建立新 Loop 的输入是 Request，进入正在运行的 Loop 的输入是 Steering。
- 只有当前契约已经定义、且证据足以支持的语义事实才形成 Item。证据不足时保留原始 Record，不为了容纳不确定分类增加宽泛的兜底角色。
- 只有能够确认由 Runtime 产生、具有独立查询价值的有界内容，才可以在用途尚不能可靠归入其他角色时使用 `runtime.unknown`。它的可查询内容使用 `Text`，其余原始事实继续由 `record_ids` 指向的 Record 保存；`runtime.unknown` 不是容纳任意 Runtime JSON 的口袋。
- `value` 中无法可靠取得的可选字段直接省略，不能从输出文字或字段名称猜测补齐。

## SemanticValue 类型

### TextContent

所有可能较长的语义文字都使用同一种有界表示，而不是裸 `string`：

```text
TextContent {
  value: string
  full_bytes: uint64
  estimated_tokens: uint64
}
```

`value` 是 Trace Index 当前发布的文字，必要时可以只保留完整文字的前缀。`full_bytes` 是 Trace Index 施加长度限制前、Source 中完整文字的 UTF-8 字节数。比较 `value` 的 UTF-8 字节数与 `full_bytes`，就能判断当前值是否完整：相等表示完整，小于则表示只发布了前缀。`estimated_tokens` 是对同一份完整 Source 文字的统一规模估算，便于 Agent 在展开内容前判断成本；它不是模型供应商的计费 token。具体估算算法和当前长度上限由代码仓库维护。

这个类型解决的是 Trace Index 自己的输出边界。Runtime 在写入 Source 之前是否已经截断内容，是另一项事实，不能通过 `TextContent` 反推。

### Text

```text
Text {
  text?: TextContent
  has_images: boolean
}
```

`text` 是可以跨 Runtime 读取的文字内容。Item 只有图片或 Runtime 没有提供可读文字时，省略 `text`。`has_images` 始终存在，只表示是否包含至少一项图片。图片的原始表示仍由 `record_ids` 指向的 Record 保存。

### Reasoning

```text
Reasoning {
  representation: full | summary | unavailable
  text?: TextContent
}
```

`full` 表示 `text` 是 Runtime 提供的完整推理文字，`summary` 表示 `text` 是 Runtime 明确标记的推理摘要。这两种取值都要求 `text` 存在且非空。`unavailable` 表示 Runtime 只证明推理发生但不暴露内容，此时省略 `text`。推理内容不可用不影响 Item 成立。

### ToolCall 与 ShellToolCall

```text
ToolCall {
  tool_name?: string
  arguments?: RuntimeValue
  working_directory?: string
}

ShellToolCall extends ToolCall {
  shell_fragments: ShellFragment[1..*]
}
```

`ToolCall` 保存所有工具调用共有的语义字段。`tool_name` 表示 Runtime 明示的工具或函数名称，`arguments` 保存能够可靠投影的结构化参数，`working_directory` 只在 Runtime 明示调用工作目录时出现。当前契约不保存调用状态：是否已经观察到输出可以通过独立的 `tool.output` Item 判断，缺少输出并不能证明调用仍在等待或已经完成。

`ShellToolCall` 是 `ToolCall` 的特化类型。它保留 `tool_name`、`arguments` 和 `working_directory`，并额外增加 `shell_fragments`。`arguments` 仍表示 Runtime 传给工具的参数；`shell_fragments` 表示 Trace Index 从这次调用中建立的跨 Runtime Shell 结构。两者不能互相替代，也不能把 `shell_fragments` 塞进 `arguments`。

```text
ShellFragment {
  text: TextContent
  completeness: complete | partial
  statements: ShellStatement[0..*]
}

ShellStatement {
  range: ByteRange
  parent_position?: uint64
  connector?: string
  pipeline?: {
    id: uint64
    position: uint64
  }
  invocations: Invocation[0..*]
  redirects: Redirect[0..*]
}

Invocation {
  program: string
  argv: string[]
}

Redirect {
  source_fd?: string
  operator: string
  target: string
  range: ByteRange
}

ByteRange {
  start: uint64
  end: uint64
}
```

`ShellFragment.text` 保存这段 Shell 程序的有界可读文字。`completeness=complete` 表示 `statements` 对完整 Shell 片段的结构解析已经完成，`partial` 表示只能可靠建立部分结构；它描述解析覆盖，不描述文字是否因 Trace Index 的输出上限而只发布了前缀。文字发布是否完整仍由 `TextContent.value` 与 `full_bytes` 判断。`statements` 按片段中的观察顺序排列，数组位置就是 `parent_position` 使用的局部位置。`range` 使用左闭右开的字节范围，并且只相对于所属 Shell 片段。

`connector` 表示该 Statement 与前一 Statement 的 Shell 连接方式。`pipeline` 只在 Statement 属于管道时出现；`id` 只在当前 Fragment 内区分不同管道，`position` 表示所在管道中的位置。`Invocation` 保存 Agent 写下的程序和 Shell 词序列，不声称该程序一定执行。`Redirect` 保存无法从 Invocation 参数可靠推出的重定向结构。

只有 Runtime 的结构能够确认这是一项 Shell 工具调用，并且能够形成至少一项 `ShellFragment` 时，才使用 `agent.tool_call.shell` 和 `ShellToolCall`。不能因为参数文字看起来像命令，或者工具名称碰巧包含 shell、bash、exec 等词，就进行分类。Shell 中写下 `cat file` 仍然是 `agent.tool_call.shell`，不能根据命令意图改成尚未定义的 read subtype。

`completeness` 与 `semantic.evidence_strength` 回答不同问题：前者说明 Shell 结构是否完整，后者说明这项 Semantic 分类使用了结构依据还是启发式依据。确定性解析只得到部分结构时，`completeness` 是 `partial`，但不会仅因此把结构分类降为 `heuristic`。

Runtime 用来关联调用和输出的原生调用 ID 仍可在来源 Record 中核查。跨 Runtime 的语义引用使用 Item 自己的 `item_id`，因此 `ToolCall` 和 `ShellToolCall` 都不重复保存一项只能在单个 Runtime 内解释的 `call_id`。

### ToolOutput

```text
ToolOutput {
  call_item_id?: ItemId
  text?: TextContent
  exit_code?: int64
  failed?: boolean
  duration_ms?: uint64
  runtime_truncated?: boolean
  runtime_output_tokens?: uint64
}
```

对应的 ToolCall Item 已经进入当前投影并能够可靠解析时，`call_item_id` 直接指向该 Item。Runtime 只提供原生调用引用、当前还不能解析目标 Item 时，省略 `call_item_id`；原生引用仍由来源 Record 保存。

`call_item_id` 是解析后的领域引用，不是来源 Record 单独携带的值。来源 Item 的 `record_ids` 必须提供原生调用关系，被引用的 ToolCall Item 必须存在于当前投影，映射规则才能把两者解析为 `call_item_id`。

`exit_code`、`failed`、`duration_ms`、`runtime_truncated` 和 `runtime_output_tokens` 都只保存 Runtime 实际报告、并且能够可靠归属于这次输出的事实。`failed` 不能通过搜索 `error`、`failed` 等输出文字推断；缺失的 `duration_ms` 也不是 `0`。

`runtime_truncated` 与 `TextContent` 描述两个先后发生的边界。前者表示 Runtime 在把工具输出写入 Source 之前是否已经截断原始输出；后者表示 Trace Index 是否又为了有界查询而只发布了 Source 中完整文字的前缀。`runtime_output_tokens` 是 Runtime 对原始工具输出给出的规模测量，`TextContent.estimated_tokens` 则描述 Source 中实际存在的完整文字。两者不能相加，也不能互相替代。

### Delegation

```text
Delegation {
  text?: TextContent
  has_images: boolean
  child_session_id?: SessionId
}
```

`Delegation` 表示父 Agent 交给子 Agent 的任务。`child_session_id` 在被委派的子 Session 已经进入当前投影并能够可靠解析时出现。只有原生子 Agent 引用、还不能解析目标 Session 时，省略 `child_session_id`；原生引用仍由来源 Record 保存。

### SubagentActivity

```text
SubagentActivity {
  text?: TextContent
  has_images: boolean
  subagent_session_id?: SessionId
}
```

`SubagentActivity` 表示子 Agent 工作期间的进度或 Agent 间通信。`subagent_session_id` 指出这项活动描述哪个子 Session。Item 自己的顶层 `session_id` 仍表示这项记录发生在哪个 Session，两者不能混淆。

### SubagentReport

```text
SubagentReport {
  text?: TextContent
  has_images: boolean
  source_session_id?: SessionId
}
```

`SubagentReport` 表示子 Agent 交回的工作结果。`source_session_id` 指出报告来自哪个子 Session。目标 Session 还不能可靠解析时省略该字段，不能根据报告文字或 Agent 名称猜测。

三个 Session 引用与 `call_item_id` 遵守同一条规则：来源 Item 的 `record_ids` 提供原生关系证据，目标 Session 存在于当前投影，映射规则再建立解析后的领域引用。目标 Session 自己的 Record 不需要复制进来源 Item 的 `record_ids`。

### Compaction

```text
Compaction {
  summary?: TextContent
}
```

`summary` 是 Runtime 用来替代较早上下文的摘要文字。Runtime 只记录发生了压缩、没有保存摘要时，省略该字段。

### Instruction

```text
Instruction {
  text?: TextContent
  category?:
    project
    | user
    | skill
    | permission
    | collaboration
    | plugin
    | tool_catalog
}
```

`category` 描述已经能够确定、并且真实查询需要区分的指令主题，不描述 Runtime 使用的标签、文件名或消息通道；无法归入当前已定义主题时省略。指令出现在哪个 Session 或 Loop 已由 Item 自己的归属表达，不再增加另一套 `scope`。`text` 只在指令文字可以统一读取时出现。

### Context

```text
Context {
  text?: TextContent
  category?:
    environment
    | memory
    | file
    | session_reference
    | internal
  has_images: boolean
}
```

Context 向 Agent 提供可使用的事实或材料，但不要求它怎样行动。`category` 描述已经能够确定的材料含义；无法归入当前已定义主题时省略。IDE 选区、附件标签或文件注入通道等原生机制仍可在来源 Record 中核查，不复制进 Item。图片是否存在只由 `has_images` 表达，不再重复建立 `image` 类别。

## 人的输入

| `semantic.role` | Value 类型 | 精确定义 |
| --- | --- | --- |
| `human.request` | `Text` | 能够确认由人产生，并被 Runtime 用来建立一个新 Agent Loop 的输入 |
| `human.steering` | `Text` | 能够确认由人产生，并被 Runtime 加入一个仍在运行的 Agent Loop 的输入 |

`human.request` 与 `human.steering` 的差别只来自输入和 Loop 的结构关系。输入建立新 Loop 时是 `human.request`；输入到达时原 Loop 尚未结束，并被 Runtime 加入该 Loop 时是 `human.steering`。`completed`、`interrupted` 或 `failed` 的 Loop 都已经结束，之后的输入如果建立新 Loop，就是 `human.request`，不能因为它出现在中断之后或文字看起来像“继续”而推断成 `human.steering`。

这项分类不判断自然语言意图。人在运行中的 Loop 里提出看似全新的要求，结构上仍是 `human.steering`；人在前一 Loop 结束后提交补充或纠正，结构上仍是 `human.request`。两种角色都继续使用 `Text`，因为 `semantic.role` 已经表达输入与 Loop 的关系，不再在 Value 中重复保存另一项判别字段。

SDK、API 或名为 `user` 的通道也可能由程序自动写入，因此通道名称不能证明输入来自人类。只有来源证据支持人的身份，并且 Runtime 明示结构或结构关系支持输入的 Loop 归属时，才能建立对应的 `human.*` 角色；证据不足时不能根据文本、相邻位置或前一 Loop 的结果补造分类。当前契约不为这种未解决情况建立一个宽泛的输入 Item：原始内容仍由 Record 保留，等真实需求证明需要稳定查询这种事实时，再设计最小的新语义。

`human.request` 和 `human.steering` 都必须有 `loop_id`，因为角色本身已经声称其与某个外层 Loop 的关系已知。

## Agent 产生的事实

| `semantic.role` | Value 类型 | 精确定义 |
| --- | --- | --- |
| `agent.commentary` | `Text` | Agent 在本次 Loop 尚未结束、之后仍会继续推理或调用工具时，向调用者输出的说明、进度或中间文字 |
| `agent.final_answer` | `Text` | Runtime 结构认定为结束本次 Loop，并向调用者输出的 Agent 文字 |
| `agent.reasoning` | `Reasoning` | Runtime 单独持久化的模型推理活动、推理摘要或其存在性 |
| `agent.tool_call` | `ToolCall` | Agent 发起的一次工具或函数调用，但当前稳定语义不能进一步归入已经定义的 subtype |
| `agent.tool_call.shell` | `ShellToolCall` | Agent 发起的一次由 Runtime 结构确认、并具有可投影 Shell 程序的工具调用 |
| `agent.delegation` | `Delegation` | 父 Agent 交给子 Session 的任务或工作指令 |

`agent.final_answer` 不表示答案正确、Task 已完成或用户满意。没有外层 Loop 结束依据的“最后一条 assistant 消息”不能自动归为最终回答。只有 Runtime 结构证明文字在执行期间发出时，才使用 `agent.commentary`；只有 Runtime 结构把文字作为本轮最终回答时，才使用 `agent.final_answer`。无法可靠区分时，原始 Record 仍然保留，不增加含义宽泛的兜底 Item。

`agent.commentary` 和 `agent.final_answer` 都必须有 `loop_id`。

`agent.tool_call.shell` 是 `agent.tool_call` 的语义 subtype，不是另一种生产者或另一项 Item。调用者查询所有工具调用时可以同时选择通用角色和已经定义的 subtype；只查询 Shell 调用时直接选择 `agent.tool_call.shell`。未来只有真实跨 Runtime 查询证明另一类工具调用需要稳定结构时，才增加新的 subtype。

## 工具与子 Agent

| `semantic.role` | Value 类型 | 精确定义 |
| --- | --- | --- |
| `tool.output` | `ToolOutput` | 一次工具调用产生的进度、结果或错误输出 |
| `subagent.activity` | `SubagentActivity` | 委派工作中的活动或 Agent 间通信，但不是启动任务本身，也不是交回父 Agent 的结果 |
| `subagent.report` | `SubagentReport` | 子 Agent 将工作结果交回启动它的父 Agent 或主对话 |

当工具调用和输出都被选择形成 Item 时，它们是两项独立 Item。输出通过自己的 `ToolOutput.call_item_id` 引用调用。查询一次调用的全部输出时，反向查找 `call_item_id` 等于该调用 Item ID 的 `tool.output`。`ToolCall` 不复制输出列表。

委派、活动和报告也由各自的 Value 保存目标 Session：`Delegation.child_session_id`、`SubagentActivity.subagent_session_id` 和 `SubagentReport.source_session_id`。这些引用是该 Item 语义的一部分，没有独立身份或生命周期，因此不再建立单独的 Item 关系对象。

可以按工作阶段区分三个子 Agent 角色：父 Agent 交出任务时是 `agent.delegation`，子 Agent 工作期间的进度或通信是 `subagent.activity`，子 Agent 把结果交回父 Agent 或主对话时是 `subagent.report`。三者描述不同的程序事实，不能因为文字相似而合并。

## Runtime 产生或注入的事实

这些角色表示 Runtime 自己产生、维护或注入的程序事实。名称描述语义，不绑定任何 Runtime。任何 Runtime 只要产生符合定义的 Item，都使用相同角色。

| `semantic.role` | Value 类型 | 精确定义 |
| --- | --- | --- |
| `runtime.instructions` | `Instruction` | Runtime 施加给 Agent 的行为要求或约束 |
| `runtime.context` | `Context` | Runtime 放入 Agent 上下文、供其使用但不直接约束行动的事实或材料 |
| `runtime.state` | `Text` | Runtime 告知 Agent 的当前工作区或执行状态；只有形成稳定语义的可读内容进入 Item |
| `runtime.notice` | `Text` | Runtime 主动发送给 Agent 的错误、提醒或一般通知 |
| `runtime.compaction_summary` | `Compaction` | Runtime 用来替代已从活动上下文移除内容的压缩摘要 |
| `runtime.unknown` | `Text` | 能够确认由 Runtime 产生或注入、具有独立查询价值，但用途尚不能可靠归入其他角色的事实 |

例如，要求 Agent 遵守某项规则的文字属于 `runtime.instructions`。Runtime 注入 Agent 上下文、供其阅读的文件内容属于 `runtime.context`。工作区或执行模式的变化属于 `runtime.state`。Runtime 主动发出的错误或提醒属于 `runtime.notice`。如果文件内容是 Agent 主动调用读取工具后得到的结果，它仍可能属于 `tool.output`。这些例子只说明角色边界，具体 Runtime 的原生标签仍由 Adapter 解释。

只用来建立 Session 或 Loop 的开始、结束或结果字段的 Runtime 记账记录，不再额外形成 `runtime.lifecycle` Item。相同事实已经由对应领域实体表达，再建立一项生命周期 Item 只会制造重复事实。记账记录仍作为 Record 保留；它同时携带独立的状态、通知或其他程序事实时，只为那项独立事实建立相应 Item。

原生图片附件、IDE 选区、斜杠命令、外部 shell 命令、Hook 输出和文件变更都不是独立 `SemanticRole`。当它们能够形成稳定、值得查询的程序语义时，按用途投影为输入、指令、上下文、状态、通知、工具输出或未知；没有稳定语义的原始表示只保留在 Record 中。

## Role 与 Value 配对

| Value 类型 | 允许的 `semantic.role` |
| --- | --- |
| `Reasoning` | `agent.reasoning` |
| `ToolCall` | `agent.tool_call` |
| `ShellToolCall` | `agent.tool_call.shell` |
| `ToolOutput` | `tool.output` |
| `Delegation` | `agent.delegation` |
| `SubagentActivity` | `subagent.activity` |
| `SubagentReport` | `subagent.report` |
| `Compaction` | `runtime.compaction_summary` |
| `Instruction` | `runtime.instructions` |
| `Context` | `runtime.context` |
| `Text` | 除上述专用 Value 外的其余当前已定义角色 |

同一 Value 类型可以由多个角色复用，但角色不能选择另一种 Value 类型。Runtime Record 中存在额外字段，不会自动扩大 `SemanticValue`。只有 Trace Index 领域模型定义了稳定字段后，Adapter 才能把它写入 `semantic.value`。

## EvidenceStrength

`semantic.evidence_strength` 描述建立这项 Semantic 分类时使用的最弱依据：

- `structural`：Runtime 的原生类型、身份、关系或程序结构直接支持这项分类。
- `heuristic`：分类依赖文字前缀、位置惯例或其他可能随 Runtime 变化的回退规则。

例如，Runtime 明确记录一项原生事件是工具调用时，对应语义分类可以是 `structural`。如果只能根据文字前缀或位置惯例判断它是工具调用，分类就是 `heuristic`。

这里不区分 Runtime 明示的信息和从 Runtime 结构中确定性推出的信息。两者对分类都属于 `structural`。当前模型也不增加字段级强度、`mixed` 或数值置信度。`Semantic.value` 中的可选事实仍按各自字段的成立条件填写；`evidence_strength` 不声称为每个字段另外保存了一份证明。精确依据名称及其版本适配由 Adapter 实现维护。

新增角色只有在现有词表无法表达已经真实出现、并且调用者需要跨 Runtime 查询的程序语义时才成立。某个 Runtime 新增原生 `kind` 本身不是增加语义角色的理由。已有角色的含义和 Value 类型不能被悄悄改写。

本页不维护 Codex、Claude Code 或 Pi 的字段对照，也不说明某个 Runtime 版本怎样识别角色。Adapter 只需要满足结果约束：符合定义的事实使用对应角色，无法可靠取得的可选字段保持不存在，Item 通过 `record_ids` 返回物理 Record。工具调用的输出不复制进 `ToolCall`。它保持独立的 `tool.output` Item，并在目标可以解析时填写 `call_item_id`。
