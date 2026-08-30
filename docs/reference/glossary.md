---
title: Glossary
description: Canonical meanings of Runtime, Adapter, Source, Record, Session, Loop, and Item.
---

# Glossary

Trace Index uses these terms consistently across its domain model, public SQL contract, CLI, and implementation.

## Adapter

The implementation component that recognizes one Runtime's trace format and projects an ordered Record stream into the Trace Index domain model. An Adapter produces typed Semantic facts and cites the Records that support them. Runtime field names, version differences, and mapping rules belong to the Adapter rather than to the domain contract.

## Agent

The intelligent consumer that scopes an investigation, queries indexed facts, checks evidence when needed, and forms conclusions. Trace Index returns facts and provenance; the Agent remains responsible for relevance and interpretation.

## Asset

A large or inline value in a physical Record that is represented by a reference instead of being placed directly in Agent context. The reference identifies the Record and JSON Pointer needed for explicit inspection or extraction.

## Evidence

The physical Records that support a published fact. Every Item has a non-empty set of Record references. Following a reference reaches the Source byte range and fingerprint used to verify what the Runtime actually wrote.

## Evidence Strength

How the Semantic classification was established. `structural` means the judgment follows from Runtime structure, such as a typed field, paired records, or an explicit position in an execution lifecycle. `heuristic` means at least part of the judgment relies on a convention such as a text marker or fallback position. Strength describes the classification evidence, not whether the Item is relevant or true.

## Item

One independently queryable fact in the Agent program. An Item belongs to a Session and may belong to a Loop. It publishes a typed Semantic description and cites every Record that physically supports the fact.

## Loop

One outer Agent execution lifecycle inside a Session. A Loop starts from a structurally recognized beginning. Optional `loops.end` contains the ending Record and, when stated by the Runtime, the outcome; the observed trace can stop before either appears.

## Public Relation

A stable SQL View or virtual table exposed through the bounded Query Plane. Five Public Relations publish the domain objects; `blobs` and `item_search` provide text access without adding domain entities. Storage tables remain readable for index-integrity debugging, but their names and shapes are implementation details.

## Query Plane

The bounded, read-only SQL interface used to query Public Relations. It limits time, rows, cell size, and total output while leaving query strategy and interpretation to the Agent.

## Record

One complete framed record in a Source. For the supported JSONL formats this is one complete line. A Record has a Source position, byte range, BLAKE3 fingerprint, and optional Runtime occurrence time. The original bytes remain in the Source.

## Runtime

An external Agent application and trace protocol. Codex, Pi, and Claude Code are the currently supported Runtimes.

## Semantic

Trace Index's cross-Runtime domain description of an Item: `{role, value, evidence_strength}`. `role` determines which `value` shape is valid. In the domain model, a present text member is bounded `TextContent {value, full_bytes, estimated_tokens}`. The public `items.semantic` column is the stable SQL encoding of that Semantic; it substitutes `BlobRef {blob_id}` for top-level TextContent and leaves the other typed members unchanged.

## BlobRef

An opaque `{blob_id}` reference used by the SQL encoding of top-level Semantic text. It joins to the public `blobs` access relation for bounded text and size metadata. BlobRef is part of the SQL access contract, not the domain Semantic shape; Blob is not a Trace Index domain entity.

## Semantic Role

The discriminant that states what an Item means, such as `human.request`, `agent.final_answer`, or `tool.output`. The dotted name keeps authorship and purpose together without exposing two independently mutable fields.

## Session

One continuing logical Agent context identified by a Runtime. Resume can add another Source to the same Session. Optional `forked_from` and `delegated_from` attributes point to another Session together with the Record that states the relationship.

## Source

One physical Runtime trace input accepted by Trace Index, normally an append-only JSONL file. A Source has an opaque identity, locator, and Runtime. It contributes physical Records and can support one logical Session.

## Storage Format

The compatibility version of the rebuildable SQLite derivative. It versions private storage, not a Runtime protocol or the domain model. A mismatch requires rebuilding the index from Sources.
