---
layout: home

hero:
  name: Trace Index
  text: Query Agent histories as evidence
  tagline: A bounded, source-traceable fact plane for Codex, Pi, and Claude Code traces.
  actions:
    - theme: brand
      text: Start Here
      link: /start-here
    - theme: alt
      text: Install
      link: /how-to/install

features:
  - title: Source-traceable
    details: Every Item retains the Record ids needed to return to the physical Runtime trace.
  - title: Bounded by default
    details: SQL execution, returned rows, cell sizes, and serialized output have explicit limits.
  - title: Runtime-faithful
    details: Typed Semantic facts stay linked to the exact Runtime Records that support them.
  - title: Version-matched
    details: Help, operational documentation, and the public Schema ship inside the installed binary.
---

# Trace Index

Agent Runtimes persist detailed traces so work can continue, but those files are difficult to investigate later. A question may cross many Sources and three different Runtime formats, while a useful answer still needs to show where every fact came from.

Trace Index builds a local, rebuildable SQLite derivative from Codex, Pi, and Claude Code JSONL Sources. It exposes five domain objects through bounded read-only SQL and keeps a path from projected facts back to their physical Records.

Trace Index provides facts and provenance. The investigating Agent still chooses the scope, compares evidence, handles uncertainty, and forms the conclusion. For one known file and one literal, `rg` can be simpler; the index is most useful when an investigation crosses Sources, Sessions, Runtimes, or time ranges.

## Where to start

Run `trace-index --help` first. It establishes the complete conceptual model every query depends on: Source, Record, Session, Loop, Item, Semantic meaning, physical evidence, the public SQL surface, and the read/write boundary.

Then choose the path that matches the current question:

- Follow [Start Here](start-here.md) for the one complete first-use workflow.
- Open [Query Indexed Facts](how-to/query-evidence.md) when the index already exists and the task is to write SQL.
- Use [Public SQL Schema](reference/public-schema.md), or the installed binary's `schema list/get`, for the exact query contract.

Text search, Runtime-specific interpretation, and physical evidence inspection are conditional workflows. Their focused guides keep those details available without making every reader absorb them up front.

## Responsibility boundary

Trace Index can establish which physical and program facts are currently published, where they occurred, which Semantic classification was assigned, how strong that classification evidence is, and which Records support the fact.

It does not decide whether two differently worded statements mean the same thing, whether a pattern is important, whether the indexed corpus is sufficient for a claim, or what final conclusion should be reported. When a question depends on an exact Runtime representation rather than a published Semantic fact, the Agent follows the Item's Record evidence and inspects the Source bytes.

## Continue reading

- Follow [Start Here](start-here.md) for the first complete workflow.
- Use [Query Indexed Facts](how-to/query-evidence.md) for direct SQL over the five objects.
- Use [Search Literal Text](how-to/search-literals.md) for candidate retrieval and exact confirmation.
- Read the [Evidence Model](explanation/evidence-model.md) for Semantic facts and Record evidence.
- Read [Runtime Adapters](explanation/runtime-adapters.md) before comparing Runtime behavior.
- Use the [Public SQL Schema](reference/public-schema.md) for the stable Relation contract.

The website is the human learning path. For exact behavior of an installed version, prefer its bundled help, docs, and Schema.
