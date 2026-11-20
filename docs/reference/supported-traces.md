---
title: Supported Trace Formats
description: Recognized Codex, Pi, Claude Code, and Antigravity CLI JSONL Sources and edge-case behavior.
---

# Supported Trace Formats

Trace Index recognizes JSONL Sources written by Codex, Pi, Claude Code, and Antigravity CLI. A Source must provide enough Runtime evidence to establish one logical Session identity before domain facts are published.

## Common ingestion behavior

Only complete newline-terminated Records are indexed. A trailing partial line remains outside the indexed prefix until a later append completes it. Record reads are bounded; an oversized Record remains physically addressable and verifiable even when it cannot be projected into an Item.

Malformed or unrecognized Records can remain in the physical Record layer when the surrounding Source is supported. A Record contributes an Item only when the Adapter can publish a meaningful, typed Semantic fact. Runtime lifecycle bookkeeping used solely to establish Session or Loop structure is retained as evidence for those objects rather than duplicated as an Item.

Runtime payloads are not copied into an open-ended Item field. Semantic role and typed value publish facts that have earned a stable query meaning; the public SQL encoding references top-level Semantic text by `{blob_id}` and reads it through `blobs` only when needed. The Item's `record_ids` preserve the path to every exact Runtime witness. Meaningful bounded content can use `runtime.unknown` when it belongs on the timeline but its more specific purpose is not yet classified. An opaque or merely new Runtime record type remains a Record until a real query earns another Semantic contract.

## Codex

Codex rollout Sources are recognized from their `session_meta`, `response_item`, and `event_msg` structures. A structural Session header establishes identity. Runtime lifecycle records establish Loops when their start is present. Paired user-interface and model-input records can support one Item with multiple Record witnesses instead of producing duplicate Items.

Codex supplies structural fields for several important judgments, including explicit lifecycle states and phased agent output. Older or untagged shapes sometimes require a fallback convention; those Semantic Items are marked `heuristic` rather than being presented as equally strong evidence.

Legacy copied forks are a query-scope exception. Their child Source physically repeats the parent's history before writing new work, but those older Sources do not publish an explicit boundary between inherited and newly occurring Records. Trace Index therefore retains the repeated Items in the child Session instead of guessing a boundary from equal text. A query that aggregates activity across a fork lineage can count that inherited prefix more than once; keep Session scope visible, follow `forked_from`, and inspect Record evidence when lineage-wide totals matter. Current paginated forks and reverted rollouts use producer-declared history boundaries and do not require text-based inference.

## Pi

Pi Sources begin from a `session` header followed by typed entries. The header establishes the Session. Pi does not provide one universal native outer-execution id, so the Adapter recognizes a Loop from the human request that starts work and uses the assistant entry's Runtime stop reason for the observed end and outcome.

Pi records parent identities and Runtime-specific message types in its entry structure. The Adapter uses those fields as evidence but does not copy them into every Item. Where the protocol exposes only a user wire role rather than stronger authorship evidence, the resulting human classification is heuristic.

## Claude Code

Claude Code Sources carry a Session identity in their records. A subagent Source names its own agent identity and projects to a separate Session with `delegated_from` pointing to the launching Session. Runtime prompt and completion structures provide Loop boundaries and outcomes when present.

Claude Code Records can form a parent-linked tree when a conversation is rewound. That Runtime topology remains in the Records and Adapter logic; it does not add another cross-Runtime domain entity. Subagent tasks, activity, and reports are published through their Semantic Item shapes and Session references.

## Antigravity CLI

Pass the local `~/.gemini/antigravity-cli/brain` root explicitly, or pass a CLI transcript file with its original `brain/<conversation-uuid>/.system_generated/logs/` layout. The UUID comes from that [producer-defined path](https://antigravity.google/docs/hooks). The Runtime key is `antigravity`. The Adapter accepts the CLI transcript shape observed in version 1.5.13. It reads the consolidated `transcript_full.jsonl` or `transcript.jsonl`; recursive discovery ignores chunk copies in the logs directory. If both variants are supplied, it selects the full transcript. If the selected full transcript could not be indexed and no full transcript has been indexed for that Session, pass the short file on its own until the full file is ready. A full transcript with an indexed complete prefix retains precedence even when its last line is incomplete. A later successful sync of the full transcript replaces an indexed short transcript atomically. Record IDs and locators refer to the selected Source.

Explicit user inputs start heuristic Loop groupings for navigation. Planner text is searchable as heuristic commentary. Loop counts measure those groupings, whose end and outcome stay absent. Model names, reasoning text and tool-call arguments come from planner steps. A declared `run_command` with `CommandLine` supplies the planned Shell syntax and any working directory declared in the call. Execution and its actual working directory require separate evidence. Generic steps and checkpoints remain Records. Session creation time, Session working directory, output-to-call links, exit codes and usage are absent from the projection. Use `items.occurred_at` for event-time filters. A short transcript may already contain producer-truncated text or arguments. Blob byte counts describe the text actually retained in the selected Source.

This input scope covers local CLI transcript files. The desktop and IDE use the same path suffix, so establish that an explicitly supplied or relocated transcript came from the CLI before indexing it. Conversation databases, desktop and IDE stores, and headless stdout event streams are outside the supported input contract.

## Runtime evolution

Runtime field names and shapes vary by version. Adapters are maintained against the Runtime implementation and validated against real Sources. The public contract defines the resulting Semantic facts, not every Runtime mapping. Callers use Semantic for supported queries and inspect supporting Records when a question depends on a Runtime-specific field or version.

An absent field remains absent. Trace Index does not infer process exit codes, duration, failure, completion, or text that the Runtime did not report. This keeps missing evidence distinguishable from a zero or successful result.
