---
title: Public SQL Schema
description: Public relations and their evidence semantics.
---

# Public SQL Schema

Trace Index publishes Source, Record, Session, Loop, and Item as five read-only domain relations. It also publishes `blobs` for demand-driven text access and `item_search` for candidate retrieval. These seven relations form the stable query contract; the two access relations do not add domain objects.

Discover the exact names, columns, SQLite types, nullability, and descriptions from the running binary:

```bash
trace-index schema list
trace-index schema get items --compact
```

## Relations

<!-- generated: relation-list — run `UPDATE_DOCS=1 cargo test generated_documentation` to refresh -->

- `sources`: Physical Runtime Trace inputs currently published by Trace Index.
- `records`: Boundary-complete physical Records. Raw bytes remain in the Source and can be read with `record inspect`.
- `sessions`: Runtime-maintained logical Agent contexts. Arrays and nested optional attributes are JSON values belonging to the Session object.
- `loops`: Outer Agent execution lifecycles. A missing `end` means no ending has been observed; an `end` without outcome means the boundary is known but the Runtime did not state a result.
- `items`: Individually queryable Agent program facts. `semantic` is the typed Trace Index meaning; `record_ids` identifies its physical evidence.
- `blobs`: Bounded Semantic text addressable through Item BlobRef values. Blob rows are access data, not independent trace facts.
- `item_search`: Trigram candidate index over explicitly documented Item semantic text. `rowid` is the matching `items.item_id`; exact-check candidate text.

<!-- /generated -->

## Domain relations and access relations

`sources` has exactly three fields: the opaque `source_id`, canonical `locator`, and `runtime` that defines the Source's native structure. Framing, synchronization checkpoints, and publication state are implementation concerns rather than public Source attributes.

`records` locates every complete framed Record within its Source. `content_range` is a JSON half-open byte range `{start, end}`; together with `fingerprint`, it provides the physical locator needed for verification.

`sessions` identifies a continuing Runtime context. `session_id` is an opaque Trace Index-local join key; `native_id` is the identity assigned by the named Runtime and is the value normally reported when a user asks for a Codex, Pi, or Claude Code Session id. `source_ids` is a JSON array because resume and lineage can make several Sources support one Session. `forked_from` and `delegated_from` are optional Session attributes encoded as JSON objects; a sparse private table used to store them does not create another domain object.

`loops` identifies an outer execution lifecycle inside a Session. `session_position` is its zero-based order inside the Session. `start_record_id` is required. Optional `end` is a JSON object containing the ending `record_id` and, when the Runtime stated a result, `outcome`. The outcome vocabulary is `completed`, `interrupted`, and `failed`.

`items` is the central fact stream. `session_id` is always present. `loop_id` and `loop_position` are present only when Loop membership is structurally known. Order a Loop by `loop_position`; Session-scoped Items with no Loop stay queryable with both fields null.

Every Item has directly stored `semantic = {role, value, evidence_strength}` JSON. This is the stable SQL encoding of Trace Index's typed domain Semantic. `role` selects the valid `value` shape. `structural` and `heuristic` state how the classification was established.

`record_ids` is the non-empty JSON array of physical witnesses supporting the Item. Use `json_each(record_ids)` when each witness must be joined to `records`.

The domain model represents a present Semantic text member as bounded `TextContent {value, full_bytes, estimated_tokens}`. The SQL encoding does not repeat that object inside `items.semantic`: a regular text-bearing value uses `text: {blob_id}`, while `runtime.compaction_summary` uses `summary: {blob_id}`. Join that reference to `blobs` only when the query needs the bounded text or its size metadata. Runtime fields not represented by the typed Semantic contract remain in the supporting Records. Follow `record_ids` and use `record inspect` when an investigation needs the exact Runtime representation.

Start text analysis from `items`, then follow the BlobRef to `blobs`. A Blob row reached this way is the text of that Item's Semantic value. A standalone Blob row has no published domain meaning: storage may retain deduplicated or no-longer-referenced content while rebuilding an index, and the access relation does not turn that storage row into a trace fact.

## Loop model and usage

`loops.model` is optional JSON:

```text
Model {
  id: string
  effort?: string
  context_window?: integer
}
```

`id`, `effort`, and `context_window` are Runtime-reported facts for the model execution observed in that Loop. Runtime vocabulary is preserved. A missing member means the Runtime did not report it; Trace Index does not infer context capacity from a model name.

`loops.usage` is optional normalized JSON:

```text
Usage {
  input: integer
  cached?: integer
  cache_write?: integer
  output: integer
  reasoning?: integer
}
```

`input` includes all model input, including cache reads and writes. `cached` and `cache_write` are subsets of `input`, not additional amounts. `output` includes reasoning tokens, and `reasoning` is a subset of `output`. All numbers are sums of the model calls observed inside this Loop. Derive total model tokens as `input + output`; no separate total is stored. Missing usage is unknown, not zero.

## Semantic SQL encoding

The public SQL representation of top-level Semantic text uses a small reference:

```text
BlobRef {
  blob_id: integer
}
```

`blob_id` joins to the `blobs` access relation:

| Column | Meaning |
|---|---|
| `blob_id` | Opaque text reference in this index |
| `text` | Bounded text published for queries; it can be a prefix |
| `full_bytes` | UTF-8 bytes in the complete Source text before Trace Index applied its bound |
| `estimated_tokens` | Deterministic estimate over that same complete Source text |

Compare the UTF-8 byte length of `blobs.text` with `blobs.full_bytes` to determine whether the published text is complete; equality means complete, while a smaller value means a prefix. `estimated_tokens` uses the Storage Format's deterministic `ascii4_unicode1_v1` estimator: ASCII characters contribute `ceil(count / 4)` and each non-ASCII Unicode scalar contributes one. It is a corpus-size estimate, not provider billing usage.

Within the SQL encoding, `semantic.role` selects exactly one `semantic.value` shape:

| Role | Value shape | Meaning |
|---|---|---|
| `human.request` | `{text?: BlobRef, has_images: bool}` | Human input that opens a new Loop. |
| `human.steering` | `{text?: BlobRef, has_images: bool}` | Human input delivered while the current Loop is still running. |
| `agent.commentary` | `{text?: BlobRef, has_images: bool}` | Agent progress or intermediate communication emitted during a Loop. |
| `agent.final_answer` | `{text?: BlobRef, has_images: bool}` | The Agent response marked as final for a Loop. |
| `agent.reasoning` | `{representation: full \| summary \| unavailable, text?: BlobRef}` | Reasoning exposed by the Runtime as full text, a summary, or unavailable. |
| `agent.tool_call` | `{tool_name?: string, arguments?: JSON, working_directory?: string}` | A Runtime tool invocation with Agent-authored arguments. |
| `agent.tool_call.shell` | Tool-call fields plus `{shell_fragments: ShellFragment[]}` | A tool invocation whose command-bearing arguments also have parsed shell structure. |
| `agent.delegation` | `{text?: BlobRef, has_images: bool, child_session_id?: SessionId}` | Work the Agent sends to a child Agent, optionally linked to its Session. |
| `tool.output` | `{call_item_id?: ItemId, text?: BlobRef, exit_code?: integer, failed?: bool, duration_ms?: integer, runtime_truncated?: bool, runtime_output_tokens?: integer}` | The result returned by a tool, optionally linked to the calling Item. |
| `subagent.activity` | `{text?: BlobRef, has_images: bool, subagent_session_id?: SessionId}` | Intermediate activity associated with a child Session. |
| `subagent.report` | `{text?: BlobRef, has_images: bool, source_session_id?: SessionId}` | A result returned from a child Session to its parent Agent. |
| `runtime.instructions` | `{text?: BlobRef, category?: project \| user \| skill \| permission \| collaboration \| plugin \| tool_catalog}` | Instructions injected by the Runtime or harness, not Human input. |
| `runtime.context` | `{text?: BlobRef, category?: environment \| memory \| file \| session_reference \| internal, has_images: bool}` | Environment, memory, file, or Session context supplied by the Runtime, not Human input. |
| `runtime.state` | `{text?: BlobRef, has_images: bool}` | Runtime-owned state or a state change placed on the Session timeline. |
| `runtime.notice` | `{text?: BlobRef, has_images: bool}` | A Runtime-owned informational or control event placed on the Session timeline. |
| `runtime.compaction_summary` | `{summary?: BlobRef}` | A Runtime-produced summary that replaces earlier context. |
| `runtime.unknown` | `{text?: BlobRef, has_images: bool}` | Meaningful bounded Runtime content whose more specific stable role is not yet known. |

`ShellFragment` is a nested value, not another public Relation:

Its `text` member remains an inline bounded command-fragment value `{value, full_bytes, estimated_tokens}`. It is part of the structured shell value, not the top-level `value.text` or `value.summary` large-text access path.

```text
ShellFragment {
  text: {value: string, full_bytes: integer, estimated_tokens: integer}
  completeness: complete | partial
  statements: [{
    range: {start, end}
    parent_position?: integer
    connector?: string
    pipeline?: {id, position}
    invocations: [{program, argv: string[]}]
    redirects: [{source_fd?, operator, target, range: {start, end}}]
  }]
}
```

The Blob's published byte length and `runtime_truncated` describe different boundaries. The Runtime can truncate the original tool output before writing the Source; then Trace Index can independently bound the Source text before publishing it. `runtime_output_tokens` describes the Runtime's original-output measurement when reported, while `blobs.full_bytes` and `estimated_tokens` describe the complete text actually present in the Source.

Optional Tool Output fields remain absent when the Runtime supplied no evidence. In particular, a missing duration is not zero, a missing `failed` value is not success, and a missing `call_item_id` does not prove that no call occurred.

## JSON values are typed domain values

SQLite represents arrays and nested structs as JSON text. This encoding does not make their contracts open-ended. Record ranges, Loop ends, Session references, Blob references, and every Semantic variant have defined shapes.

Use SQLite JSON functions for direct projection. For example, `json_each(source_ids)` expands the Sources supporting a Session. A short filter, JSON projection, or grouping over the five Relations does not justify another permanent public Relation.

## Text retrieval

`item_search` is a trigram candidate index, not a domain object. It indexes only Semantic text explicitly selected for retrieval; Runtime bookkeeping and tool output are not implicitly covered. Its `text` column is a contentless FTS `MATCH` operand, not another readable copy of the text: selecting it returns `NULL`. Candidate output is `rowid`. Join that id to `items.item_id`, obtain the BlobRef selected by `semantic.role`, join `blobs`, and exact-check `blobs.text` because trigram matching is case-folded and approximate.

`blobs` and `item_search` are public because they provide access capabilities that the five domain relations do not provide cheaply: demand-driven large-text reads and indexed candidate retrieval. Search results still take their meaning and provenance from `items` and `records`.

## Public contract and private storage

SQLite tables such as `domain_items`, `content_blobs`, `item_records`, or `session_parents` are storage choices. They may deduplicate bounded Semantic text, normalize arrays and sparse attributes, or maintain checkpoints and write-time state, but their private names and shapes are not stable query interfaces. Public `items.semantic` preserves the declared BlobRef, while the public `blobs` access relation owns text lookup. The existence of that access relation does not make Blob a domain entity.

`query run` may read those tables for index-integrity debugging. Ordinary analysis should use the seven Public Relations. A query that succeeds against a private table can still be semantically wrong and can break on the next Storage Format.
