---
title: Runtime Adapters
description: Why supported Runtimes share a domain model without one universal Message shape.
---

# Runtime Adapters

Codex, Pi, and Claude Code record similar kinds of work through different protocols. An Adapter translates each protocol into Source, Record, Session, Loop, and Item while keeping the original Runtime representation reachable through Record evidence.

## One Semantic fact from Runtime evidence

For every published Item, the Adapter produces Semantic JSON `{role, value, evidence_strength}`: a Trace Index role, the role's typed value, and the strength of the evidence used to classify it. The Item separately cites the physical Records from which the Adapter derived that fact.

This lets queries depend on a stable Semantic contract without pretending that three Runtime payloads have one body schema. Runtime-specific fields stay in their Source Records. They become Semantic fields only after a real query demonstrates a stable meaning worth supporting.

## Projection needs surrounding Records

Many useful facts cannot be decided from one JSONL line. A Session identity may be stated in a header while its attributes arrive later. A Loop outcome appears after its request. One Item can have user-interface and model-input witnesses. A tool output refers to a call observed earlier. Whether a human message opens work or steers work already under way depends on the current Loop.

Adapters therefore consume an ordered Record stream with bounded state. They publish a Session only after structural identity evidence exists, form Loops only when a supported beginning is observed, correlate multi-Record facts, and leave optional fields absent when the Runtime never supplies their evidence.

## Semantic evidence

The preferred mapping follows Runtime structure: discriminated record types, explicit fields, identities, parent references, lifecycle signals, and correlations. These judgments publish with `structural` evidence strength.

Some supported historical or untagged shapes expose only a text convention or fallback position. The Adapter can still classify them, but publishes `heuristic`. If a classification combines several judgments, the public strength is the weakest one.

The detailed rule names remain implementation diagnostics. The domain contract needs the stable distinction between structural and heuristic evidence, not a permanent vocabulary for every Runtime parser branch.

## Runtime mapping is an implementation boundary

The public model defines Semantic roles and value shapes. It does not define which Codex field, Pi entry type, or Claude Code marker maps to each role. Those rules change with Runtime versions and live beside the Adapter code and tests.

This boundary keeps the model precise rather than vague. Records preserve what the Runtime actually supplied; Semantic states exactly what Trace Index promises; Adapter code documents and tests the current mapping between them.

## Missing and unknown facts

Missing evidence remains null or prevents publication of the larger object. An unsupported Source does not receive an invented Session. A Record without a supported Item meaning remains physical evidence rather than becoming an opaque `runtime.unknown` bag. `runtime.unknown` is reserved for meaningful, bounded content that belongs on the timeline even though its more specific purpose is not yet classified.

The model grows only after a real repeated query needs a new stable meaning. A new Runtime record name by itself does not require a new Semantic role or domain entity.
