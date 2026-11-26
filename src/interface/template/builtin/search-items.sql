/* trace-index-template
name = "search-items"
description = "Full-text search candidate items with match-centered bounded previews (FTS excludes agent.tool_call, agent.tool_call.shell, and tool.output; verify exact literals)"
min_format = 4
default_limit = 50

[params.query]
type = "string"
required = true
description = "Search terms passed to search_query()"

[params.session]
type = "session_ref"
description = "Filter to one session (id:<n>, native:<id>, <runtime>:<id>, or unambiguous id / >=8-char native_id prefix)"

[params.exclude_session]
type = "session_ref"
description = "Exclude one already-indexed session (id:<n>, native:<id>, <runtime>:<id>, or unambiguous id / >=8-char native_id prefix)"

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
description = "Optional case-insensitive exact substring filter applied to blobs.text (also anchors preview centering)"

[params.sort]
type = "enum"
values = ["relevance", "recent"]
default = "relevance"
description = "Ordering mode: relevance (FTS5 BM25 rank) or recent (occurred_at DESC, item_id DESC)"

[params.max_chars]
type = "int"
default = 300
min = 1
max = 65536
description = "Maximum Unicode characters returned in preview (inclusive of ellipsis markers)"
*/
-- #if sort=recent
WITH matched AS MATERIALIZED (
-- #else
WITH matched AS (
-- #endif
    SELECT
-- #if sort=relevance
        fts.rank AS fts_rank,
-- #endif
        i.item_id AS candidate_item_id,
        i.session_id,
        i.loop_id,
        i.occurred_at,
        json_extract(i.semantic, '$.role') AS role,
        json_extract(i.semantic, '$.evidence_strength') AS evidence_strength,
        COALESCE(
            json_extract(i.semantic, '$.value.text.blob_id'),
            json_extract(i.semantic, '$.value.summary.blob_id')
        ) AS blob_id
    FROM item_search(search_query(:query)) AS fts
    JOIN items AS i ON i.item_id = fts.rowid
-- #if runtime
    JOIN sessions AS s_filter ON s_filter.session_id = i.session_id
-- #endif
    WHERE i.item_id = fts.rowid
-- #if session
      AND i.session_id = :session
-- #endif
-- #if exclude_session
      AND i.session_id <> :exclude_session
-- #endif
-- #if runtime
      AND s_filter.runtime = :runtime
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
-- #if sort=recent
    ORDER BY i.occurred_at DESC, i.item_id DESC
-- #endif
)
SELECT
    m.candidate_item_id,
    m.session_id,
    s.runtime,
    s.native_id,
    m.loop_id,
    m.occurred_at,
    m.role,
    m.evidence_strength,
    b.full_bytes,
-- #if contains
    search_snippet(b.text, :query, :max_chars, :contains) AS preview
-- #else
    search_snippet(b.text, :query, :max_chars) AS preview
-- #endif
FROM matched AS m
CROSS JOIN sessions AS s ON s.session_id = m.session_id
LEFT JOIN blobs AS b ON b.blob_id = m.blob_id
-- #if contains
WHERE instr(lower(COALESCE(b.text, '')), lower(:contains)) > 0
-- #endif
-- #if sort=relevance
ORDER BY m.fts_rank
-- #endif
