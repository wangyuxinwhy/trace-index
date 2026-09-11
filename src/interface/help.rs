//! Shared bootstrap knowledge; frontend syntax stays in each frontend's help.

use std::fmt::Write as _;

use crate::domain::SEMANTIC_ROLES;
use crate::storage::public_schema::{BLOBS, ITEM_SEARCH, ITEMS, LOOPS, RECORDS, SESSIONS, SOURCES};

const CONCEPTS: &str = r"Conceptual model:
  Source ──contains──> Record
  one or more Sources ──support──> Session ──contains──> Loop ──contains──> Item
  Item ──cites──> one or more physical Records

  Source   One physical Runtime trace input.
  Record   One complete Runtime record with a byte range and fingerprint.
  Session  One continuing logical Agent context, supported by one or more Sources.
  Loop     One outer Agent execution lifecycle inside a Session.
  Item     One independently queryable fact from Agent execution and the default query root.

  Loops inside a Session are ordered by session_position;
  Items inside a Loop are ordered by loop_position. A Session-level Item can have
  no Loop. Every Item cites a non-empty record_ids array as its physical evidence.

Semantic model and SQL encoding:
  Each Item has Semantic = {role, value, evidence_strength}. The role selects the
  typed value; evidence_strength is structural or heuristic. items.semantic is
  the stable SQL encoding. Top-level domain TextContent is encoded as {blob_id}
  and resolved through blobs only when a query needs text.";

const INVESTIGATION: &str = r"Session identity:
  sessions.session_id is a Trace Index-local join key. sessions.native_id is the
  Runtime-native identity. When a user asks for a Codex, Pi, or Claude Code
  Session id, report native_id together with runtime; keep session_id for SQL joins.

Query correctly:
  1. Start from items and select facts with semantic.role.
  2. Join sessions or loops when Runtime, context, time, or order defines scope.
  3. Read non-text semantic.value members directly; join blobs only for text.
  4. Follow record_ids only for exact Runtime evidence or index-integrity debugging.
  5. Discover Schema before extracting an unfamiliar JSON shape.

Investigation defaults:
  Orient with the conversation spine.
    When the first question is what a Session was doing, begin with
    human.request, human.steering, agent.commentary, and agent.final_answer.
    Together they give a compact view of the Human's goals and corrections,
    the Agent's stated progress, and the answer delivered at each Loop.

    This is an orientation view, not the complete evidence set. Commentary can
    be provisional or later corrected, and a Final Answer can report an action
    without proving that the corresponding Tool Call succeeded.

  Narrow candidates before expanding text.
    First count or group the candidate population, then constrain it by the
    Runtime, Session, context, time, and Semantic roles relevant to the claim.
    Read the conversation spine for plausible Sessions before retrieving broad
    Tool Outputs, reasoning, or whole-conversation text.

    A text-search hit is only a candidate. Confirm its Session, time, role, and
    exact Blob text before treating it as relevant evidence. A candidate that
    helps exclude a Session can still be useful even when it is absent from the
    final answer.

  Match evidence depth to the claim.
    Use Human roles for what the Human requested, corrected, accepted, or
    rejected. Use Agent commentary and Final Answers for what the Agent said it
    was doing or concluded. Use Tool Calls and Tool Outputs for what was
    attempted and what the tool returned. Inspect physical Records only when an
    exact Runtime field or source representation is required.

  Reconstruct chronology with stable positions.
    Order Loops by session_position and Items by loop_position. Timestamps and
    numeric ids can help identify scope, but they are not substitutes for these
    published ordering fields when reconstructing a Session timeline.

  Preserve the intended population and missing values.
    For a claim about a population, state the denominator before joining an
    optional value or relationship. Report contributing and missing counts
    separately; an inner join can silently redefine the population, and a
    missing value is not zero.

  Verify completeness at each boundary.
    Check whether the query result is complete, whether returned cells or Blob
    text are truncated, and whether the Runtime had already truncated a Tool
    Output. These are independent boundaries. Partial rows or text can support
    a bounded example, but not an exhaustive conclusion unless the missing
    coverage is made explicit.

  Stop when the requested claim is supported.
    Expand from orientation to execution evidence only as far as the question
    requires. Preserve unresolved ambiguity instead of selecting a convenient
    Session, interpretation, or denominator.";

const BOUNDARY: &str = "Operational boundary:
  Reads never synchronize or mutate the index. Synchronize explicitly only
  when newly written traces are required. Export and extraction are explicit
  writes to a selected destination, not automatic consequences of reading.";

/// Returns the shared first-use knowledge without configuration or database I/O.
/// Frontends supply their own discovery syntax and method/command contracts.
#[must_use]
pub fn agent_context() -> String {
    let mut text = format!("{CONCEPTS}\n\nSemantic role vocabulary:\n");
    for role in SEMANTIC_ROLES {
        writeln!(text, "  {}\n    {}", role.as_str(), role.meaning()).expect("write to String");
    }
    let domain = [SOURCES, RECORDS, SESSIONS, LOOPS, ITEMS]
        .map(|relation| relation.name)
        .join(", ");
    let access = [BLOBS, ITEM_SEARCH]
        .map(|relation| relation.name)
        .join(", ");
    write!(text, "\nPublic SQL surface:\n  Domain Relations: {domain}\n  Access Relations: {access}\n\n  Access Relations provide text lookup and candidate retrieval; they are not\n  additional domain objects. Facts and provenance still come from Items and Records.\n\n{INVESTIGATION}\n\n{BOUNDARY}").expect("write to String");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::public_schema::PUBLIC_RELATIONS;

    #[test]
    fn bootstrap_covers_the_owned_vocabulary_and_relations() {
        let text = agent_context();
        for role in SEMANTIC_ROLES {
            assert!(text.contains(&format!("  {}\n    {}", role.as_str(), role.meaning())));
        }
        let exposed = [SOURCES, RECORDS, SESSIONS, LOOPS, ITEMS, BLOBS, ITEM_SEARCH];
        assert_eq!(exposed.len(), PUBLIC_RELATIONS.len());
        for relation in PUBLIC_RELATIONS {
            assert!(exposed.iter().any(|entry| entry.name == relation.name));
        }
        assert!(!text.contains("trace-index "));
        assert!(!text.contains("Index."));
    }
}
