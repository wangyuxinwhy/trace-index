---
title: "Trace Index 词汇表"
description: "澄清 Trace Index 中容易产生歧义的术语，并指向提供完整定义的页面。"
---

# Trace Index 词汇表

> [!IMPORTANT]
> 这份词汇表只澄清 Trace Index 中容易产生歧义的术语，并把读者引向拥有完整定义的页面。它不复制领域模型，也不把 Runtime 的原生名称强行改写成共同概念。

## Agent 与原生记录

| 术语 | 在 Trace Index 中的含义 | 完整定义 |
| --- | --- | --- |
| Agent Trace | Agent Runtime 在运行 Agent 时实际持久化的原生记录整体。它是运行活动留下的证据，不等于聊天记录，也不保证记录了全部活动 | [Trace Index 是什么](/design/what-is-trace-index) |
| Runtime | 维护 Agent 上下文、驱动 Agent Loop 并持久化原生 Trace 的程序。这里不是指操作系统、编程语言、模型或模型供应商 | [Trace Index 是什么](/design/what-is-trace-index) |
| 原生结构 | Runtime 自己赋予含义的记录类型、身份、顺序、父子关系和生命周期信号。原生名称只在对应 Runtime 的语义范围内成立 | [Trace Index 架构与知识边界](/design/architecture) |
| Adapter | 读取一种 Runtime 的原生 Trace，识别 Source 与 Record，并为领域投影提供原生结构解释的边界组件 | [Trace Index 总体架构](/design/architecture) |
| Transcript | 从 Trace 中选取并组织人机对话后形成的阅读结果。它会有意省略工具、环境和运行状态等非对话事实 | [Trace Index 是什么](/design/what-is-trace-index) |
| Log | 程序为诊断和状态观察产生的记录。Log 可以辅助解释 Agent 活动，但不因此等同于 Agent Trace | [Trace Index 是什么](/design/what-is-trace-index) |

## 两个模型视角

| 术语 | 在 Trace Index 中的含义 | 完整定义 |
| --- | --- | --- |
| 物理事实 | Runtime 实际持久化了什么，以及这些材料来自哪份物理输入。Source 和 Record 位于这一视角 | [Trace Index 领域模型](/design/domain-model) |
| 程序事实 | Trace Index 根据原生结构解释和领域规则建立的 Agent 程序事实，例如 Session、Loop 和 Item | [Trace Index 领域模型](/design/domain-model) |
| Source | 一份可以独立定位、读取并隔离变化的物理 Trace 输入 | [Source 领域模型](/design/model/source) |
| Record | Source 中一条边界完整、能够独立定位和校验的物理记录 | [Record 领域模型](/design/model/record) |
| Session | Runtime 赋予身份并持续维护的 Agent 上下文。它不等于 Task、话题或一次 Loop | [Session 领域模型](/design/model/session) |
| Loop | Runtime 启动并持续驱动的一次 Agent 执行生命周期 | [Loop 领域模型](/design/model/loop) |
| Item | Trace Index 从 Record 中选择保留的一项具有独立查询价值的程序事实 | [Item 领域模型](/design/model/item) |

这些最小定义只帮助读者选对对象。字段、身份、边界、数量关系和不变量见相应领域模型。

## Item 的语义与证据

| 术语 | 在 Trace Index 中的含义 | 完整定义 |
| --- | --- | --- |
| Semantic | Item 中由 Trace Index 定义的跨 Runtime 表示，形状为 `{role, value, evidence_strength}`。`role` 决定 `value` 的具体类型 | [Item 语义契约](/design/model/semantic-contract) |
| Evidence Strength | 建立一项 Semantic 分类时所用的最弱依据。`structural` 表示由 Runtime 结构直接支持，`heuristic` 表示分类依赖回退规则 | [Item 语义契约](/design/model/semantic-contract) |
| Record evidence | Item 的 `record_ids` 指向支撑当前 Semantic 的原始 Record。需要 Runtime 专属细节时沿这些 Record 回到 Source，而不是让 Item 复制原始 payload | [Item 领域模型](/design/model/item) |

Semantic 回答“这项事实可以怎样稳定理解”，Record evidence 回答“这项理解由哪些原始材料支持”。只有 Record 但没有稳定语义时，物理记录仍然存在，但不会为了提高覆盖率而形成 Item。

## 映射、投影与查询

| 术语 | 在 Trace Index 中的含义 | 完整定义 |
| --- | --- | --- |
| Runtime 映射 | Adapter 把某种 Runtime 的原生结构解释为 Trace Index 领域事实的规则。具体字段和版本分支由代码与测试维护 | [Trace Index 架构与知识边界](/design/architecture) |
| 投影 | 把 Adapter 的原生结构解释和领域规则应用于 Source 与 Record，从而建立 Session、Loop、Item 及其字段 | [Trace Index 投影模型](/design/evidence-and-provenance) |
| 证据 | 支撑一项当前程序事实的 Record，以及该 Record 所属的 Source。具体引用字段由拥有该事实的实体定义 | [Trace Index 投影模型](/design/evidence-and-provenance) |
| 溯源 | 从一项当前程序事实返回支撑它的 Record、Source 和原始 Trace | [Trace Index 投影模型](/design/evidence-and-provenance) |
| 当前投影 | 已经完整发布、可以查询，并且仍由当前 Source、Record、Adapter 解释和领域规则支持的程序事实 | [Trace Index 投影模型](/design/evidence-and-provenance) |
| 全文检索 | 从 Item 的文本内容建立候选查找入口，再回到命中 Item、程序上下文和物理来源的访问能力 | [Trace Index 总体架构](/design/architecture) |
| 原生身份 | Runtime 为自己的上下文或活动提供的身份。它必须结合对应 Runtime 和实体成立条件解释，不能直接当作 Trace Index 的统一身份 | [Trace Index 投影模型](/design/evidence-and-provenance) |
| Task | 人或 Agent 希望完成的自然语义目标。Trace Index 可以提供相关事实，但不从 Session、Loop 或文字内容自动推断 Task 边界 | [Trace Index 领域模型](/design/domain-model) |

## 原生名称不自动等于共同概念

不同 Runtime 可以使用 `turn`、`session`、`message` 等相同或相近的词，却指向不同的程序边界。Trace Index 不按字面翻译这些名称，而是检查它们提供的身份、边界和关系证据，再映射到共同模型。

看到原生名称时，先把它当作 Runtime 术语。只有 Adapter 代码和测试证明其结构满足某个共同概念的成立条件后，才能用 Source、Record、Session、Loop 或 Item 描述它。精确字段、当前支持范围和版本行为在代码仓库维护，不进入词汇表。
