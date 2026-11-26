/* trace-index-template
name = "item-context"
description = "Fetch surrounding items around one anchor item_id across the session timeline (excludes agent.reasoning by default; keeps tool calls/outputs)"
min_format = 2
default_limit = 101

[params.item_id]
type = "item_ref"
required = true
min = 1
description = "Anchor items.item_id (must exist)"

[params.window]
type = "int"
default = 6
min = 0
max = 50
description = "Number of items before and after the anchor to include (up to 2*window+1 rows)"

[params.include_reasoning]
type = "bool"
default = false
description = "Include agent.reasoning items in the neighborhood window"

[params.max_chars]
type = "int"
default = 800
min = 1
max = 65536
description = "Maximum characters returned in preview"
*/
WITH anchor AS (
    SELECT item_id, session_id
    FROM items
    WHERE item_id = :item_id
),
timeline AS (
    SELECT
        i.item_id,
        i.session_id,
        i.loop_id,
        l.session_position,
        i.loop_position,
        i.occurred_at,
        i.record_ids,
        i.semantic,
        ROW_NUMBER() OVER (
            ORDER BY COALESCE(l.session_position, -1), COALESCE(i.loop_position, -1), i.item_id
        ) AS seq_pos
    FROM items AS i
    JOIN anchor AS a ON a.session_id = i.session_id
    LEFT JOIN loops AS l ON l.loop_id = i.loop_id
    WHERE i.session_id = a.session_id
-- #if !include_reasoning
      AND (i.item_id = a.item_id OR json_extract(i.semantic, '$.role') <> 'agent.reasoning')
-- #endif
),
anchor_pos AS (
    SELECT seq_pos
    FROM timeline
    WHERE item_id = :item_id
)
SELECT
    CASE WHEN t.item_id = :item_id THEN 1 ELSE 0 END AS is_anchor,
    t.seq_pos - ap.seq_pos AS offset_from_anchor,
    t.item_id,
    t.session_id,
    s.runtime,
    s.native_id,
    s.working_directory,
    t.loop_id,
    t.session_position,
    t.loop_position,
    t.occurred_at,
    json_extract(t.semantic, '$.role') AS role,
    json_extract(t.semantic, '$.evidence_strength') AS evidence_strength,
    t.record_ids,
    b.full_bytes,
    substr(
        COALESCE(
            b.text,
            json_extract(t.semantic, '$.value.shell_fragments[0].text.value'),
            json_extract(t.semantic, '$.value.tool_name')
        ),
        1,
        :max_chars
    ) AS preview
FROM timeline AS t
CROSS JOIN anchor_pos AS ap
JOIN sessions AS s ON s.session_id = t.session_id
LEFT JOIN blobs AS b
    ON b.blob_id = COALESCE(
        json_extract(t.semantic, '$.value.text.blob_id'),
        json_extract(t.semantic, '$.value.summary.blob_id')
    )
WHERE t.seq_pos BETWEEN ap.seq_pos - :window AND ap.seq_pos + :window
ORDER BY t.seq_pos
