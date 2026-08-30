---
name: trace-index
description: Query indexed Codex, Pi, and Claude Code traces with trace-index when investigating sessions, agent work, tool use, or physical source evidence.
---

# Trace Index

Use `trace-index` as the read-only, source-traceable SQL query plane for Codex, Pi, and Claude Code traces. Trace Index returns facts and provenance; interpret them and form the conclusion yourself.

At the start of each trace-analysis task, run `trace-index --help` once. Its help, bundled documentation, and Schema match the installed binary. Follow the discovery path instead of guessing relation names, columns, JSON shapes, or flags:

```bash
trace-index docs list
trace-index schema list
trace-index schema get <RELATION> --compact
```

Load `trace-index docs get start-here` when the tutorial is useful. Load the full schema for one relation only when its field descriptions or JSON contract are needed.

The public domain objects are `sources`, `records`, `sessions`, `loops`, and `items`. `blobs` and `item_search` are access relations, not domain objects. `blobs` resolves bounded semantic text by `blob_id`; `item_search` retrieves candidate Item ids. Do not build routine analysis on visible storage tables.

Use `items` as the central fact relation. Within one Loop, order by `loop_position`. Each Item has:

- `semantic`, a directly stored JSON object `{role, value, evidence_strength}` for Trace Index's cross-Runtime meaning.
- A non-empty `record_ids` JSON array for its physical witnesses.

Read `semantic.role` with these stable meanings:

- `human.request`: Human input that opens a new Loop.
- `human.steering`: Human input delivered while the current Loop is still running.
- `agent.commentary`: Agent progress or intermediate communication emitted during a Loop.
- `agent.final_answer`: The Agent response marked as final for a Loop.
- `agent.reasoning`: Reasoning exposed by the Runtime as full text, a summary, or unavailable.
- `agent.tool_call`: A Runtime tool invocation with Agent-authored arguments.
- `agent.tool_call.shell`: A tool invocation whose command-bearing arguments also have parsed shell structure.
- `agent.delegation`: Work the Agent sends to a child Agent, optionally linked to its Session.
- `tool.output`: The result returned by a tool, optionally linked to the calling Item.
- `subagent.activity`: Intermediate activity associated with a child Session.
- `subagent.report`: A result returned from a child Session to its parent Agent.
- `runtime.instructions`: Instructions injected by the Runtime or harness, not Human input.
- `runtime.context`: Environment, memory, file, or Session context supplied by the Runtime, not Human input.
- `runtime.state`: Runtime-owned state or a state change placed on the Session timeline.
- `runtime.notice`: A Runtime-owned informational or control event placed on the Session timeline.
- `runtime.compaction_summary`: A Runtime-produced summary that replaces earlier context.
- `runtime.unknown`: Meaningful bounded Runtime content whose more specific stable role is not yet known.

Inspect the Item schema before extracting JSON. `semantic.role` selects the type of `semantic.value`; do not assume every value has a `text` field. Report a judgment whose `semantic.evidence_strength` is `heuristic` more cautiously than one marked `structural`. When a claim depends on an exact Runtime field that the Semantic contract does not publish, follow `record_ids` and inspect the supporting Record instead of assuming the Item contains a Runtime payload copy.

In the public SQL encoding, top-level domain TextContent is not inline. `semantic.value.text`, or `semantic.value.summary` for `runtime.compaction_summary`, is a `BlobRef {blob_id}`. Inspect `schema get blobs` before the first text query, then join that id to `blobs.blob_id` only when the query needs `text`, `full_bytes`, or `estimated_tokens`. Start from `items`; a standalone Blob row is access data and has no published domain meaning. Compare the UTF-8 byte length of `blobs.text` with `blobs.full_bytes` before treating it as the whole Source text. On Tool Outputs, `runtime_truncated` and `runtime_output_tokens` describe an earlier Runtime boundary and must not be conflated with the Blob's Source-text boundary.

Loop model identity and normalized token usage are nested under `loops.model` and `loops.usage`. Cache-read and cache-write tokens are already subsets of `usage.input`, and reasoning tokens are already a subset of `usage.output`. Derive total model tokens as `input + output`; never add the subsets again or treat missing Usage as zero.

Run bounded projections, filters, counts, groups, and joins with `query run`. Stop when the requested claim is supported. When `complete` is false, the rows are only a partial result set; follow the returned `hint` before drawing an exhaustive conclusion. Reads never refresh the index; run `index sync` only when newly written traces are required.

For broad text recall, use search only to generate candidates instead of fetching whole conversations. Derive several literal anchors from the user's wording, measure candidate density, and initially return only identities, scope, role, and short snippets. Compare candidates against the Runtime, time, project, actor, and provenance constraints that the request actually supplies before expanding any conversation. If the evidence does not identify one Session, preserve that ambiguity or ask for the missing discriminator. The bundled `how-to/search-literals` document gives the complete workflow and SQL patterns.

In `sessions`, `session_id` is the Trace Index-local join key and `native_id` is the Runtime-native identity. Report `runtime` plus `native_id` when the user asks for a Codex, Pi, or Claude Code Session id.

Use `item_search` only to generate broad literal candidates. Its contentless `text` column is a MATCH operand and reads as NULL; candidate output is `rowid`. Join that id to `items.item_id`, select the `blob_id` defined by that Item's `semantic.role`, join `blobs`, and exact-check `blobs.text` with `instr()` or an equally explicit comparison. Trigram matching is case-folded and broader than exact matching. For literals shorter than three Unicode characters, narrow Items structurally first and join only their referenced Blobs.

Do not join `blobs` for role counts, timelines, tool metadata, Loop usage, or any other query that does not read text. Keeping Blob access demand-driven avoids loading large values into routine analysis.

Do not inspect physical Records or include Record ids by default. Expand `items.record_ids` and use `record inspect <ID>` only when the user asks for raw source evidence, an exact physical representation, or index-integrity debugging. Export Records or Assets only to an explicit destination.

Match evidence scope to the claim. Keep Runtime, Session, Source, Loop, time, and semantic-role boundaries visible when they affect whether rows are comparable. An empty result can reflect an Adapter coverage gap, so verify the relevant Runtime coverage before concluding that something never occurred.

Treat `semantic.role` as evidence ownership, not merely as a query filter. A claim that a person requested, decided, approved, rejected, or corrected something needs supporting `human.request` or `human.steering` text. Adjacent Agent commentary or a Final Answer can establish what the Agent proposed, but adjacency does not establish that the person accepted it. When the Human response is ambiguous, keep proposals and confirmed decisions separate in the conclusion.

Measure coverage when a claim describes a property across a scoped Item population but an optional Semantic member or optional relationship determines which Items contribute values. State the intended population, contributor count, and missing count. An inner join to that optional value can silently shrink the denominator; missing is not zero, and contributors must not be described as the whole population. Deliberate search, role, time, or project filters define the scope and are not missing coverage.
