---
title: Start Here
description: Build an index and answer from its bounded public query plane.
---

# Start Here

This tutorial builds a local index, introduces the five public domain objects, and queries trace facts without guessing the current schema.

## 1. Create a configuration

```bash
trace-index config init
trace-index config show
```

The generated configuration sets a database path and leaves trace roots commented out. Uncomment or add the Codex, Pi, and Claude Code roots you want indexed, or pass files and directories directly to `index sync`.

## 2. Build the index

```bash
trace-index index sync
trace-index index status
```

Synchronization is explicit. Read commands never refresh or modify the index.

## 3. Discover the current contract

Run the version-matched help once, then discover relation and column names:

```bash
trace-index --help
trace-index schema list
trace-index schema get items --compact
trace-index schema get blobs --compact
```

Use `schema get items` or `schema get blobs` without `--compact` when you need column descriptions. The bundled help, documentation, and schema describe the running binary; do not substitute names remembered from another version.

## 4. Understand the five objects

Trace Index publishes five domain objects:

```text
Source ──contains──> Record
one or more Sources ──support──> Session ──contains──> Loop ──contains──> Item
Item ──cites──> one or more physical Records
```

- `sources` identifies physical Runtime Trace inputs.
- `records` identifies complete Runtime records and preserves their verifiable byte ranges.
- `sessions` identifies continuing logical Agent contexts, each supported by one or more Sources.
- `loops` identifies outer execution lifecycles inside Sessions.
- `items` contains the individually queryable facts produced in those contexts.

`blobs` and `item_search` are additional access relations. `blobs` resolves bounded Semantic text by id; `item_search` retrieves candidates over selected text. Neither is another domain object.

## 5. Read Items in a Loop

Items in a Loop have a zero-based `loop_position`:

```bash
trace-index query run --stdin <<'SQL'
SELECT loop_position,
       semantic
FROM items
WHERE loop_id = 1
ORDER BY loop_position;
SQL
```

Some Items belong to a Session without belonging to a Loop. For those rows, `loop_id` and `loop_position` are both null.

To read the high-value conversation spine across a Session, order Loops by `session_position` and Items inside each Loop by `loop_position`:

```sql
SELECT l.session_position,
       i.loop_position,
       json_extract(i.semantic, '$.role') AS role,
       b.text
FROM loops AS l
JOIN items AS i USING (loop_id)
LEFT JOIN blobs AS b
  ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
WHERE l.session_id = 1
  AND json_extract(i.semantic, '$.role') IN (
      'human.request',
      'human.steering',
      'agent.commentary',
      'agent.final_answer'
  )
ORDER BY l.session_position, i.loop_position;
```

## 6. Read Semantic descriptions

Every Item publishes directly stored `semantic = {role, value, evidence_strength}` JSON. This is the stable, typed Trace Index meaning used for queries. The Item's `record_ids` lead to the physical Runtime Records that support that meaning.

`semantic.role` selects the shape of `semantic.value`. In this public SQL encoding, top-level domain TextContent is represented by `BlobRef {blob_id}` rather than repeated inline. Join `blobs` only when the query needs the published string or its size metadata:

```sql
SELECT loop_position,
       json_extract(semantic, '$.role') AS role,
       b.text,
       json_extract(semantic, '$.evidence_strength') AS strength
FROM items AS i
LEFT JOIN blobs AS b
  ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
WHERE i.loop_id = 1
  AND json_extract(i.semantic, '$.role')
      IN ('human.request', 'agent.final_answer')
ORDER BY i.loop_position;
```

Use `semantic` directly for roles and non-text values; such queries should not access `blobs`. If the question depends on an exact Runtime field that the Semantic contract does not publish, expand `record_ids` and inspect the supporting Record. `structural` evidence rests on trace structure; `heuristic` evidence rests on a weaker convention and should be reported accordingly.

## 7. Search text, then confirm it

For a broad literal of at least three Unicode characters, use `item_search` to produce candidates:

```sql
WITH candidates AS MATERIALIZED (
  SELECT rowid AS item_id
  FROM item_search(trigram_query('needle phrase'))
)
SELECT i.item_id,
       json_extract(i.semantic, '$.role') AS role,
       b.text
FROM candidates
JOIN items i USING (item_id)
JOIN blobs b
  ON b.blob_id = CASE
       WHEN json_extract(i.semantic, '$.role') = 'runtime.compaction_summary'
         THEN json_extract(i.semantic, '$.value.summary.blob_id')
       ELSE json_extract(i.semantic, '$.value.text.blob_id')
     END
WHERE instr(b.text, 'needle phrase') > 0
LIMIT 20;
```

The trigram match is case-folded and intentionally broader than exact literal matching. A candidate is not proof of a match: join `rowid` to `items.item_id`, select the BlobRef defined by that Item's `semantic.role`, join `blobs`, and exact-check `blobs.text` with `instr()`.

`item_search` covers only semantic text explicitly selected by the Item contract. For one- or two-character literals, first narrow with `sessions`, `loops`, or `semantic.role`, then join the referenced Blobs and scan that smaller text set with `instr()`.

When the input contains only partial natural-language clues, use several literal anchors, compare compact candidate Sessions against the supplied scope, and expand only supported candidates. Do not dump every hit; preserve ambiguity when the available evidence does not identify one Session. The complete funnel and bounded SQL patterns are in `trace-index docs get how-to/search-literals`.

## 8. Follow an Item to physical evidence

Every Item has a non-empty JSON array of supporting Record ids. Expand it only when the task needs physical provenance:

```sql
SELECT value AS record_id
FROM items, json_each(items.record_ids)
WHERE item_id = 123;
```

Inspect a selected Record when the user asks for its raw representation, exact source evidence, or index-integrity debugging:

```bash
trace-index record inspect 12345
```

For normal analysis, answer from bounded queries over the five public objects and stop once the requested claim is supported.

Continue with `trace-index docs get how-to/query-evidence`, or discover every topic with `trace-index docs list`.
