---
title: "Trace Index 投影模型"
description: "定义物理事实如何形成当前程序事实、每项事实怎样返回证据，以及输入变化后的有效性边界。"
---

# Trace Index 投影模型

> [!IMPORTANT]
> Trace Index 投影把 Adapter 对原生结构的解释和领域规则应用于 Source 与 Record，从而建立 Session、Loop、Item 及其字段。投影只建立当前物理证据能够支持的程序事实；每项事实都必须能够返回支撑它的 Record 和 Source。输入与结果完整发布后，才成为当前投影。

[Trace Index 领域模型](/design/domain-model) 定义五个核心实体，各实体模型定义自己的字段、身份、边界和不变量。本页定义这些实体共同遵守的投影规则：物理事实怎样形成程序事实，程序事实怎样返回证据，以及输入变化后哪些事实仍然属于当前投影。

## 投影连接物理事实和程序事实

Runtime 持久化的是格式各异的原生 Trace。Source 和 Record 忠实描述这些物理输入，Session、Loop 和 Item 则描述能够由这些输入确定的程序结构。Adapter 负责解释一种 Runtime 的原生结构；投影负责按照领域规则把这种解释建立为共同程序事实。

```text
Source
  └─ Record
       + Adapter 提供的原生结构解释
       + 领域规则
       ↓
Session / Loop / Item 及其字段
```

面对同一组有效 Source、Record、Adapter 解释规则和领域约束，投影应当得到相同的程序事实。投影不根据文本内容猜测任务、意图、偏好或因果，也不以复制一份完整的 Runtime 事件清单为目标。

## 跨 Source 组合只能依赖已有领域证据

多份 Source 是否支撑同一个 Session，只由同一 Runtime 上下文身份以及各 Source 中的身份 Record 决定，不能根据文件发现顺序、目录相邻、时间接近或内容相似度猜测。`forked_from` 和 `delegated_from` 不参与合并 Session；它们只在两个 Session 已经分别建立后，表达两个不同 Session 之间的历史派生或委派来源。

每份参与程序投影的 Source 只支撑一个 Session；一个 Session 可以由一份或多份 Source 支撑。这项关系由 Session 的 `source_ids` 表达，不需要增加独立的通用关系对象。具体实现可以采用不同处理顺序，只要最终结果满足这些领域约束。

## Record 与程序事实不是一一对应

Record 的边界来自物理格式，Item 的边界来自可查询的语义事实，因此二者允许多对多：

- 一条 Record 可以不形成 Item，例如它只提供 Session 身份或 Loop 边界，或者没有独立查询价值。
- 一条 Record 可以形成多个 Item，例如一条原生事件中包含多个可以独立引用的内容块。
- 每个 Item 必须由一条或多条 Record 支撑，多条 Record 可以共同支撑同一个 Item，例如 Runtime 重复记录了同一次程序活动。
- 一条 Record 可以同时支撑 Session、Loop、Item 或它们的多个字段。

Record 是否形成 Item、形成几个 Item，以及多条 Record 是否共同描述同一次程序活动，由原生结构和相应实体模型共同决定。Record 不会因为形成程序事实而被修改或消耗。

## 每项程序事实都要保留物理依据

证据回答“这项程序事实由哪些物理记录支持”。当前领域模型不定义独立的 Evidence 实体；证据由拥有该事实的现有字段表达：

| 程序事实 | 物理依据 |
| --- | --- |
| Session 身份 | `identity_record_id` |
| Session 的物理来源 | `source_ids` |
| Session 的历史派生 | `forked_from.record_id` |
| Session 的委派来源 | `delegated_from.record_id` |
| Loop 建立 | `start_record_id` |
| Loop 结束 | `end.record_id` |
| Item | `record_ids` |

一项程序事实最终必须能够返回当前 Record，并通过 Record 的 `source_id` 返回当前 Source。只有 Source 而没有相应 Record，不能证明某次 Session、Loop 或 Item 活动确实发生。

Record 与 Source 回答证据位于哪里；一个字段为什么能够由这些证据建立，由对应实体契约和 Adapter 的可复现解释共同回答，不因此增加通用 Evidence 字段。Item 的物理来源仍然是 `record_ids`；`semantic.evidence_strength` 只区分语义判断采用结构依据还是启发式规则，不是另一项物理依据。

证据不是排他的所有权。同一条 Record 可以支持多项事实，多条 Record 也可以共同支持同一项事实。失去其中一条 Record 时，只有剩余证据不再满足相应实体模型的成立条件，该事实才退出当前投影。

Record 是当前领域模型的证据定位单位。更细的 Runtime 结构位置属于 Adapter 与接口实现，不进入共同领域契约。

## 投影只建立证据能够确定的内容

实体身份、Loop 边界和归属只有在相应实体模型的证据条件满足时才能建立。Item 的 `semantic.role` 和 `semantic.value` 还可以使用 Item 语义契约允许的启发式规则，但必须标记 `semantic.evidence_strength = heuristic`。来源不足时，只保留已经确认的物理事实和程序事实，不用整齐的默认值补造未知内容。

例如：

- 缺少 Loop 结束证据时，Loop 不能被填成成功结束。
- 原生父级引用尚未解析到目标 Session 时，引用继续留在 Record 中，Session 暂不建立对应父级属性。
- Runtime 原生类型未知时，Record 仍然成立，但不建立缺少依据的 Item。

某个字段只有在真实需求证明必须区分 `unknown`、`unresolved` 或 `conflict` 时，才由拥有该字段的实体模型定义相应状态。投影模型不预先为所有事实设计一套通用缺失或冲突状态。

新证据可以补全一项事实的可选字段或物理依据。如果新证据证明原来的实体边界错误，应当让旧对象退出当前投影，并按正确边界建立对象；不能让已经存在的 `session_id`、`loop_id` 或 `item_id` 悄悄改指另一件事。

## 当前投影只包含当前证据支持的事实

“当前投影”是已经完整发布、可以查询，并且仍由当前 Source、Record、Adapter 解释规则和领域约束支持的程序事实。它不表示历史上只发生过这些事实，也不等于 Runtime 文件此刻尚未同步的最新内容。

Source、Record、Adapter 解释规则或领域约束发生变化时，只重新建立实际受影响的 Session、Loop、Item 及其字段。仍然满足自身最低证据要求的实体可以保留，不再满足要求的实体退出当前投影。怎样计算受影响集合和执行增量重建属于代码实现。

新结果只有满足下面三个条件，才能替换受影响的旧事实：

1. 所有程序事实仍有满足自身模型要求的当前 Record 和 Source 证据。
2. 已经失效的 Source 或 Record 不再支撑新结果。
3. 受影响范围内的实体、字段和目标引用彼此一致，被引用的目标存在于当前投影。

满足这些条件的候选结果还必须完整发布，才能成为当前投影。查询怎样只看到替换前或替换后的完整结果，由 [Trace Index 发布与一致性](/design/index-consistency) 定义。

当前模型不提供历史投影或投影版本。

## 页面边界

本页不定义五个实体各自的字段，不规定某个 Runtime 的原生字段路径，也不设计数据库表、公共证据对象、失效依赖表或增量重建算法。具体 Runtime 解释、证据存储和重建机制由代码与测试维护；实体特有的证据要求由相应领域模型定义。

继续阅读 [Trace Index 发布与一致性](/design/index-consistency)，理解候选投影怎样成为对查询者原子可见的当前结果。
