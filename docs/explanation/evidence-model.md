---
title: Evidence Model
description: The five domain objects, typed Semantic values, and Record evidence.
---

# Evidence Model

Trace Index turns physical Runtime traces into queryable Agent-program facts without separating those facts from their origin.

```text
Source
└── Record
    └── evidence for Session, Loop, or Item
        └── Item
            ├── Semantic
            │   └── evidence strength
            └── Record evidence
```

## Physical evidence

A Source identifies one physical Runtime input. A Record identifies one complete framed value inside that Source by position, byte range, and BLAKE3 fingerprint. The index does not need to copy the raw payload to prove its origin: `record inspect`, `record verify`, and `record export` return to the original byte range on demand.

Session and Loop boundary fields cite the Records that establish them. Every Item carries a non-empty `record_ids` set. When the Runtime writes two physical representations of one logical fact, both Records support one Item; the index preserves the fact once without losing either witness.

## Semantic fact and physical evidence

An Item publishes one typed Semantic description and cites its physical witnesses.

Semantic defines the stable domain meaning Trace Index commits to as `{role, value, evidence_strength}`. `semantic.role` selects a defined `semantic.value` shape. A request, tool output, delegation, instruction, and compaction summary therefore remain different typed values even though all are Items. In this domain model, present text is bounded `TextContent {value, full_bytes, estimated_tokens}`.

The public SQL encoding substitutes `BlobRef {blob_id}` for top-level TextContent. The `blobs` access relation resolves that reference to bounded text and size metadata only when a query needs the content. This is an access representation, not a second domain Semantic shape: it changes how SQL reads text without adding a Blob entity to the evidence model or detaching the text from its Item and Record evidence.

The Runtime's complete representation remains in the supporting Record. Fields that earn a stable, useful meaning enter Semantic; fields that have not earned that contract are not copied into an untyped Item payload. Exact Runtime or version-specific questions follow `record_ids` to `record inspect`.

## Structural and heuristic judgments

`semantic.evidence_strength` reports the weakest evidence used to produce the Semantic classification.

Structural evidence comes from protocol structure: a typed field, an explicit lifecycle marker, a call/output correlation identity, paired records, or a position established by such facts. Heuristic evidence comes from conventions such as a text marker or a fallback position when the Runtime provides no stronger field.

A heuristic Item is still a published fact with provenance. Its strength tells the Agent where a Runtime change is more likely to invalidate the classification and where raw evidence or Runtime-specific analysis may be warranted.

## Facts and conclusions

Evidence proves what the Source recorded and how Trace Index classified it. It does not prove that the content is relevant, correct, human-authored beyond the stated strength, or sufficient for a broader conclusion. Search matches are candidates; aggregate results inherit the evidence limits of their rows; the Agent forms the final interpretation.

## Storage does not add meaning

SQLite normalizes arrays and sparse attributes into private tables, deduplicates bounded Semantic text, and maintains indexes for performance. Those storage tables support the five domain objects; they do not create additional entities in the evidence model. Public `blobs` and `item_search` are access relations: the former reads referenced text, while the latter finds candidate Items. Both acquire meaning from Item and its Record evidence.
