---
title: Search Literal Text
description: Recover broad memories through bounded literal candidates and confirmation.
---

# Search Literal Text

`item_search` is the text-search access relation. It returns candidate Item ids; it is not a domain object and does not establish an exact match, authorship, Session, or relevance. Its contentless `text` column is only a `MATCH` operand and reads as `NULL`, so it is not another text source. The candidate's meaning comes from `items`, while its bounded text comes from `blobs`.

Choose the search plan from the scope already known:

- For a broad literal of at least three Unicode characters, retrieve trigram candidates from `item_search`.
- For a selective Session, project, role, or time range, narrow Items first and then join only their referenced Blobs.
- Tool output is not in `item_search`; select `tool.output` Items, join their text Blob, and scan that smaller set.
- Queries that do not read text should never join `blobs`.

## Recover a topic from an imprecise memory

When the available input is a partial natural-language recollection rather than a unique identifier, do not turn the whole sentence into one search and do not read every matching conversation. Use a narrowing funnel:

1. Write down the evidence boundary first: relevant Runtime, approximate time, project, actor, and the kind of event or decision the answer needs. Unknown boundaries remain unknown; do not invent them.
2. Derive several short literal anchors from the memory. Prefer names or identifiers, action words, symptoms, and distinctive phrases. Include ordinary wording variants that a person could naturally have used, but do not treat a guessed synonym as evidence.
3. Measure how many Items and Sessions each anchor recalls before fetching text. Drop anchors that are overwhelmingly broad or use them only together with a structural scope.
4. Build a compact candidate table: Item id, Session id, Runtime, time, working directory, Semantic role, matched anchor, and a short snippet. Aggregate by Session first when there are many hits.
5. Compare several plausible Sessions. Look for independent anchors that agree on the same event and reject candidates whose time, project, actor, or surrounding exchange conflicts with the request.
6. Expand only the strongest candidates. Read the matching Item and a small amount of adjacent conversation spine, then widen or page deliberately if the evidence is still incomplete.

A candidate satisfying one literal anchor is not yet a conclusion. If the available scope and surrounding evidence still support more than one candidate, report the ambiguity or ask for a discriminating detail instead of silently choosing one.

### Measure candidate density

`item_search` candidate counts are cheap enough to decide which anchor deserves expansion. This query intentionally does not read Blob text:

```sql
WITH hits(term, item_id) AS (
  SELECT 'package-name', rowid
  FROM item_search(trigram_query('package-name'))
  UNION ALL
  SELECT 'publish failed', rowid
  FROM item_search(trigram_query('publish failed'))
  UNION ALL
  SELECT 'registry timeout', rowid
  FROM item_search(trigram_query('registry timeout'))
)
SELECT h.term,
       COUNT(DISTINCT h.item_id) AS candidate_items,
       COUNT(DISTINCT i.session_id) AS candidate_sessions
FROM hits h
JOIN items i ON i.item_id = h.item_id
GROUP BY h.term
ORDER BY candidate_sessions, candidate_items;
```

These are trigram candidate counts, not exact occurrence counts. Their purpose is query planning.

### Compare compact candidates before reading conversations

After selecting useful anchors, exact-check them and return bounded snippets plus structural scope. Keep Human inputs and Final Answers when they are the best signals for locating the user's remembered topic; choose different roles when the task calls for different evidence.

```sql
WITH hits(term, item_id) AS (
  SELECT 'package-name', rowid
  FROM item_search(trigram_query('package-name'))
  UNION ALL
  SELECT 'publish failed', rowid
  FROM item_search(trigram_query('publish failed'))
), candidates AS (
  SELECT h.term,
         i.item_id,
         i.session_id,
         i.occurred_at,
         json_extract(i.semantic, '$.role') AS role,
         json_extract(i.semantic, '$.value.text.blob_id') AS blob_id
  FROM hits h
  JOIN items i ON i.item_id = h.item_id
  WHERE json_extract(i.semantic, '$.role') IN (
    'human.request',
    'human.steering',
    'agent.final_answer'
  )
)
SELECT c.item_id,
       c.session_id,
       s.runtime,
       s.native_id,
       c.occurred_at,
       s.working_directory,
       c.role,
       c.term AS matched_term,
       substr(b.text, 1, 320) AS snippet
FROM candidates c
JOIN sessions s USING (session_id)
JOIN blobs b USING (blob_id)
WHERE instr(lower(b.text), lower(c.term)) > 0
ORDER BY c.occurred_at DESC, c.item_id DESC
LIMIT 40;
```

If this still returns many rows, replace the final projection with `GROUP BY c.session_id` and count distinct hits per Session before requesting any snippets. Raising `--max-output-bytes` is not the first recovery step.

### Confirm one candidate with adjacent evidence

Once an anchor Item is selected, inspect a bounded neighborhood inside the same Loop instead of dumping the whole Session. The anchor id, number of preceding and following Items, and snippet length are query inputs chosen from the evidence the claim needs; they are not Trace Index defaults. The applicable Semantic roles determine whether an additional role filter is valid.

```sql
WITH parameters(anchor_item_id, before_items, after_items, snippet_chars) AS (
  VALUES (123, 4, 4, 512)
), anchor AS (
  SELECT loop_id, loop_position
  FROM items, parameters
  WHERE item_id = anchor_item_id
)
SELECT i.item_id,
       i.loop_position,
       json_extract(i.semantic, '$.role') AS role,
       substr(b.text, 1, p.snippet_chars) AS text
FROM parameters p
CROSS JOIN anchor a
JOIN items i
  ON i.loop_id = a.loop_id
 AND i.loop_position BETWEEN a.loop_position - p.before_items
                         AND a.loop_position + p.after_items
LEFT JOIN blobs b
  ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
ORDER BY i.loop_position;
```

The values in `parameters` only demonstrate how to make the bound explicit; they are not recommended constants. Add a Semantic-role filter when the requested evidence permits it. Tool calls, Tool Outputs, Runtime context, or raw Records should be included only when the claim being investigated actually depends on them.

If `query run` returns `complete=false`, its rows are a partial result set. Follow the returned `hint`: reduce the projection, aggregate, narrow the scope, or continue from a stable ordered boundary. A nonzero `cells_truncated` means some returned cells are prefixes even when every row was returned.

## Retrieve candidates, then exact-check Blob text

Most text-bearing Semantic values use `value.text = {blob_id}`. A compaction summary instead uses `value.summary = {blob_id}`. Select the reference according to the documented role, then join `blobs`:

```sql
WITH candidates AS MATERIALIZED (
  SELECT rowid AS item_id
  FROM item_search(trigram_query('智能体使用'))
), candidate_items AS (
  SELECT i.item_id,
         i.session_id,
         i.loop_id,
         i.loop_position,
         json_extract(i.semantic, '$.role') AS role,
         CASE
           WHEN json_extract(i.semantic, '$.role') = 'runtime.compaction_summary'
             THEN json_extract(i.semantic, '$.value.summary.blob_id')
           ELSE json_extract(i.semantic, '$.value.text.blob_id')
         END AS blob_id
  FROM candidates c
  JOIN items i USING (item_id)
)
SELECT ci.item_id,
       ci.role,
       ci.loop_id,
       ci.loop_position,
       substr(b.text, max(instr(b.text, '智能体使用') - 160, 1), 512) AS snippet
FROM candidate_items ci
JOIN blobs b USING (blob_id)
WHERE instr(b.text, '智能体使用') > 0
ORDER BY ci.session_id, ci.loop_id, ci.loop_position, ci.item_id
LIMIT 20;
```

The final `instr()` is required. Trigram retrieval is case-folded and deliberately broader than the requested literal.

## Search tool output directly

Tool output is often large and repetitive, so it is not inserted into `item_search`. Scope the Items first, then resolve only their text references:

```sql
WITH outputs AS MATERIALIZED (
  SELECT item_id,
         loop_id,
         json_extract(semantic, '$.value.call_item_id') AS call_item_id,
         json_extract(semantic, '$.value.text.blob_id') AS blob_id
  FROM items
  WHERE session_id = 1
    AND json_extract(semantic, '$.role') = 'tool.output'
)
SELECT o.item_id,
       o.loop_id,
       o.call_item_id,
       substr(b.text, 1, 512) AS snippet
FROM outputs o
JOIN blobs b USING (blob_id)
WHERE instr(b.text, 'needle phrase') > 0
LIMIT 20;
```

The Item selection happens before Blob access. This matters for both clarity and cost: role, Session, time, failure, duration, and other non-text predicates can be evaluated without reading large text.

## Search a selective Item set

For a known project and a short literal, narrow Sessions and Items before joining Blobs:

```sql
WITH scoped_items AS MATERIALIZED (
  SELECT i.item_id,
         i.occurred_at,
         json_extract(i.semantic, '$.role') AS role,
         json_extract(i.semantic, '$.value.text.blob_id') AS blob_id
  FROM sessions s
  JOIN items i USING (session_id)
  WHERE s.working_directory = '/path/to/project'
    AND json_extract(i.semantic, '$.role') IN (
      'human.request',
      'human.steering',
      'agent.final_answer'
    )
)
SELECT si.item_id,
       si.role,
       substr(b.text, 1, 512) AS snippet
FROM scoped_items si
JOIN blobs b USING (blob_id)
WHERE instr(b.text, '知识') > 0
ORDER BY si.occurred_at, si.item_id
LIMIT 20;
```

`item_search` folds case while SQLite `instr()` is case-sensitive. Use `instr(lower(b.text), lower('ExampleName')) > 0` only when the requested exact check should also ignore case.

The index searches only bounded Semantic text selected for retrieval. A miss cannot prove that the literal was absent from every original Record. Follow the Item's `record_ids` and use `record inspect` only when the task explicitly requires raw evidence.
