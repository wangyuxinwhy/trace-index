/* trace-index-template
name = "search-sessions"
description = "Full-text search candidate items aggregated by session (no blob reads; counts are candidate hits; FTS excludes agent.tool_call, agent.tool_call.shell, and tool.output)"
min_format = 4
default_limit = 30

[params.query]
type = "string"
required = true
description = "Search terms passed to search_query()"

[params.runtime]
type = "string"
description = "Filter to one runtime (e.g. codex, claude, pi, antigravity)"

[params.cwd]
type = "string"
description = "Filter sessions whose working_directory contains this substring"

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

[params.min_hits]
type = "int"
default = 1
min = 1
description = "Minimum candidate item hits required per session"
*/
WITH hits AS (
    SELECT
        i.session_id,
        COUNT(*) AS candidate_items,
        MIN(i.occurred_at) AS first_occurred_at,
        MAX(i.occurred_at) AS last_occurred_at
    FROM item_search_terms(search_query(:query)) AS fts
    JOIN items AS i ON i.item_id = fts.rowid
    WHERE i.item_id = fts.rowid
-- #if roles
      AND json_extract(i.semantic, '$.role') IN (SELECT value FROM json_each(:roles))
-- #endif
-- #if since
      AND i.occurred_at >= :since
-- #endif
-- #if until
      AND i.occurred_at <= :until
-- #endif
    GROUP BY i.session_id
    HAVING COUNT(*) >= :min_hits
)
SELECT
    s.session_id,
    s.runtime,
    s.native_id,
    s.name,
    s.working_directory,
    h.candidate_items,
    h.first_occurred_at,
    h.last_occurred_at
FROM hits AS h
JOIN sessions AS s ON s.session_id = h.session_id
WHERE s.session_id = h.session_id
-- #if runtime
  AND s.runtime = :runtime
-- #endif
-- #if cwd
  AND instr(COALESCE(s.working_directory, ''), :cwd) > 0
-- #endif
ORDER BY h.candidate_items DESC, h.last_occurred_at DESC, s.session_id DESC
