---
title: "Trace Index Item 领域模型"
description: "定义 Item 的完整字段、Semantic 与 Record evidence 的分工，以及身份、归属、顺序和数量约束。"
---

# Trace Index Item 领域模型

> [!IMPORTANT]
> Item 是 Trace Index 从原始 Record 中选择出来、值得被单独查询的一项 Agent 程序事实，例如一次人的输入、一次 Agent 输出、一次工具调用或一次工具输出。Item 只保存已经能够稳定解释的 `semantic`，并用 `record_ids` 指回支撑这项解释的原始 Record。Runtime 中尚未形成稳定语义的内容仍然留在 Record 中，不会为了“也许以后有用”而复制进 Item。

[Trace Index 领域模型](/design/domain-model) 定义 Source、Record、Session、Loop 和 Item 的分工。本页定义 Item 自身的字段、身份、归属、物理来源和不变量。[Item 语义契约](/design/model/semantic-contract) 继续定义 `Semantic.role`、每种 `Semantic.value` 的完整结构以及判断强度。

## Item 的完整形状

```text
Item {
  item_id: ItemId
  session_id: SessionId
  loop_id?: LoopId
  loop_position?: uint64
  occurred_at?: Instant

  record_ids: RecordId[1..*]
  semantic: Semantic
}

Semantic {
  role: SemanticRole
  value: SemanticValue
  evidence_strength: structural | heuristic
}
```

`?` 表示字段可以不存在，`[1..*]` 表示数组至少包含一项。这个结构定义调用方可以依赖的领域契约，不要求 SQLite 必须用同样的列或嵌套方式保存它。

## 字段说明

| 字段 | 是否必须 | 含义 |
| --- | --- | --- |
| `item_id` | 必须 | Trace Index 分配的 Item 身份，用于查询、引用和关联 |
| `session_id` | 必须 | 这项程序事实属于哪个逻辑 Session |
| `loop_id` | 可选 | 这项事实属于哪个外层执行 Loop；只在归属能够确定时出现 |
| `loop_position` | 可选 | Item 在同一 Loop 内的零起始观察顺序；只有 `loop_id` 存在时才能出现 |
| `occurred_at` | 可选 | Runtime 为这项事实提供或支持的发生时间 |
| `record_ids` | 必须 | 支撑这项 Item 的一个或多个物理 Record；不能为空 |
| `semantic` | 必须 | Trace Index 对这项事实给出的稳定、可查询解释 |
| `semantic.role` | 必须 | 说明这项事实属于哪一种程序语义，并决定 `value` 的类型 |
| `semantic.value` | 必须 | 由 `role` 选择的类型化字段，不是任意 JSON 口袋 |
| `semantic.evidence_strength` | 必须 | 表明整项语义判断由结构证据还是较弱的启发式规则支持 |

## Item 的信息边界

Item 的目的不是制作一份更方便读取的原始 Trace 副本。它只物化正常查询真正需要的事实：

- 能够稳定解释、值得过滤、聚合、关联或阅读的内容进入 `semantic`。
- 需要作为语义正文读取的文字使用 `TextContent`；存储层可以对它做有界发布和内容去重。
- Runtime 的外层 envelope、重复历史、尚未建模的长尾字段和只用于协议排查的细节留在 Record 中。
- 调用方确实需要原始 Runtime 表示时，沿 `record_ids → Record → Source` 回到原始字节。

因此 Item 的稳定表达是 `Semantic + Record evidence`：前者承载已经承诺可以解释的事实，后者保留返回完整来源的路径。Adapter 仍然会读取 Runtime 原生结构，但解析时使用的 payload 继续属于 Record，不会因此自动成为 Item 字段。

## Semantic 与 Record evidence 的分工

`semantic` 和 `record_ids` 回答两个不同问题：

- `semantic` 回答“Trace Index 认为这项程序事实是什么，以及调用方可以稳定读取哪些字段”。
- `record_ids` 回答“这项判断由哪些原始记录支持，必要时到哪里核查”。

两者缺一不可。只有 Record 而没有稳定语义时，那条物理记录仍是有效 Record，但不会形成 Item。只有语义而没有 Record 来源时，则无法验证这项事实，不允许形成 Item。

一项 Item 可以引用多个 Record。例如 Runtime 可能把一次工具调用的主体、结束状态和耗时分别写在不同 Record 中；如果这些 Record 共同支持一项 Tool Output，`record_ids` 就列出全部必要来源。数组顺序不表示因果或优先级。

## Record 没有对应 Item 是正常情况

并非每条完整 Record 都应该产生 Item。以下情况只保留 Record 通常更准确：

- Record 只是建立 Source、Session 或 Loop 边界的记账信息，对应事实已经由其他领域对象表达。
- Runtime 写下了内容，但目前无法可靠判断其程序用途。
- 内容只是重复的历史、外层 envelope 或低价值状态快照，常规查询并不需要复制它。
- Record 只有格式标签或附件记账，没有可以稳定发布的语义内容。

这不表示索引失败或数据丢失。Record 仍然保留来源位置、字节范围和完整性信息；以后真实查询证明某类事实值得稳定支持时，可以增加最小 Semantic 类型并重新构建索引。

## 身份与数量关系

`item_id` 表示一次具体发生的程序事实，不由内容哈希决定。两次内容完全相同的人类输入、工具调用或 Agent 输出，仍然是两项不同 Item，因为它们发生在不同位置或时刻。

每项 Item：

- 恰好属于一个 Session。
- 至多属于一个 Loop。
- 恰好有一个 Semantic。
- 至少引用一个 Record。

一个 Record 可以支持零项、一项或多项 Item。一个 Session 或 Loop 可以包含任意数量的 Item。

## Loop 归属与顺序

`loop_id` 只在 Trace 能够支持外层执行归属时出现。Session 级指令、上下文或通知可能发生在两个 Loop 之间，因此允许只有 `session_id` 而没有 `loop_id`。

当 `loop_id` 存在时，`loop_position` 表示该 Loop 内的观察顺序：

- 从 `0` 开始。
- 同一 Loop 内不能重复。
- 只表示时间线位置，不表示因果、父子或继承关系。
- 不能拿不同 Loop 的 `loop_position` 直接比较先后；跨 Loop 应先使用 Loop 的 `session_position`。

`human.request`、`human.steering`、`agent.commentary` 和 `agent.final_answer` 的定义本身依赖 Loop，因此这些角色必须有 `loop_id`。其他角色是否属于 Loop，由真实 Trace 的结构决定。

## 时间

`occurred_at` 表示 Runtime 提供或支持的领域发生时间，不是 Source 被扫描、索引或写入数据库的时间。Runtime 没有可靠时间时省略，不能用文件修改时间或索引时间补造。

同一 Loop 内的主顺序由 `loop_position` 表达。时间戳可能缺失、相同或粒度不足，因此不能代替结构顺序。

## 示例：人的请求

```json
{
  "item_id": 1201,
  "session_id": 41,
  "loop_id": 87,
  "loop_position": 0,
  "occurred_at": 1767225600123,
  "record_ids": [9051],
  "semantic": {
    "role": "human.request",
    "value": {
      "text": {
        "value": "找出执行超过一分钟的工具调用",
        "full_bytes": 45,
        "estimated_tokens": 12
      },
      "has_images": false
    },
    "evidence_strength": "structural"
  }
}
```

这项 Item 表达“人的输入建立了一个新 Loop”。如果调用方还要核对 Runtime 原始消息字段，就检查 `record_ids=[9051]` 指向的 Record，而不是期待 Item 复制整条消息 payload。

## 示例：工具输出

```json
{
  "item_id": 1210,
  "session_id": 41,
  "loop_id": 87,
  "loop_position": 9,
  "record_ids": [9062, 9064],
  "semantic": {
    "role": "tool.output",
    "value": {
      "call_item_id": 1209,
      "text": {
        "value": "completed",
        "full_bytes": 9,
        "estimated_tokens": 2
      },
      "exit_code": 0,
      "duration_ms": 73124,
      "runtime_truncated": false
    },
    "evidence_strength": "structural"
  }
}
```

两个 Record 共同支持输出正文、退出状态和耗时。`call_item_id` 是已经解析的领域引用；如果当前投影无法可靠找到对应 Tool Call，就省略该字段，不能把 Runtime 的原生调用 ID 塞进 Item 作为替代。

## 不变量

1. `record_ids` 必须非空，且每个目标 Record 都真实存在。
2. `semantic.role` 与 `semantic.value` 的类型必须匹配。
3. `loop_position` 只有在 `loop_id` 存在时才能出现。
4. 同一 Loop 中的 `loop_position` 唯一，并按观察顺序递增。
5. `occurred_at` 缺失时保持不存在，不能用索引时间补值。
6. 相同内容在不同位置真实发生两次时，必须形成两项 Item。
7. 无法稳定解释的 Record 不会为了提高“覆盖率”而形成含义宽泛的 Item。
8. 原始 Runtime 细节只能通过 Record 证据核查，不能从 Semantic 缺失字段反向猜测。

## 演进原则

Item 契约从最小、真实有用的字段开始。某个 Runtime 新增字段，不会自动扩张 Item；只有真实查询反复需要、含义能够稳定定义，并且现有 Semantic 无法表达时，才增加最小的新角色或 Value 字段。修改后应同步领域文档、公共接口、Adapter 和契约测试，并通过重新构建验证已有事实没有被意外删除或重复。
