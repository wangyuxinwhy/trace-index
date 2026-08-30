---
title: "Trace Index 是什么"
description: "说明 Trace Index 如何连接 Agent 对 Trace 的生产与消费，跨会话、跨项目、跨 Runtime 组织和索引可查询、可溯源的事实。"
---

# Trace Index 是什么

> [!IMPORTANT]
> Trace Index 是面向 Agent 的本地 Agent Trace 事实查询层。它把异构 Runtime 持久化的原始 Trace 组织成范围明确、可以查询并能回到来源验证的事实，使 Agent 能够跨会话、跨项目和跨 Runtime 使用历史运行信息。

Agent Trace 是 Agent Runtime 在运行 Agent 时实际持久化的原生记录整体。这里的 Runtime 是维护 Agent 上下文、驱动 Agent Loop，并决定哪些活动会被持久化的程序。Trace 可能包含人的输入、Agent 输出、推理、工具调用、工具结果、上下文材料和运行状态，但它只代表 Runtime 实际记录下来的部分，不保证覆盖运行中的一切。

Trace 不等于 Transcript，也不等于一般 Log。Transcript 是从 Trace 中选取并组织人机对话后形成的阅读结果，会有意省略工具、环境和运行状态；Log 主要服务程序诊断和状态观察。Trace Index 的输入是 Runtime 的原始 Trace，它可以从中形成对话读取或诊断查询，但不把某一种阅读结果当作原始事实。

## Trace Index 解决什么问题

原始 Trace 分散在不同文件、会话、项目和 Runtime 中。每次调查都直接读取原始文件，Agent 就必须反复完成来源发现、格式识别、上下文身份恢复、运行边界判断和工具活动关联，还要把大量无关内容带入有限上下文。

其中一部分工作并不依赖当前问题：确定物理来源、识别完整记录、恢复 Runtime 明示的程序结构，并保留回到原始记录的路径。Trace Index 把这些确定性工作提前完成，让后来的 Agent 从已经组织好的事实开始，再根据当前问题决定查询范围和解释方式。

```mermaid
flowchart LR
    runtime[Agent Runtime] -->|持久化| trace[原始 Agent Trace]
    trace -->|组织与索引| index[Trace Index]
    index -->|有界事实与来源| agent[Agent]
    agent -->|解释与行动| task[当前任务]
```

Trace Index 缩短的是从“提出问题”到“取得相关历史事实”的距离。它不替 Agent 判断哪些事实与当前任务相关，也不替 Agent 形成结论。

## 产品定义

“面向 Agent 的本地 Trace 事实查询层”包含下面五项约束：

| 关键词 | 含义 |
| --- | --- |
| 面向 Agent | 能力、字段、结果边界和失败信息首先支持 Agent 自主发现、组合和验证事实 |
| 本地 | 原始 Trace、索引和查询者属于同一个本地数据所有者；系统不是面向他人 Trace 的共享服务 |
| 事实 | 只建立来源能够支持的物理事实和程序事实，不把针对某个问题形成的解释提前固化 |
| 查询层 | Agent 可以按当前问题组合条件并逐步缩小范围，而不要求未来问题在 Trace 产生时已经被预知 |
| 可溯源 | 程序事实能够返回支撑它的 Record、Source 和原始 Trace |

“有界”同样是产品定义的一部分。Trace 可以持续增长，Agent 的上下文和单次任务成本却始终有限。查询必须允许 Agent 先定位范围，再读取有限事实，并明确说明结果是否受限或不完整。

## Trace Index 负责什么

Trace Index 负责：

- 发现并识别可以独立处理的物理 Trace 输入。
- 保留 Runtime 实际写下的完整物理记录及其来源位置。
- 根据 Runtime 结构和领域规则建立 Session、Loop、Item 等程序事实。
- 提供对这些领域对象的直接、只读查询。
- 提供全文检索，使 Agent 能从文字命中返回相应 Item 和上下文。
- 让程序事实能够回到物理来源复核。

不同 Runtime 可以使用不同结构表达同类程序事实，也可能没有记录某项信息。Trace Index 统一可共同查询的领域语义，并让每项解释能够返回支撑它的原始 Record。共同字段不表示不同 Runtime 的证据强度天然相同；Runtime 专属细节仍可沿 Record 回到 Source 核查。

## 为什么索引事实，而不是预写摘要

未来会提出什么问题无法在 Trace 产生时预知。今天看似无关的一次工具失败、用户修正或废弃路径，可能是以后恢复决定、诊断问题或比较工作流所需的证据。

固定摘要会提前决定什么重要、什么可以丢弃。Trace Index 因而保存可重新查询的事实，把相关性判断、总结、归纳和演绎留到问题出现时由 Agent 完成。摘要、记忆、经验和知识可以建立在 Trace Index 之上，但不是 Trace Index 的核心事实。

## 三条产品边界

### 观察边界

Trace Index 只能组织 Runtime 实际持久化的内容。来源没有记录的上下文、行动或观察必须保持缺失，不能通过猜测补全。

### 统一边界

Trace Index 统一共同领域语义，不抹平 Runtime 差异。跨 Runtime 查询是一种能力，不是“所有 Runtime 行为和证据已经等价”的结论。

### 解释边界

Trace Index 提供来源支持的事实和依据。Agent 根据当前问题决定哪些事实相关、它们意味着什么，以及接下来采取什么行动。

## 继续阅读

[Trace Index 设计原则](/design/design-principles) 说明来源忠实性、跨 Runtime 统一、Agent 使用体验、查询成本和实现复杂度发生冲突时怎样取舍。[Trace Index 领域模型](/design/domain-model) 定义 Source、Record、Session、Loop、Item 五个核心实体。
