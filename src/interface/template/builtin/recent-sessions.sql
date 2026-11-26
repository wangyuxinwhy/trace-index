/* trace-index-template
name = "recent-sessions"
description = "List recent sessions with loop/item counts, optionally filtered by runtime, working_directory, or time"
min_format = 2
default_limit = 30

[params.runtime]
type = "string"
description = "Filter to one runtime (e.g. codex, claude, pi, antigravity)"

[params.cwd]
type = "string"
description = "Filter sessions whose working_directory contains this substring"

[params.since]
type = "time"
description = "Lower bound on sessions.created_at (e.g. 7d, 24h, epoch_ms, or RFC 3339 with offset)"

[params.until]
type = "time"
description = "Upper bound on sessions.created_at (e.g. 1d, epoch_ms, or RFC 3339 with offset)"
*/
SELECT
    s.session_id,
    s.runtime,
    s.native_id,
    s.name,
    s.working_directory,
    s.created_at,
    (SELECT COUNT(*) FROM loops AS l WHERE l.session_id = s.session_id) AS loop_count,
    (SELECT COUNT(*) FROM items AS i WHERE i.session_id = s.session_id) AS item_count
FROM sessions AS s
WHERE s.session_id IS NOT NULL
-- #if runtime
  AND s.runtime = :runtime
-- #endif
-- #if cwd
  AND instr(COALESCE(s.working_directory, ''), :cwd) > 0
-- #endif
-- #if since
  AND s.created_at >= :since
-- #endif
-- #if until
  AND s.created_at <= :until
-- #endif
ORDER BY s.created_at DESC, s.session_id DESC
