/* trace-index-template
name = "shell-calls"
description = "Audit agent.tool_call.shell calls paired with tool.output exit_code, failed flag, and duration (program filter is a candidate index over parsed invocations; partial fragments may omit dynamic commands)"
min_format = 2
default_limit = 50

[params.program]
type = "string"
description = "Filter to shell calls invoking this statically parsed program name (partial parse_completeness may miss dynamic invocations)"

[params.session]
type = "session_ref"
description = "Filter to one session (id:<n>, native:<id>, <runtime>:<id>, or unambiguous id / >=8-char native_id prefix)"

[params.runtime]
type = "string"
description = "Filter to one runtime (e.g. codex, claude, pi, antigravity)"

[params.since]
type = "time"
description = "Lower bound on call occurred_at (e.g. 7d, 24h, epoch_ms, or RFC 3339 with offset)"

[params.until]
type = "time"
description = "Upper bound on call occurred_at (e.g. 1d, epoch_ms, or RFC 3339 with offset)"

[params.failed_only]
type = "bool"
default = false
description = "Restrict to shell calls whose paired tool.output reported failed=true or nonzero exit_code"

[params.max_chars]
type = "int"
default = 400
min = 1
max = 65536
description = "Maximum characters returned in command"
*/
SELECT
    call.item_id,
    call.session_id,
    s.runtime,
    s.native_id,
    call.loop_id,
    call.occurred_at,
    json_extract(call.semantic, '$.value.tool_name') AS tool_name,
    json_array_length(call.semantic, '$.value.shell_fragments') AS fragment_count,
    CASE
        WHEN EXISTS (
            SELECT 1
            FROM json_each(call.semantic, '$.value.shell_fragments') AS pf
            WHERE json_extract(pf.value, '$.completeness') <> 'complete'
        ) THEN 'partial'
        ELSE json_extract(call.semantic, '$.value.shell_fragments[0].completeness')
    END AS parse_completeness,
    (
        SELECT length(group_concat(json_extract(cf.value, '$.text.value'), char(10)))
        FROM json_each(call.semantic, '$.value.shell_fragments') AS cf
    ) AS command_chars,
    substr(
        (
            SELECT group_concat(json_extract(cf.value, '$.text.value'), char(10))
            FROM json_each(call.semantic, '$.value.shell_fragments') AS cf
        ),
        1,
        :max_chars
    ) AS command,
    out.item_id AS output_item_id,
    json_extract(out.semantic, '$.value.exit_code') AS exit_code,
    json_extract(out.semantic, '$.value.failed') AS failed,
    json_extract(out.semantic, '$.value.duration_ms') AS duration_ms
FROM items AS call
JOIN sessions AS s ON s.session_id = call.session_id
LEFT JOIN items AS out
    ON json_extract(out.semantic, '$.role') = 'tool.output'
   AND json_extract(out.semantic, '$.value.call_item_id') = +call.item_id
WHERE json_extract(call.semantic, '$.role') = 'agent.tool_call.shell'
-- #if session
  AND call.session_id = :session
-- #endif
-- #if runtime
  AND s.runtime = :runtime
-- #endif
-- #if since
  AND call.occurred_at >= :since
-- #endif
-- #if until
  AND call.occurred_at <= :until
-- #endif
-- #if program
  AND instr(call.semantic, '"program":' || json_quote(:program)) > 0
  AND EXISTS (
      SELECT 1
      FROM json_each(call.semantic, '$.value.shell_fragments') AS f,
           json_each(f.value, '$.statements') AS st,
           json_each(st.value, '$.invocations') AS inv
      WHERE json_extract(inv.value, '$.program') = :program
  )
-- #endif
-- #if failed_only
  AND (
      json_extract(out.semantic, '$.value.failed') = 1
      OR json_extract(out.semantic, '$.value.exit_code') <> 0
  )
-- #endif
ORDER BY call.occurred_at DESC, call.item_id DESC, out.item_id DESC
