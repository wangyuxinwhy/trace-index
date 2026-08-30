---
title: Query Indexed Facts
description: Explore public relations with selective, bounded, read-only SQL.
---

# Query Indexed Facts

Trace Index publishes five domain objects and two text-access relations:

```text
sources  records  sessions  loops  items  blobs  item_search
```

Start by discovering the current binary's contract:

```bash
trace-index schema list
trace-index schema get sessions --compact
trace-index schema get loops --compact
trace-index schema get items --compact
trace-index schema get blobs --compact
```

The compact form shows names, SQLite types, and nullability. Load the full schema for one relation when a field's meaning is not yet clear.

Run one bounded, read-only SQL statement:

```bash
trace-index query run \
  "SELECT working_directory, COUNT(*) AS sessions
     FROM sessions
    GROUP BY working_directory
    ORDER BY sessions DESC
    LIMIT 20"
```

For multiline SQL, use stdin or a UTF-8 file rather than fragile shell quoting:

```bash
trace-index query run --stdin <<'SQL'
SELECT s.runtime,
       json_extract(i.semantic, '$.role') AS role,
       COUNT(*) AS items
FROM items i
JOIN sessions s USING (session_id)
GROUP BY s.runtime, json_extract(i.semantic, '$.role')
ORDER BY items DESC;
SQL

trace-index query run --file analysis.sql
```

## Read Semantic values and text references

SQLite stores nested domain values as JSON text. Use SQLite JSON functions instead of treating JSON as an opaque string. Top-level Semantic text is different: `value.text`, or `value.summary` for a compaction summary, contains `{blob_id}` and the bounded text lives in the `blobs` access relation.

For example, human and Agent text is referenced by the `text` member selected by the Item's `semantic.role`:

```sql
SELECT item_id,
       json_extract(semantic, '$.role') AS role,
       b.text,
       b.full_bytes,
       json_extract(semantic, '$.evidence_strength') AS strength
FROM items AS i
JOIN blobs AS b
  ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
WHERE json_extract(i.semantic, '$.role') = 'human.request'
ORDER BY i.occurred_at DESC
LIMIT 20;
```

Use `semantic.role`, `semantic.value`, and `semantic.evidence_strength` for indexed facts. Do not join `blobs` when the query only needs roles, counts, timestamps, tool metadata, or another non-text value. When the task asks about an exact Runtime field that Semantic does not publish, expand the Item's `record_ids` and inspect the supporting Record.

## Restore one Loop

A Loop is one outer execution lifecycle inside a Session. Items structurally assigned to it have a zero-based `loop_position`:

```sql
SELECT loop_position,
       semantic
FROM items
WHERE loop_id = 1
ORDER BY loop_position;
```

Do not infer completion from the last Item. Read the Loop boundary and Runtime-declared outcome:

```sql
SELECT loop_id,
       session_position,
       start_record_id,
       end
FROM loops
WHERE session_id = 1
ORDER BY session_position;
```

`session_position` is the stable Session-level order.

`end IS NULL` means no ending Record has been observed. A non-null `end` always contains `record_id`; its optional `outcome` member is `completed`, `interrupted`, or `failed` when the Runtime stated a result. A missing `outcome` does not imply success or failure.

Model identity and token usage belong to the same Loop boundary:

```sql
SELECT loop_id,
       session_position,
       json_extract(model, '$.id') AS model,
       json_extract(model, '$.effort') AS effort,
       json_extract(model, '$.context_window') AS context_window,
       json_extract(usage, '$.input') AS input_tokens,
       json_extract(usage, '$.cached') AS cached_tokens,
       json_extract(usage, '$.output') AS output_tokens,
       json_extract(usage, '$.input') + json_extract(usage, '$.output') AS total_tokens
FROM loops
WHERE session_id = 1
ORDER BY session_position;
```

`cached` and `cache_write` are already included in `input`; `reasoning` is already included in `output`. Do not add subsets twice. Missing usage means the Runtime did not report enough information, not that the Loop used zero tokens.

Some Items belong to a Session but not to a Loop. For those Items, `loop_id` and `loop_position` are both NULL. Query them explicitly when Session-level context matters:

```sql
SELECT item_id, semantic
FROM items
WHERE session_id = 1 AND loop_id IS NULL
ORDER BY occurred_at, item_id;
```

## Find what a person asked

Do not infer authorship from a Runtime protocol convention. Trace Index publishes that judgment directly as `semantic.role`:

```sql
SELECT s.working_directory,
       i.occurred_at,
       b.text AS request,
       json_extract(i.semantic, '$.evidence_strength') AS strength
FROM items i
JOIN sessions s USING (session_id)
LEFT JOIN blobs b
  ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
WHERE json_extract(i.semantic, '$.role') = 'human.request'
ORDER BY i.occurred_at DESC
LIMIT 20;
```

`human.request` opens a Loop. `human.steering` is human input delivered while a Loop is already open. `semantic.evidence_strength = 'structural'` means the judgment rests on Runtime structure; `heuristic` means it rests on a weaker convention and should be reported as such when attribution matters.

Semantic role is also evidence ownership. When a conclusion says that a person requested, decided, approved, rejected, or corrected something, the supporting text must come from a Human Item. An adjacent `agent.commentary` or `agent.final_answer` Item shows what the Agent proposed; chronological proximity does not turn that proposal into Human confirmation. If the Human response does not clearly accept or reject the proposal, preserve that distinction instead of silently resolving it.

## Report aggregate coverage

Before reporting an aggregate over a scoped Item population, check whether an optional Semantic member or optional relationship prevents some Items from contributing a value. State all three quantities: the intended population, contributors, and missing values. An inner join to the optional value can silently shrink the denominator; missing is not zero, and a contributor count is not a population count. Deliberate search, role, time, and project filters define the scoped population and do not require comparison with excluded Items.

## Inspect the evidence for candidate Sessions

Start Session discovery with the narrow facts the question supplies: Runtime, time, working directory, or matching Item text. After identifying candidate Session ids, inspect their published identity, optional relationships, and physical Sources together:

```sql
SELECT session.session_id,
       session.runtime,
       session.native_id,
       session.created_at,
       session.working_directory,
       session.forked_from,
       session.delegated_from,
       source.source_id,
       source.locator
FROM sessions AS session
LEFT JOIN json_each(session.source_ids) AS support
LEFT JOIN sources AS source ON source.source_id = support.value
WHERE session.session_id IN (1, 2)
ORDER BY session.session_id, source.source_id;
```

`working_directory` describes the Runtime context, while each Source `locator` identifies a physical trace input supporting the Session. These are queryable facts, not an automatic relevance decision. Read a small number of Items relevant to the question from each candidate when the answer depends on what work the Session actually performed.

`session_id` is an opaque Trace Index-local key intended for joins. `native_id` is the identity assigned by the Runtime. When the requested deliverable is a list of Codex, Pi, or Claude Code Sessions, report `runtime` and `native_id`; include `session_id` only when it helps continue SQL investigation.

## Scope by project and time

Materialize a selective Session set before joining Items:

```sql
WITH bounds AS (
  SELECT unixepoch('2026-01-01T00:00:00Z') * 1000 AS start_ms,
         unixepoch('2026-01-02T00:00:00Z') * 1000 AS end_ms
), scoped AS MATERIALIZED (
  SELECT session_id
  FROM sessions
  WHERE working_directory = '/path/to/project'
)
SELECT i.occurred_at,
       json_extract(i.semantic, '$.role') AS role,
       b.text
FROM scoped
CROSS JOIN bounds
JOIN items i USING (session_id)
LEFT JOIN blobs b
  ON b.blob_id = CASE
       WHEN json_extract(i.semantic, '$.role') = 'runtime.compaction_summary'
         THEN json_extract(i.semantic, '$.value.summary.blob_id')
       ELSE json_extract(i.semantic, '$.value.text.blob_id')
     END
WHERE i.occurred_at >= bounds.start_ms
  AND i.occurred_at < bounds.end_ms
ORDER BY i.occurred_at, i.item_id
LIMIT 50;
```

Times are epoch milliseconds. Use half-open intervals for complete dates. `sessions.created_at` and `items.occurred_at` describe domain time; Source synchronization metadata does not prove when an interaction happened.

## Analyze tool calls and outputs as Items

There is no derived tool-call View. Calls and outputs remain separate Items so the timeline stays truthful. A Tool Output points back to its call through `semantic.value.call_item_id` when the Runtime supplied enough evidence to resolve it.

```sql
WITH calls AS (
  SELECT item_id,
         json_extract(semantic, '$.value.tool_name') AS tool_name
  FROM items
  WHERE json_extract(semantic, '$.role')
        IN ('agent.tool_call', 'agent.tool_call.shell')
), outputs AS (
  SELECT item_id,
         json_extract(semantic, '$.value.call_item_id') AS call_item_id,
         json_extract(semantic, '$.value.failed') AS failed,
         json_extract(semantic, '$.value.duration_ms') AS duration_ms
  FROM items
  WHERE json_extract(semantic, '$.role') = 'tool.output'
)
SELECT c.tool_name,
       COUNT(DISTINCT c.item_id) AS calls,
       COUNT(o.item_id) AS outputs,
       SUM(o.failed IS NOT NULL) AS outcomes_reported,
       SUM(o.failed = 1) AS reported_failures,
       ROUND(AVG(o.duration_ms), 1) AS average_duration_ms
FROM calls c
LEFT JOIN outputs o ON o.call_item_id = c.item_id
GROUP BY c.tool_name
ORDER BY calls DESC
LIMIT 20;
```

NULL is an absent Runtime fact, not zero. Report outcome coverage before interpreting failure rates. A call with no linked output may still have happened; the trace may simply lack a resolvable result.

No additional View is needed to find calls that took more than one minute:

```sql
SELECT call.item_id AS call_item_id,
       json_extract(call.semantic, '$.value.tool_name') AS tool_name,
       output.item_id AS output_item_id,
       json_extract(output.semantic, '$.value.duration_ms') AS duration_ms
FROM items AS output
LEFT JOIN items AS call
  ON call.item_id = json_extract(output.semantic, '$.value.call_item_id')
WHERE json_extract(output.semantic, '$.role') = 'tool.output'
  AND json_extract(output.semantic, '$.value.duration_ms') > 60000
ORDER BY duration_ms DESC;
```

This query only includes outputs whose Runtime reported a duration. It does not silently treat unknown durations as zero.

When output size or completeness matters, read both observation boundaries:

```sql
SELECT item_id,
       length(CAST(b.text AS BLOB)) = b.full_bytes AS source_text_complete,
       b.full_bytes AS source_text_bytes,
       b.estimated_tokens AS estimated_source_tokens,
       json_extract(i.semantic, '$.value.runtime_truncated') AS runtime_truncated,
       json_extract(i.semantic, '$.value.runtime_output_tokens') AS runtime_output_tokens
FROM items AS i
JOIN blobs AS b
  ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
WHERE json_extract(i.semantic, '$.role') = 'tool.output'
ORDER BY b.full_bytes DESC
LIMIT 20;
```

`source_text_complete = false` means Trace Index published only a prefix of the text present in the Source. It is derived from `blobs.text` and `blobs.full_bytes` rather than stored independently. `runtime_truncated = true` means the Runtime had already truncated the original tool output before the Source text was written. Either condition can hold independently.

For a shell-aware call, inspect the nested semantic structure with JSON paths rather than joining permanent syntax Views:

```sql
SELECT item_id,
       json_extract(
         semantic,
         '$.value.shell_fragments[0].statements[0].invocations[0].program'
       ) AS first_program
FROM items
WHERE json_extract(semantic, '$.role') = 'agent.tool_call.shell'
LIMIT 20;
```

## Return to physical evidence only when needed

`record_ids` is a non-empty JSON array of the physical Records supporting an Item. Expand it only when the task needs provenance:

```sql
SELECT i.item_id,
       witness.value AS record_id,
       r.source_id,
       r.source_position,
       r.fingerprint
FROM items i
CROSS JOIN json_each(i.record_ids) AS witness
JOIN records r ON r.record_id = witness.value
WHERE i.item_id = 1;
```

The `records` relation contains byte ranges and fingerprints, not raw Runtime JSON. Join `sources` for the locator. Use `record inspect` only for an explicit raw-evidence request or index-integrity debugging.

## Work within query bounds

Start with counts, groups, identities, and bounded snippets. When `complete` is false, the returned rows are only part of the result set. Follow the report's `hint`: narrow the scope, aggregate before fetching rows, return fewer or smaller columns, or continue from a stable, unique `ORDER BY` key. A nonzero `cells_truncated` means selected values are prefixes even when all rows were returned. Raise a budget only for selected material that the claim actually needs.

Public relations are the normal facts for analysis. Internal storage tables are implementation details even if SQLite can technically read them.
