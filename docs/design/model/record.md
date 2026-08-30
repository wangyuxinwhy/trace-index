---
title: "Trace Index Record 领域模型"
description: "定义 Record 的最小完整字段、当前物理引用范围、内容定位与证据责任。"
---

# Trace Index Record 领域模型

> [!IMPORTANT]
> Record 是 Source 中一条已经达到 JSONL 行结束边界的物理记录。它保存的是“原始证据位于哪里”，而不是“这条记录在程序语义上表示什么”。一条内容无法解析的完整记录仍然是 Record；一段尚未结束的尾部还不是 Record。

[Trace Index 领域模型](/design/domain-model) 从物理视角建立 `Source → Record`，[Source 领域模型](/design/model/source) 定义 Record 所属的物理输入。本页定义 Record 的最小完整字段、当前引用范围和证据责任。各 Runtime 怎样识别并填充这些物理事实，由 Adapter 代码和测试负责。

## Record 解决什么问题

Source 能够说明一组证据来自哪一份物理输入，但不能单独指出其中某一次具体写入。Record 把 Source 划分成可以独立定位和校验的物理记录，使 Trace Index 能够回答：

- 一项事实最终来自 Source 中的哪条原始记录？
- 这条记录在 Source 中排第几，具体覆盖哪些字节？
- 再次读取 Source 时，这段原始内容是否仍然相同？
- Runtime 是否为这条记录明确提供了发生时间？

Record 不负责回答消息由谁产生、一次工作循环在哪里开始、工具调用和输出怎样关联，或者一条原始记录应该形成几个 Item。这些属于程序投影。

## 什么时候形成 Record

Trace Index v1 的 Source 是 JSONL 文件。换行符表示一条物理记录已经完整结束；只有达到这一结束边界的内容才形成 Record。

```text
当前读取的 Source
├─ 第 0 条边界完整的内容 ──> Record 0
├─ 第 1 条边界完整的内容 ──> Record 1
├─ 边界完整但内容语法无效 ──> 仍是 Record
└─ 尚未达到结束边界的尾部 ──> 不是 Record
```

以 JSONL 为例，一行内容只有在换行符出现后才形成 Record。换行符前的内容即使不是合法 JSON，仍是一条完整的物理记录。文件末尾尚未写完、也没有换行符的部分，要等后续写入完成边界后才能形成 Record。

## Record 的最小完整形状

```text
Record {
  record_id: RecordId
  source_id: SourceId
  source_position: uint64
  content_range: ByteRange
  fingerprint: Fingerprint
  occurred_at?: Instant
}

ByteRange {
  start: uint64
  end: uint64
}
```

`?` 表示字段可以不存在。`ByteRange` 使用半开区间 `[start, end)`：包含 `start` 指向的字节，不包含 `end` 指向的字节。`RecordId`、`SourceId`、`Fingerprint` 和 `Instant` 是领域类型，不规定数据库列或公共接口采用什么序列化。

| 字段 | 必需性 | 为什么存在 |
| --- | --- | --- |
| `record_id` | 必须 | 为当前已发布投影中的这条 Record 提供简洁、不透明的引用，使 Session、Loop、Item 和证据接口不必反复携带整组物理坐标 |
| `source_id` | 必须 | 指出 Record 唯一属于哪一份 Source；没有 Source，位置、格式和原始内容都无法解释 |
| `source_position` | 必须 | 表达从 `0` 开始的 Source 内物理顺序，使调用方能够稳定排序 Record，并在 Source 中定位相邻物理记录 |
| `content_range.start` | 必须 | 指出原始内容在 Source 中从哪个字节开始 |
| `content_range.end` | 必须 | 指出原始内容在哪个字节之前结束，使原始内容能够被精确读取；格式分隔符不包含在这段内容中 |
| `fingerprint` | 必须 | 校验 `content_range` 指向的原始内容仍然相同，防止同一坐标在 Source 改写后悄悄指向另一段内容 |
| `occurred_at` | 可选 | Runtime 明确为这条物理记录提供可靠时间时，保留该事实；没有明示时间的记录仍然完整有效 |

每个字段都解决一个已经存在、而且不能由另一个字段替代的问题。`source_position` 适合表达 Record 顺序和范围；`content_range` 适合直接读取原始字节；`fingerprint` 适合确认这些字节没有改变。三者相关，但不能互相冒充。

## 原始内容仍留在 Source 中

Record 不复制一份原始 JSON 或 Runtime 对象。读取原始内容时，先通过 `source_id` 找到 Source，再读取 `content_range` 覆盖的字节，并用 `fingerprint` 校验结果。

这样只有 Source 保存原始事实，Record 保存到该事实的精确坐标。公共接口可以为了阅读安全提供预览或结构化表示，但预览、截断文本和解析后的对象都不是第二份 Record 原始内容。

## `RecordId` 是当前引用，不是永久外部身份

`record_id` 只承诺能够在同一份当前已发布投影中唯一引用一条 Record。它是不透明值，调用方不能从中推断 Source、顺序、时间或 Runtime，也不应把它当作跨数据库重建永久有效的外部键。

一次完整重建，或者 Source 截断、改写并重新发布后，即使新的 Record 仍位于相同物理位置，系统也可以为它分配新的 `record_id`。需要长期保存证据位置的调用方，应保存 Source 身份、物理范围和校验信息，或者在新的当前投影中重新解析引用。

以下区别仍然成立：

- 两条内容完全相同的连续记录拥有不同的 `source_position`，因此是两条 Record。
- 相同内容出现在两个 Source 中，分别属于各自的 Source。
- `fingerprint` 证明内容相同，不单独决定 Record 身份。
- Runtime 原生消息 ID、调用 ID 或树节点 ID 表达程序结构，不替代物理 Record 的引用。

## 字段共同成立的约束

1. `record_id`、`source_id`、`source_position`、`content_range` 和 `fingerprint` 同时存在。
2. 同一 Source 中 `source_position` 唯一，从 `0` 开始，按格式识别出的物理记录顺序连续递增。
3. `content_range.start <= content_range.end`，并且 `content_range.end` 不超过当前已发布 Source 的已验证字节边界。
4. 同一 Source 中，Record 的 `content_range` 按 `source_position` 排列且互不重叠。JSONL 的换行符证明记录已经完整结束，但不包含在 Record 的内容范围中；每条 Record 不再保存一个重复的结束边界字段。
5. `fingerprint` 只校验 `content_range` 中的原始内容，不能脱离 Source 和范围单独解释。
6. `occurred_at` 只能来自 Runtime 明确写下、且能够可靠归属于这条 Record 的时间。文件修改时间、索引时间和相邻记录时间都不能代替它。

## Record 与 Source

一份能够发布的 Source 至少包含一条 Record，一条 Record 只属于一份 Source。没有任何完整 JSONL 行的候选输入还不能成为 Source。

| 方向 | 关系 | 数量约束 |
| --- | --- | --- |
| Source → Record | 包含 | `1..*` |
| Record → Source | 归属 | `1` |

这项归属由 `Record.source_id` 表达。Source 不保存另一份需要同步维护的 Record ID 列表。Record 的位置和物理顺序只在所属 Source 内有效，不能把两个 Source 的 `source_position` 直接拼成程序顺序。

## 物理顺序不等于程序关系

`source_position` 只回答 Runtime 以什么物理顺序写下这些记录。Runtime 内部的父指针、分支、回退、工具调用与输出、委派和 Session 延续表达的是程序关系。

例如，Runtime 回到较早节点后写下的新记录仍然位于文件尾部；它的物理位置更晚，不代表它在程序路径上继承紧邻的上一条 Record。程序路径由 Item、Loop 或 Session 的结构事实表达。

## Record 怎样支撑程序事实

Record 是证据单位，不是 Session、Loop 或 Item 的逐条副本：

- 一条 Record 可以只声明 Session 信息或工作循环边界，不形成 Item。
- 一条 Record 可以包含多个有独立查询价值的内容，因此形成多个 Item。
- Runtime 可以把同一件事写在两条物理记录中，此时一个 Item 可以由多条 Record 共同支撑。
- 同一条 Record 可以同时支撑一个程序实体及其多个属性。

是否形成 Item、形成几个 Item，以及多条物理证据何时表示同一件事，由 [Trace Index 投影模型](/design/evidence-and-provenance) 和 [Item 领域模型](/design/model/item) 决定。Record 只负责让这些结论能够返回精确原始证据。

## 解析结果和原生类型不属于 Record 核心字段

边界完整、序列化语法有效、Runtime 类型受到支持，是三个不同判断。它们不能被混成 Record 是否存在的条件：

- 达到格式结束边界后，Record 已经存在。
- 内容能否解析，是当前读取和解析过程的结果。
- 解析后的原生类型怎样解释，由 Adapter 代码和程序投影负责。

实现可以为了诊断暴露解析错误、大小限制或映射覆盖情况，但这些是索引处理事实，不是这条物理 Record 的永久属性。

Record 领域模型也不保存 `native_kind`。不同 Runtime 的类型结构并不相同，有些类型还是 Adapter 根据多个原生字段组合出来的查询标签。原始字段已经完整保留在 `content_range` 指向的 Runtime 内容中；当其中的事实形成 Item 时，Item 只保存已经稳定定义的 Semantic，原生 `kind`、外层结构和未建模字段仍通过 Record 回到 Source 核查。

## Source 变化时怎样处理 Record

Record 没有脱离 Source 的独立生命周期，也不经历“未解析 → 已解析”的领域状态迁移。

- Source 追加并形成新的完整边界时，增加新的 Record。
- 尚未结束的尾部不进入当前已发布的 Record 集合。
- Source 截断或改写后，受影响的当前 Record 和依赖它们的程序事实一起由新的完整发布结果替换。
- 同步或解析失败没有形成可以完整发布的新结果时，上一份已发布的 Source、Record 集合和程序事实保持不变。

哪些事实仍然属于当前投影，见 [Trace Index 投影模型](/design/evidence-and-provenance)；完整结果怎样对查询者生效，见 [Trace Index 发布与一致性](/design/index-consistency)。

## 页面边界

本页不判断消息作者、Item 语义、Loop 边界、Session 延续和父级属性或当前程序路径，也不规定 SQLite 表、公共查询列、摘要算法、解析错误码、大小限制和 Adapter 识别代码。

继续阅读 [Trace Index 投影模型](/design/evidence-and-provenance)，理解 Record 怎样成为程序事实的证据。
