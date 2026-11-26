/* trace-index-template
name = "search-items"
description = "Full-text search candidate items with bounded previews (FTS excludes agent.tool_call, agent.tool_call.shell, and tool.output; verify exact literals)"
min_format = 4
default_limit = 50

[params.query]
type = "string"
required = true
description = "Search terms passed to search_query()"

[params.session]
type = "session_ref"
description = "Filter to one session (id:<n>, native:<id>, <runtime>:<id>, or unambiguous id / >=8-char native_id prefix)"

[params.runtime]
type = "string"
description = "Filter to one runtime (e.g. codex, claude, pi, antigravity)"

[params.roles]
type = "list"
item_type = "searchable_semantic_role"
description = "Comma-separated FTS-indexed semantic roles to include (excludes tool calls and tool.output)"

[params.since]
type = "time"
description = "Lower bound on item occurred_at (e.g. 7d, 24h, epoch_ms, or RFC 3339 with offset)"

[params.until]
type = "time"
description = "Upper bound on item occurred_at (e.g. 1d, epoch_ms, or RFC 3339 with offset)"

[params.contains]
type = "string"
description = "Optional case-insensitive exact substring filter applied to blobs.text"

[params.max_chars]
type = "int"
default = 300
min = 1
max = 65536
description = "Maximum characters returned in preview"
*/
SELECT
    i.item_id AS candidate_item_id,
    i.session_id,
    s.runtime,
    s.native_id,
    i.loop_id,
    i.occurred_at,
    json_extract(i.semantic, '$.role') AS role,
    json_extract(i.semantic, '$.evidence_strength') AS evidence_strength,
    b.full_bytes,
    substr(b.text, 1, :max_chars) AS preview
FROM item_search(search_query(:query)) AS fts
JOIN items AS i ON i.item_id = fts.rowid
JOIN sessions AS s ON s.session_id = i.session_id
LEFT JOIN blobs AS b
    ON b.blob_id = COALESCE(
        json_extract(i.semantic, '$.value.text.blob_id'),
        json_extract(i.semantic, '$.value.summary.blob_id')
    )
WHERE i.item_id = fts.rowid
-- #if session
  AND i.session_id = :session
-- #endif
-- #if runtime
  AND s.runtime = :runtime
-- #endif
-- #if roles
  AND json_extract(i.semantic, '$.role') IN (SELECT value FROM json_each(:roles))
-- #endif
-- #if since
  AND i.occurred_at >= :since
-- #endif
-- #if until
  AND i.occurred_at <= :until
-- #endif
-- #if contains
  AND instr(lower(COALESCE(b.text, '')), lower(:contains)) > 0
-- #endif
ORDER BY bm25(item_search), i.item_id
