---
title: "Trace Index Loop 领域模型"
description: "定义 Loop 的完整字段、身份、成员关系、模型与用量、结束边界和运行结果。"
---

# Trace Index Loop 领域模型

> [!IMPORTANT]
> Loop 是 Runtime 启动并持续驱动的一次 Agent 执行生命周期。一次 Loop 可以包含多次模型生成、工具调用、工具输出和运行中的人类调整。`loop_id` 是当前已发布投影中引用这次执行的不透明 ID，`start_record_id` 指出它凭哪条物理记录成立。Runtime 如果报告了本轮使用的模型和 token 用量，Loop 还会保存可选的 `model` 与 `usage`；缺失表示没有可靠事实，不表示零用量。

[Trace Index 领域模型](/design/domain-model) 从程序结构视角定义 Session、Loop 和 Item。本页定义 Loop 的完整字段、身份、成员关系、结束边界和运行结果。

## Loop 解决什么领域问题

Runtime 收到一项开启输入后，通常不会只调用模型一次。Agent 可能先解释准备做什么，再调用工具，读取工具输出，继续生成，并在执行期间接收人的补充或纠正。这些活动共同属于一次仍在推进的程序执行。

Loop 回答：哪些 Item 发生在同一次由 Runtime 持续驱动的 Agent 执行中，这次执行从哪里开始，以及是否已经结束并留下明确结果。

没有 Loop，工具往返会被拆成互不相关的消息；运行中的补充输入也可能被误认为下一次独立执行。Loop 的边界来自 Runtime 的程序结构，不来自输入文字包含几个任务，也不来自相邻消息看起来是否相关。

## Loop 的完整形状

```text
Loop {
  loop_id: LoopId
  session_id: SessionId
  session_position: uint64
  native_id?: string
  start_record_id: RecordId

  end?: {
    record_id: RecordId
    outcome?: completed | interrupted | failed
  }

  model?: {
    id: string
    effort?: string
    context_window?: uint64
  }

  usage?: {
    input: uint64
    cached?: uint64
    cache_write?: uint64
    output: uint64
    reasoning?: uint64
  }
}
```

`?` 表示字段可以不存在。`LoopId`、`SessionId` 和 `RecordId` 都是不透明引用，调用方只能把它们作为完整标识进行比较和引用。

| 字段 | 必需性 | 领域含义 |
| --- | --- | --- |
| `loop_id` | 必须 | 在当前已发布投影中引用这次 Agent 执行生命周期的不透明 ID |
| `session_id` | 必须 | 这次执行唯一所属的 Session |
| `session_position` | 必须 | Loop 在所属 Session 内的零起始顺序，用于跨 Loop 恢复完整时间线 |
| `native_id` | 可选 | Runtime 明确赋予这次外层执行的原生身份；没有这种身份时省略 |
| `start_record_id` | 必须 | 明确证明这次新 Loop 已经建立的一条规范 Record |
| `end` | 可选 | 已经有结构证据确认这次 Loop 不再继续时出现 |
| `end.record_id` | 存在 `end` 时必须 | 明确结束本轮，或明确建立下一独立 Loop 从而限定本轮边界的一条 Record |
| `end.outcome` | 可选 | Runtime 明确给出的整轮结果；只知道结束边界而不知道结果时省略 |
| `model` | 可选 | Runtime 为本轮模型执行报告的模型身份与配置；没有可靠报告时整体省略 |
| `model.id` | 存在 `model` 时必须 | Runtime 报告的模型身份，保留 Runtime 自己的名称 |
| `model.effort` | 可选 | Runtime 报告的推理强度或同类配置，保留 Runtime 自己的词汇 |
| `model.context_window` | 可选 | Runtime 报告的上下文容量；不能根据模型名称猜测 |
| `usage` | 可选 | 本轮中已经观察到的模型调用用量之和；缺失表示未知，不是零 |
| `usage.input` | 存在 `usage` 时必须 | 模型处理的全部输入 token，已经包含缓存读取和缓存写入部分 |
| `usage.cached` | 可选 | 已包含在 `input` 中的缓存读取 token |
| `usage.cache_write` | 可选 | 已包含在 `input` 中的缓存写入 token |
| `usage.output` | 存在 `usage` 时必须 | 模型生成的全部输出 token，已经包含推理 token |
| `usage.reasoning` | 可选 | 已包含在 `output` 中的推理 token |

当前模型不为缺失事实建立额外的 `unknown` 或 `open` 值。没有 `end` 已经准确表示“尚未观察到结束”，`end` 中没有 `outcome` 已经准确表示“知道本轮结束，但不知道结果”。

## Loop 的身份从明确开始建立

一条 Record 只有在 Runtime 的结构能够证明“这里建立了一次新的外层 Agent 执行”时，才能成为 `start_record_id`。这条证据可以同时携带 Runtime 的原生外层身份，也可以只声明一次外层生命周期开始。

`native_id` 不是建立所有 Loop 的前提。有些 Runtime 为外层执行提供稳定身份，有些 Runtime 只持久化能够确认开始的结构信号。两者都可以形成 Loop；区别只是后者省略 `native_id`。

身份遵守以下规则：

- 同一外层执行继续生成、调用工具、接收工具输出或处理运行中输入时，`loop_id` 不变。
- Runtime 重复记录同一个原生外层身份时，仍然是同一个 Loop，不因多了一条身份 Record 而新建对象。
- 前一 Loop 已经结束，Runtime 又建立新的独立执行时，形成新的 `loop_id`。
- 输入句柄、消息 ID、模型调用 ID、工具调用 ID、物理相邻和时间接近都不能单独建立 Loop 身份。
- 没有足够结构证据确认一次新的外层执行时，不创建 Loop。已经发生的 Item 仍然属于 Session，但不强行填写 `loop_id`。
- 更强证据证明原来的生命周期分区错误时，受影响的 Loop 和 Item 归属按正确边界重建。重建后的 Loop 可以获得新的 `loop_id`，领域契约不承诺复用旧值。

`start_record_id` 使用单条 Record，而不是 Record 范围或列表。它指向建立这次外层执行的规范证据。重复观察属于来源事实，不要求领域对象保存全部重复身份记录。

## Loop 与 Session、Item

一个 Loop 恰好属于一个 Session，一个 Session 可以包含零到多个 Loop。Session 在第一轮执行开始前可以暂时没有 Loop。

| 方向 | 关系 | 数量约束 |
| --- | --- | --- |
| Session → Loop | 包含 | `0..*` |
| Loop → Session | 归属 | `1` |
| Loop → Item | 包含 | `0..*` |
| Item → Loop | 证据确定时归属 | `0..1` |

成员关系的规范方向由 Item 的可选 `loop_id` 表达，Loop 不复制一个需要同步维护的 `item_ids` 列表。Loop 可以暂时没有 Item：例如 Runtime 已经明确建立执行，但尚未产生可选择为 Item 的活动。

Item 已经发生而 Loop 归属不能可靠确定时，它继续拥有必需的 `session_id`，并省略 `loop_id`。Trace Index 不为了让每个 Item 都落入一棵整齐的树而猜测边界。

Item 填写 `loop_id` 时，它自己的 `session_id` 必须与该 Loop 的 `session_id` 相同。Loop 不能包含属于另一个 Session 的 Item。

Loop 内顺序由 Item 的 `loop_position` 表达。它只表示同一 Loop 中的观察顺序，不表示因果、父子、分支或下一次执行从哪个程序位置继续。

Session 内的 Loop 顺序由 `session_position` 表达。它从 `0` 开始，并在同一 Session 中唯一。读取完整对话时间线时，先按 `session_position` 排列 Loop，再按各 Loop 内的 `loop_position` 排列 Item。不同 Loop 的 `loop_position` 都从 `0` 重新开始，不能直接拿来比较跨 Loop 先后。

## 模型与用量

`model` 与 `usage` 描述整次 Loop 中观察到的模型执行，而不是某一条消息或工具调用。它们留在 Loop 上，Agent 才能直接比较不同执行用了什么模型、上下文容量是多少，以及一轮工作消耗了多少模型 token，而不必从多条 Runtime 原生记录重复恢复并相加。

这些字段只保留 Runtime 确实报告的事实。`model.id` 不会触发模型名称到容量的推断；`effort` 也不被强行转换成跨 Runtime 的枚举。某个可选字段没有出现，表示 Runtime 没有报告或当前证据无法可靠建立它。Runtime 明确报告的 `0` 则仍然是已知的零，两者不能混淆。

不同 Runtime 对缓存 token 的报告方式不一样，`usage` 统一成下面的计数关系：

```text
cached <= input
cache_write <= input
reasoning <= output
total = input + output
```

`cached` 和 `cache_write` 是 `input` 的组成部分，`reasoning` 是 `output` 的组成部分。统计总量时不能把这些子集再次相加。Trace Index 不保存另一项 `total`，因为它可以无歧义地由 `input + output` 得到。

`usage` 是 Loop 内已观察到的模型调用之和，不表示供应商账单、费用或整份 Session 的累计用量。原生计数如何转换成这组共同字段，以及多个原生报告如何避免重复累计，由 Adapter 代码和测试负责。

## 结束边界与运行结果

`end` 表示已经确认本轮不再继续，`end.outcome` 表示 Runtime 对整轮执行给出的结果。两者相关但不相同，形成三种有效状态：

| `end` | `end.outcome` | 领域含义 |
| --- | --- | --- |
| 不存在 | — | 尚未观察到结束。Loop 可能仍在运行，也可能只是后续 Trace 尚未到达 |
| 存在 | 不存在 | 已经确认本轮不再继续，但没有整轮结果证据 |
| 存在 | `completed`、`interrupted` 或 `failed` | 已经确认结束，并且 Runtime 明确给出了整轮结果 |

字段是否存在已经完整表达这些状态，不再增加 `open`、`bounded` 或 `unknown` 等同义枚举。

| `outcome` | 领域含义 |
| --- | --- |
| `completed` | Runtime 明确记录这次外层 Agent 执行正常结束 |
| `interrupted` | Runtime 明确记录这次外层执行被人或程序中断 |
| `failed` | Runtime 明确记录这次外层执行因错误而结束 |

`completed` 只表示 Runtime 正常结束了这次 Loop，不表示自然语言中的 Task 已经完成、答案正确或用户满意。

`end` 必须由外层执行边界支持。下一次独立 Loop 的开始可以限定上一 Loop 的 `end`，但不能单独产生上一 Loop 的 `outcome`。单次模型生成的停止原因、工具调用前的停止、输出长度耗尽或看起来像最终回答的文字，都不能单独证明外层 Loop 已经结束。

最终回答与运行结果也不能互相推导。`agent.final_answer` 是属于 Loop 的 Item 语义，`end.outcome` 是整次外层执行的运行结果。Loop 不增加 `final_answer_item_id`。读取回答时，应查询属于该 Loop 且语义角色为 `agent.final_answer` 的 Item。

## 建立、补全与重建

Loop 根据当前 Record 投影建立：

1. 观察到明确的外层开始证据后，以该 Record 建立 `loop_id`、`session_id`、`session_position` 和 `start_record_id`，并在存在真实原生外层身份时填写 `native_id`。
2. 后续证据可以增加属于同一 Loop 的 Item，而不改变身份。
3. 明确结束信号或下一次独立 Loop 的开始可以补全 `end`。
4. Runtime 明确给出整轮结果时，可以补全 `end.outcome`。
5. Runtime 报告模型身份、配置或用量时，可以补全 `model` 与 `usage`，而不改变 Loop 身份。
6. 来源或映射规则变化并证明原分区错误时，相关 Loop 与 Item 归属一起按当前证据重建。

共同的证据和当前性约束见 [Trace Index 投影模型](/design/evidence-and-provenance)。本页只定义 Loop 自己特有的成立条件。

## 不变量

任何合法 Loop 都满足：

1. `loop_id`、`session_id`、`session_position` 和 `start_record_id` 同时存在。
2. 每个 Loop 恰好属于一个 Session。
3. `native_id` 只表示 Runtime 的外层执行身份，不能由输入、消息、模型调用或工具调用句柄代替。
4. `end` 存在时必须具有一个 `record_id`，且该 Record 必须证明外层执行结束或下一独立 Loop 已经建立。`outcome` 只能出现在 `end` 内。
5. `outcome` 只能是 `completed`、`interrupted` 或 `failed`，并且必须来自 Runtime 的整轮执行结构，不能从文字或单次模型停止原因推断。
6. Item 只有在 Loop 归属得到证据支持时才填写 `loop_id`。
7. Item 填写 `loop_id` 时，Item 与 Loop 必须属于同一个 Session。
8. `loop_id` 只保证在当前已发布投影中唯一引用一项 Loop。来源或映射变化触发重建后，调用方不能假设旧值仍然有效。
9. `model` 存在时必须包含非空 `id`；`context_window` 只能来自 Runtime 报告，不能按模型名补全。
10. `usage` 存在时必须同时包含 `input` 与 `output`；所有计数都是非负整数，且 `cached <= input`、`cache_write <= input`、`reasoning <= output`。
11. 同一 Session 内的 `session_position` 从 `0` 开始且不能重复；跨 Loop 顺序只能先按它确定。
12. 缺失的 `model`、`usage` 或其可选成员表示未知，不能按零处理；模型 token 总量只由 `input + output` 得出。

## 页面边界

Loop 不判断自然语言中的 Task 数量，不评价回答质量，也不把单次模型调用或消息句柄提升为外层执行。模型执行身份与归一化用量是可选的 Loop 属性；权限、指令和其他 Runtime 上下文如果形成 Item，仍由 Item 的 Semantic 表达，并以 Record 作为来源证据。它们不组成一份必须存在的 Loop 上下文快照。

具体 Runtime 的原生字段路径、Adapter 算法、数据库表和公共查询列不属于本页。人的输入怎样成为 Request 或 Steering，见 [Item 语义契约](/design/model/semantic-contract)。
