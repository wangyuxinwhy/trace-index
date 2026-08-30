---
title: Inspect and Export Physical Evidence
description: Audit or explicitly materialize exact Records and Assets on demand.
---

# Inspect and Export Physical Evidence

Physical inspection is an on-demand audit path, not the normal query workflow. Use it when the user explicitly asks for raw Runtime evidence, a byte-exact export, an inline Asset, or index-integrity verification.

## Find the Record witnesses for an Item

Every Item has a non-empty `record_ids` JSON array. The array names the physical Records that support the Item; its order has no domain meaning.

```bash
trace-index query run --stdin <<'SQL'
SELECT i.item_id,
       json_extract(i.semantic, '$.role') AS role,
       witness.value AS record_id,
       r.source_id,
       r.source_position,
       r.content_range,
       r.fingerprint
FROM items i
CROSS JOIN json_each(i.record_ids) AS witness
JOIN records r ON r.record_id = witness.value
WHERE i.item_id = 1
ORDER BY r.source_id, r.source_position;
SQL
```

Join `sources` when the audit also needs the original locator and Runtime:

```sql
SELECT r.record_id,
       s.runtime,
       s.locator,
       r.source_position,
       r.content_range,
       r.fingerprint
FROM records r
JOIN sources s USING (source_id)
WHERE r.record_id = 12345;
```

`records` deliberately publishes provenance metadata, not raw JSON. Items likewise do not copy an open-ended Runtime payload. Use Semantic for supported facts, join a Semantic BlobRef to `blobs` only when bounded text is needed, and follow `record_ids` to the Source-backed Record when the exact Runtime representation matters.

Record ids are opaque references in the currently published index. The durable evidence is the Source locator, byte range, and fingerprint, not an assumption that the same numeric id will survive every rebuild.

## Inspect a source-backed Record

```bash
trace-index record inspect 12345
```

Inspection reads the saved byte range from the original Source and verifies its BLAKE3 fingerprint. The returned representation preserves JSON structure while replacing inline media and large scalar values with `$trace_ref` objects so normal stdout stays bounded.

This command depends on the original Source still being present. Trace Index is a rebuildable derivative, not an archive.

## Verify without displaying the Record

```bash
trace-index record verify 12345
```

Verification compares the current source bytes with the indexed byte length and fingerprint without returning the Record representation. Use it when the question is whether the physical witness has changed.

The result is data rather than an I/O-shaped failure: `verified` means the current bytes still match, `source_missing` means the locator no longer exists, `source_short` means it no longer reaches the indexed range, and `hash_mismatch` means bytes exist at that range but have changed. The latter three results do not invalidate the already indexed Item; they state that the original physical witness can no longer be reverified from its recorded locator.

## Export byte-exact Record bytes

```bash
trace-index record export 12345 --output /tmp/record.json
```

Export writes the verified Record bytes to the explicit destination. It does not send a large raw Record through normal stdout and does not overwrite an existing file unless `--force` is supplied.

## Extract an inline Asset

`record inspect` represents inline media with references shaped as `<record-id>#<json-pointer>` and reports its media type, encoding, and encoded byte length. When the media content is actually needed, decode it to an explicit destination:

```bash
trace-index asset extract \
  '12345#/payload/content/1/image_url' \
  --output /tmp/image.png
```

Extraction also refuses to overwrite an existing file unless `--force` is supplied. Keep the reference returned by `record inspect`; do not construct a JSON Pointer by guessing the Runtime's current record shape.
