---
title: Output Contract
description: Compact JSON, Markdown docs, stderr, and process exit codes.
---

# Output Contract

Machine-readable commands write one compact JSON envelope followed by one newline to stdout. The result is under `data`; fields documented as optional can be omitted.

`docs get` writes Markdown by default. `docs get <TOPIC> --output json` returns the document in the same JSON envelope.

Process and stream behavior:

- Success exits 0 and writes the result to stdout.
- A command-shape or Clap argument error exits 2 and writes usage to stderr.
- A runtime error normally exits 1, writes a diagnostic to stderr, and leaves stdout empty.
- `config check` returns its complete read-only findings on stdout. Error issues make `valid=false` and exit 1; Warning-only reports exit 0 even when `configured_sync_ready=false` because positional `index sync PATHS...` remains valid.
- `index sync` returns the complete synchronization report on stdout and exits 1 when some recognized Sources fail, so callers can inspect both the successful and failed Source results.

Only `index sync` emits progress. Progress is written to stderr and never mixed into the final stdout JSON. `--progress auto|human|ndjson|off` selects the rendering.

## Bounded query results

`query run` bounds execution time, row count, cell size, and serialized row bytes. Its report includes `complete`, optional `incomplete_reason`, optional `hint`, and `cells_truncated`. `complete` states whether every result row was returned; `incomplete_reason` is `row_limit` or `output_budget`. When `complete` is false, `hint` explains why only part of the result was returned and suggests a next query shape. An incomplete report is a successful bounded observation, not an exhaustive result.

`cells_truncated` is a separate boundary. A nonzero value means that many returned text or blob cells contain only a UTF-8-safe prefix because they reached `--max-cell-bytes`; the row set can still have `complete=true`. In that case `hint` explains how to retrieve selected values without mistaking prefixes for complete evidence.

Public values preserve their declared representation. `records.content_range`, nested Session attributes, `source_ids`, `loops.end`, `items.record_ids`, and `items.semantic` are JSON values returned through ordinary SQL cells. Top-level Semantic text is represented inside `items.semantic` by `{blob_id}`; `blobs.text`, `blobs.full_bytes`, and `blobs.estimated_tokens` are ordinary columns read only when the query joins that access relation.

## Physical materialization

Large Record values and inline media appear as structured references. An Asset reference is `<record-id>#<json-pointer>`, where the Record id is `records.record_id`. Byte-exact Record bytes and decoded Assets are written only by explicit export or extraction commands. Those commands refuse to overwrite an existing destination unless `--force` is supplied.

`record verify` reports `verified`, `source_missing`, `source_short`, or `hash_mismatch` in its result. A verification finding is returned as data: a missing or changed Source does not erase already indexed facts. Export and Asset access still require the underlying Record bytes to be present and match their indexed fingerprint.

## Synchronization reports

`index sync` reports the database-wide `indexing_policy` used to publish facts and physical and domain writes under `metrics.writes`: `records`, `sessions`, `loops`, and `items`. `index status` returns the stored policy when one has been established. A changed Source can cause its domain projection to be replaced in full even when only a small physical suffix was appended, so domain write counts need not match `indexed_records`.

`metrics.phases_ms` partitions elapsed time across discovery, preparation, clearing, reading, projection, persistence, fingerprinting, commit, secondary-index work, and an unattributed residual. `metrics.persist_ms` is operational telemetry for current write sites. The names below describe implementation work; they do not add domain objects.

<!-- generated: persist-sites — run `UPDATE_DOCS=1 cargo test generated_documentation` to refresh -->

`item_search`, `items`, `loops`, `other`, `sessions`, `trace_records`

<!-- /generated -->
