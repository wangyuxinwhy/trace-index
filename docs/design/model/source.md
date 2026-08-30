---
title: "Trace Index Source 领域模型"
description: "定义 Source 的三个必需字段、物理身份，以及与 Record、Session 的数量关系。"
---

# Trace Index Source 领域模型

> [!IMPORTANT]
> Source 是 Trace Index 已经识别并纳入当前索引的一份 Runtime Trace 文件。每个 Source 只有三个领域字段：Trace Index 分配的 `source_id`、可以重新找到原始文件的 `locator`，以及决定原生内容如何解释的 `runtime`。当前 v1 只接受 JSONL；这是所有 Source 共同遵守的输入约束，不是每个 Source 各自变化的字段。

[Trace Index 领域模型](/design/domain-model) 用 Source 和 Record 保存物理证据。本页完整定义 Source 的字段、身份，以及它与 Record、Session 的关系。各 Runtime 怎样发现并识别具体文件，由 Adapter 代码和测试负责。

## Source 解决什么问题

Runtime 会把 Agent 的活动持续写入 Trace 文件。Trace Index 必须知道每条 Record 来自哪一份文件，才能正确解释它在文件中的位置、重新读取原始字节，并在文件追加或改写后只重建受影响的事实。

因此，Source 回答的是：

> 这些 Record 来自哪一份能够单独定位、读取和重新检查的 Runtime Trace 文件？

Source 只界定物理证据来自哪里。它不根据文件内容猜测 Task 或 Project，也不把文件本身当成 Session 身份。

## Source 的完整形状

```text
Source {
  source_id: SourceId
  locator: string
  runtime: RuntimeName
}
```

这三个字段都是必需字段。当前模型没有可选字段。

| 字段 | 含义 | 为什么必须存在 |
| --- | --- | --- |
| `source_id` | Trace Index 为这份 Source 分配的不透明引用 | Record 和 Session 需要通过一个简短、稳定的引用指向 Source，而不复制物理路径。它只在当前 Trace Index 中有意义，调用方不能从数值或文字推断 Runtime、路径、顺序或时间 |
| `locator` | 能够重新找到原始 Trace 文件的规范化绝对路径 | Trace Index 需要回到原始文件读取和校验 Record。同一路径也构成当前 v1 判断 Source 身份的依据 |
| `runtime` | 产生这份 Trace，并定义其中原生结构含义的 Runtime | v1 输入都使用 JSONL，但字段、事件类型和上下文规则仍由各 Runtime 定义。缺少 Runtime 就无法可靠解释 Record，也无法建立 Session、Loop 和 Item |

`runtime` 不是操作系统、编程语言、模型或模型供应商。它表示负责维护 Agent 上下文、驱动 Agent Loop 并持久化这份 Trace 的程序。

## 当前 v1 的 JSONL 约束

当前 Trace Index 只把 JSONL Trace 文件建立为 Source。每一条以换行结束的物理行形成一条边界完整的 Record；这一行是否为合法 JSON，由索引过程在同步诊断中说明。文件末尾尚未出现换行的字节还不是一条完整 Record，因此不会提前进入当前 Record 集合。

JSONL 不作为 Source 的实例级字段保存，因为当前没有第二种真实格式需要逐份区分。把所有实例都相同的常量保存成字段，只会让读者误以为不同 Source 可以选择不同格式。如果以后 Trace Index 真正支持另一种成帧方式，再根据已经出现的输入修改契约。

Runtime 在原生 Trace 中记录的协议或结构版本属于原生内容，不是 Source 物理格式的共同版本，因此不会成为 Source 的共同字段。

## Source 怎样成立

发现一个 `.jsonl` 文件不等于已经建立 Source。当前模型要求：

1. 文件至少包含一条边界完整的 Record；
2. Trace Index 能够从结构证据确认其 `runtime`；
3. 文件拥有可重新访问的规范化绝对路径；
4. Trace Index 能够从原生上下文身份证据确定它唯一支撑的 Session。

空文件、只有未结束尾部的文件、无法识别 Runtime 的普通 JSONL 文件，或者无法确定唯一 Session 的输入，都只是发现过程中的候选输入，不是当前领域模型中的 Source。

## Source 的身份

当前 v1 使用 `locator` 判断两次观察是否指向同一个 Source：

- 同一路径追加新的完整 Record，仍是同一个 Source，`source_id` 保持不变。
- 同一路径被截断或改写，仍是同一个 Source；旧 Record 和依赖事实是否继续成立，需要按新内容重新判断。
- 相同内容出现在两个不同路径时，是两个 Source。内容摘要相同不能证明物理来源相同。
- 文件移动到另一个路径后形成新的 Source，因为不同的 `locator` 表示不同的 Source 身份。
- Session ID、工作目录、仓库、时间接近和内容相似都不能代替 `locator`。

`source_id` 在同一份索引中让其他事实稳定引用 Source，但它不是跨数据库重建仍保持相同数值的永久标识。需要重新找到原始证据时，使用 `locator`，而不是解析 `source_id`。

## Source 与 Record

一份 Source 包含一条或多条 Record，一条 Record 只属于一份 Source。

| 方向 | 关系 | 数量约束 |
| --- | --- | --- |
| Source → Record | 包含 | `1..*` |
| Record → Source | 归属 | `1` |

Source 不保存一个需要重复维护的 Record ID 列表。每条 Record 通过自己的 `source_id` 表达归属，并使用 Source 内的位置字段表达物理顺序。跨 Source 的 Record 位置不能直接拼接成程序顺序。

文件中只有部分行能够投影成 Item 是正常现象。Source 至少拥有一条物理 Record，并不意味着每条 Record 都必须形成程序语义。

## Source 与 Session

每份当前有效 Source 支撑一个逻辑 Session；一个 Session 可以由一份或多份 Source 共同支撑。

| 方向 | 关系 | 数量约束 |
| --- | --- | --- |
| Source → Session | 支撑 | `1` |
| Session → Source | 被支撑 | `1..*` |

Source 和 Session 不能直接等同。Source 是物理文件身份，Session 是 Runtime 持续维护的逻辑 Agent 上下文。Runtime 可以在保持同一上下文身份时创建新的物理文件。这时产生了新的 Source，但逻辑 Session 继续存在。

Session 通过自己的 `source_ids` 引用支撑它的一份或多份完整 Source。每份 Source 当前发布的 Record 集合构成它为唯一 Session 提供的物理证据。这不表示每条 Record 都会形成 Item。完整关系见 [Session 领域模型](/design/model/session)。

一份 Source 中可能包含继承历史、重放记录或祖先引用。这些内容仍是当前文件中的 Record，但不会让同一 Source 同时支撑多个 Session。

## Source 内容变化后发生什么

Source 的领域身份和由它支撑的当前事实需要分开理解：

- `source_id` 与 `locator` 回答“这是哪份物理输入”；
- 当前 Record 集合回答“这份输入目前有哪些已经纳入索引的完整记录”；
- Session、Loop 和 Item 回答“这些 Record 能够证明哪些程序事实”。

追加完整行会增加 Record。同一路径发生截断或改写时，Trace Index 重新建立与新内容一致的 Record 和程序投影。读取、解析或投影失败时，不应发布一半新、一半旧的结果。完整结果如何对查询者一次生效，属于 [Trace Index 发布与一致性](/design/index-consistency) 的责任。

Source 不需要额外的 `snapshot` 字段表达当前状态。已发布的 Record 集合已经说明当前有哪些物理证据；文件长度、已索引字节数、行数、前缀指纹和同步状态是增量索引与运行观测数据，由代码仓库和机器可读接口维护。

## 不变量

1. `source_id`、`locator` 和 `runtime` 同时存在。
2. `locator` 在当前 Trace Index 中唯一决定 Source 身份。
3. 每份 Source 至少拥有一条边界完整的 Record，每条 Record 只属于一份 Source。
4. 每份 Source 支撑一个 Session，每个 Session 至少由一份 Source 支撑。
5. Source 中只有换行结束的 JSONL 物理行能够形成 Record。
6. Runtime 或唯一 Session 无法由结构证据确认时，不建立 Source，也不使用猜测规则建立程序事实。

## 页面边界

本页不规定目录扫描方式、Runtime 检测字段、数据库表、增量 checkpoint、前缀指纹、同步状态或公共查询列。这些事实需要紧贴代码维护。

继续阅读 [Record 领域模型](/design/model/record) 与 [Session 领域模型](/design/model/session)，理解物理记录和逻辑上下文怎样建立在 Source 之上。
