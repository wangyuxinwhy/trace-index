---
title: "Trace Index Session 领域模型"
description: "定义 Session 的完整字段、身份、Source 证据、延续规则，以及 forked_from 与 delegated_from 两项父级属性。"
---

# Trace Index Session 领域模型

> [!IMPORTANT]
> Session 是 Runtime 赋予身份并持续维护的 Agent 上下文。`session_id` 是 Trace Index 的稳定引用，`native_id` 是 Runtime 为这段上下文提供的原生身份，`source_ids` 指出哪些物理 Source 支撑它。Session 可以包含多个 Loop，也可以在第一轮执行开始前暂时没有 Loop。

[Trace Index 领域模型](/design/domain-model) 从程序结构视角定义 Session、Loop 和 Item。本页定义 Session 的完整字段、身份、延续规则、Source 支撑关系和两项父级属性。

## Session 解决什么领域问题

Runtime 会让后续执行继续使用此前保留的对话、工具结果、压缩结果和运行状态。单独查看一次请求、一条消息或一份文件，不能判断哪些活动共享同一段可恢复上下文。

Session 回答：哪些 Loop 和 Item 共享同一个由 Runtime 持续维护的上下文身份，这段上下文由哪些物理证据支撑，以及新活动是在继续原上下文还是进入一个新的独立上下文。

## Session 与 Task 是不同层次

Task 描述人或 Agent 想完成的目标，Session 描述 Runtime 怎样承载持续上下文。两者没有固定的一一对应关系：一个 Session 可以先后承载多个 Task，一个 Task 也可以跨父子 Session 推进。Trace 通常没有稳定完整的 Task 身份，因此 Task 不属于当前五实体核心模型。

## Session 的完整形状

```text
Session {
  session_id: SessionId
  runtime: RuntimeName
  native_id: string
  identity_record_id: RecordId

  created_at?: Instant
  name?: string
  working_directory?: string

  source_ids: SourceId[1..*]

  forked_from?: {
    session_id: SessionId
    record_id: RecordId
  }

  delegated_from?: {
    session_id: SessionId
    record_id: RecordId
  }
}
```

`?` 表示字段可以不存在，`[1..*]` 表示非空集合。`source_ids` 的集合顺序不表达历史或程序顺序。`forked_from` 和 `delegated_from` 分别内联自己的目标与证据。

| 字段 | 必需性 | 领域含义 |
| --- | --- | --- |
| `session_id` | 必须 | Trace Index 对同一个持续 Agent 上下文的稳定引用 |
| `runtime` | 必须 | 赋予并维护这项上下文身份的 Runtime |
| `native_id` | 必须 | Runtime 为这项持续上下文提供的原生身份字符串 |
| `identity_record_id` | 必须 | 直接记录 `runtime` 与 `native_id` 的一条规范身份 Record |
| `created_at` | 可选 | Runtime 明确记录的上下文建立时间 |
| `name` | 可选 | 人或 Runtime 明确赋予的名称，不是从内容生成的摘要 |
| `working_directory` | 可选 | Runtime 明确记录的 Session 建立时的工作目录；它帮助定位工作上下文，但不参与身份判断 |
| `source_ids` | 必须且非空 | 支撑当前 Session 历史的一份或多份物理 Source。集合顺序不表达历史或程序顺序 |
| `forked_from.session_id` | 存在 `forked_from` 时必须 | 当前 Session 的初始历史从哪个已确定的 Session 派生 |
| `forked_from.record_id` | 存在 `forked_from` 时必须 | 明确记录这项历史派生事实的一条 Record |
| `delegated_from.session_id` | 存在 `delegated_from` 时必须 | 哪个已确定的父 Agent Session 启动当前 Session |
| `delegated_from.record_id` | 存在 `delegated_from` 时必须 | 明确记录这项委派事实的一条 Record |

## Session 的身份

Session 连续性由 Runtime 的上下文身份和持久化归属决定。后续活动复用同一 `native_id`，并被同一 Runtime 归入同一持久化上下文时，它继续同一个 Session。建立能够独立继续的新原生上下文身份时，即使复制全部历史，也形成新 Session。

身份遵守以下规则：

- 同一原生上下文被恢复并继续时，`session_id` 保持不变。
- 创建能够独立继续的新上下文时，形成新的 `session_id`。
- `native_id` 只在同一 `runtime` 内比较。具体 Runtime 从哪项原生结构读取这个身份，由 Adapter 代码和测试负责。
- 内容、标题、时间、工作目录和 Source 位置相同，不能单独合并 Session。
- 没有足够原生身份证据时不建立 Session，不能合成一个“未知身份 Session”。
- `SessionId` 是不透明引用，调用方不能解析它来推断 Runtime、父级或顺序。

关闭界面、进程退出、长时间无活动或 Source 暂时不可读，不会自动创建新 Session 或结束原 Session。

## Session 包含 Loop

一个 Session 包含零到多个 Loop，一个 Loop 只属于一个 Session。

| 方向 | 关系 | 数量约束 |
| --- | --- | --- |
| Session → Loop | 包含 | `0..*` |
| Loop → Session | 归属 | `1` |

Loop 的 `session_id` 是归属事实的规范方向。Session 不复制一个需要单独维护的 Loop ID 列表。Runtime 给出顺序证据时，可以读取同一 Session 中 Loop 的先后，但 Session 不决定 Loop 的开始、结束或结果。

## Session 由 Source 证据支撑

Source 与 Session 是证据支撑关系，不是身份关系。一份 Source 只支撑一个 Session。一个 Session 必须由一份或多份 Source 支撑。

`source_ids` 保存这些 Source 的引用。每份 Source 当前发布的完整 Record 集合构成它为 Session 提供的物理证据。多份 Source 不会因为支撑同一 Session 而失去各自物理身份。同一 Session 内的历史分支和当前路径由 Item 或 Loop 的程序结构表达。

## 两项父级属性

`forked_from` 与 `delegated_from` 是 Session 自己拥有的可选属性，回答不同问题，也可以同时存在：

- `forked_from` 表示当前 Session 的初始历史从哪个 Session 派生。
- `delegated_from` 表示哪个父 Agent Session 启动当前 Session。

每项属性都保存一个已经确定的目标 `session_id`，以及明确记录这项事实的一条 `record_id`。父级仍未解析为当前投影中的 Session 时，不建立这项领域属性；Runtime 留下的原生引用继续保存在原始 Record 中。没有父级证据时也直接省略属性。

父级属性只需要一条精确证据，不使用 Record range，也不保存最后一条 Record。范围表达一段物理内容，最后一条 Record 会随 Session 增长，而父级事实通常由 Session 建立时的一条结构化 Record 直接确定。

继承的消息和工具活动没有在新 Session 中再次发生。`forked_from` 只表达 Session 级历史来源，不枚举新 Session 能看到的每个旧 Item。

## Session 没有共同的结束状态

Session 表达可以持续和恢复的上下文，不表达一次执行的结束结果。一个 Loop 正常完成、失败或中断，不会结束 Session。Task 完成也不妨碍同一上下文以后被恢复。

当前真实 Runtime 没有提供“上下文永久终止且此后不可恢复”的共同领域事实。文件停止增长、最后活动时间、归档、清理或 Source 离开发现范围，都不能被推断为 Session 结束。

## 不变量

1. 必需字段同时存在，`source_ids` 非空。
2. `native_id` 只能在同一 `runtime` 内解释和比较。
3. 每个 Session 可以包含 `0..*` 个 Loop，每个 Loop 恰好属于一个 Session。
4. 同一个 Session 的 `source_ids` 中，每份 Source 至多出现一次；每份 Source 也只支撑一个 Session。
5. `forked_from` 和 `delegated_from` 不能指向当前 Session 自身，两项属性各自至多出现一次。
6. `forked_from.record_id` 和 `delegated_from.record_id` 必须指向明确记录对应属性的一条 Record。
7. Session 字段可以随新证据补全，但不能让 `session_id` 悄悄改指另一个原生上下文。

## 页面边界

本页不划分 Loop 生命周期，不分类 Item，不推断 Task、Project、意图或话题，也不规定 Runtime 命令、原生字段路径、数据库表和公共 Relation。物理证据如何形成当前 Session，见 [Trace Index 投影模型](/design/evidence-and-provenance)。
