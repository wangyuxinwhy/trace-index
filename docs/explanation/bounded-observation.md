---
title: Bounded Observation
description: Resource budgets, truncation facts, and explicit materialization.
---

# Bounded Observation

Observing traces must not become a dominant runtime, memory, disk, or Agent-context cost.

Every potentially unbounded surface owns a budget appropriate to its shape:

- Indexing retains at most configured bytes for one Record and one projected text value.
- SQL bounds execution time, row count, cell bytes, and total serialized row bytes.
- Record inspection preserves structure while replacing large leaves with references.
- Byte-exact Records and decoded Assets go to explicit files, not normal stdout.
- Index progress goes to stderr and is optional.

Truncation and incompleteness are facts. Output reports whether every requested row was returned and how many cells were truncated. The tool prevents accidental resource loss; the Agent decides whether to narrow a query or explicitly raise a limit.

The Item's domain Semantic text is bounded before it is stored and indexed. Its public SQL encoding in `items.semantic` keeps only a `BlobRef {blob_id}` for each top-level text member. The public `blobs` access relation publishes the bounded `text`, while `full_bytes` and `estimated_tokens` describe the complete Source text before the bound was applied. Comparing the UTF-8 byte length of `blobs.text` with `blobs.full_bytes` tells whether the text is complete without storing a second boolean that could disagree. The complete Runtime representation remains recoverable from the supporting Record in the Source, so a bounded Blob is an observation limit rather than a claim that the original content was short.

Tool output can cross two independent boundaries. A Runtime may truncate the original process output before it writes a Source Record; `runtime_truncated` and `runtime_output_tokens` preserve that report when available. Trace Index may then bound the text it publishes; the byte length of `blobs.text` compared with `blobs.full_bytes` expresses that second boundary. Keeping them separate prevents a 64 KiB indexed prefix from being mistaken for either the whole Source value or the process's original output.

Blob access is demand-driven. Role counts, timelines, Loop usage, tool duration, failure analysis, and other non-text queries read `items.semantic` without joining `blobs`. Text queries first narrow Items, then resolve only the referenced Blobs they need.

The deterministic text estimator is deliberately separate from model usage. It estimates corpus size with one stable rule over complete Source text. `loops.usage` instead preserves normalized token counts reported by model executions. The two values answer different questions and must not be compared as if they shared a tokenizer or observation boundary.

Full-text search is bounded by scope rather than by size alone: only Semantic text explicitly selected by the Item contract enters `item_search`. Tool output is not included. A search miss therefore means no indexed candidate was found, not that every raw Record value was scanned.

This is why base64 image payloads are not returned as ordinary model text. The normal representation carries media type, encoded and decoded sizes, hash, and a Record/JSON-Pointer reference. The Agent can inspect that reference and explicitly extract the image when visual evidence is actually needed.
