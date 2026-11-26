/* trace-index-template
name = "session-transcript"
description = "Read one session timeline in canonical order (excludes reasoning/tools by default unless enabled or roles is set; loop filters restrict to loop-attached items)"
min_format = 2
default_limit = 100

[params.session]
type = "session_ref"
required = true
description = "Target session (id:<n>, native:<id>, <runtime>:<id>, or unambiguous id / >=8-char native_id prefix)"

[params.tail_loops]
type = "int"
min = 1
description = "Restrict output to the last N loops of the session (drops session-level items without a loop_id)"

[params.from_loop]
type = "int"
min = 0
description = "Lower bound on loops.session_position (inclusive; drops session-level items without a loop_id)"

[params.to_loop]
type = "int"
min = 0
description = "Upper bound on loops.session_position (inclusive; drops session-level items without a loop_id)"

[params.after_item_id]
type = "item_ref"
min = 1
description = "Keyset pagination cursor: return items strictly after this item_id (must belong to session)"

[params.roles]
type = "list"
item_type = "semantic_role"
description = "Explicit comma-separated semantic roles to include (takes precedence over default include_reasoning/include_tools exclusions)"

[params.include_reasoning]
type = "bool"
default = false
description = "Include agent.reasoning items"

[params.include_tools]
type = "bool"
default = false
description = "Include agent.tool_call, agent.tool_call.shell, and tool.output items"

[params.max_chars]
type = "int"
default = 600
min = 1
max = 65536
description = "Maximum characters returned in content"
*/
SELECT
    i.item_id,
    i.session_id,
    s.runtime,
    s.native_id,
    i.loop_id,
    l.session_position,
    i.loop_position,
    i.occurred_at,
    json_extract(i.semantic, '$.role') AS role,
    json_extract(i.semantic, '$.evidence_strength') AS evidence_strength,
    i.record_ids,
    b.full_bytes,
    substr(
        COALESCE(
            b.text,
            json_extract(i.semantic, '$.value.shell_fragments[0].text.value'),
            json_extract(i.semantic, '$.value.tool_name')
        ),
        1,
        :max_chars
    ) AS content
FROM items AS i
JOIN sessions AS s ON s.session_id = i.session_id
LEFT JOIN loops AS l ON l.loop_id = i.loop_id
LEFT JOIN blobs AS b
    ON b.blob_id = COALESCE(
        json_extract(i.semantic, '$.value.text.blob_id'),
        json_extract(i.semantic, '$.value.summary.blob_id')
    )
WHERE i.session_id = :session
-- #if tail_loops
  AND i.loop_id IN (
      SELECT loop_id
      FROM loops
      WHERE session_id = :session
      ORDER BY session_position DESC
      LIMIT :tail_loops
  )
-- #endif
-- #if from_loop
  AND l.session_position >= :from_loop
-- #endif
-- #if to_loop
  AND l.session_position <= :to_loop
-- #endif
-- #if after_item_id
  AND (COALESCE(l.session_position, -1), COALESCE(i.loop_position, -1), i.item_id) > (
      (
          SELECT COALESCE(l2.session_position, -1)
          FROM items AS i2
          LEFT JOIN loops AS l2 ON l2.loop_id = i2.loop_id
          WHERE i2.item_id = :after_item_id
            AND i2.session_id = :session
      ),
      (
          SELECT COALESCE(i2.loop_position, -1)
          FROM items AS i2
          WHERE i2.item_id = :after_item_id
            AND i2.session_id = :session
      ),
      :after_item_id
  )
-- #endif
-- #if roles
  AND json_extract(i.semantic, '$.role') IN (SELECT value FROM json_each(:roles))
-- #endif
-- #if !include_reasoning
  AND (:roles IS NOT NULL OR json_extract(i.semantic, '$.role') <> 'agent.reasoning')
-- #endif
-- #if !include_tools
  AND (:roles IS NOT NULL OR json_extract(i.semantic, '$.role') NOT IN ('agent.tool_call', 'agent.tool_call.shell', 'tool.output'))
-- #endif
ORDER BY COALESCE(l.session_position, -1), COALESCE(i.loop_position, -1), i.item_id
