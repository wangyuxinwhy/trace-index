# Trace Index Repository Guidance

## Agent-facing disclosure

- Treat `trace-index --help` as the Agent's bootstrap context, not merely a command index.
- Disclose every high-probability prerequisite needed to use the CLI correctly on the first Help screen, even when that makes root Help longer than a conventional human-oriented CLI.
- For Trace Index, the complete conceptual model, object relationships, default query entry point, Semantic role vocabulary, evidence path, public-versus-access Relation boundary, discovery path, and read-versus-write boundary are prerequisite knowledge.
- Apply progressive disclosure only to conditional knowledge: task-specific query patterns, complete Semantic value shapes, text-search mechanics, Shell structure, Runtime mappings, configuration precedence, export details, and exceptional recovery paths.
- Optimize Help for relevance and correctness, not minimum length. Simplicity means removing unrelated information, not withholding concepts required for correct use.

## Knowledge ownership

- Keep one primary owner for each contract. Clap owns the command tree and exact flags; public Schema declarations own Relation names, columns, types, nullability, and descriptions; domain types own Semantic meaning; bundled Markdown owns tutorials, task procedures, reference explanation, and design rationale.
- Generate or validate repeated contract fragments instead of maintaining independent hand-written copies.
- Root Help establishes the stable conceptual whole. Leaf Help owns exact arguments, defaults, limits, side effects, and the smallest command-specific example. Bundled docs own conditional detail and extended workflows.
- `docs/start-here.md` is the one complete first-use tutorial. README and the website landing page orient readers and route them there instead of duplicating it.
- Code comments explain local responsibilities, invariants, evidence requirements, and non-obvious reasons. Do not copy product tutorials or broad domain articles into implementation comments.
- Runtime-specific mapping knowledge stays with Adapter code and focused implementation evidence unless it changes the public domain contract.

## Documentation changes

- Before adding or splitting a page, identify its reader question and why an existing page cannot answer it clearly. Do not split solely because a page is long.
- Preserve the distinction between the domain model and its SQL representation. In particular, domain Semantic text is `TextContent`; top-level `BlobRef` values belong to the public SQL access encoding.
- Keep historical experiments outside the bundled operational learning path.
- Keep one English Agent-facing documentation contract. Do not add translated mirrors that duplicate the same operational knowledge. Generated regions must be refreshed through their declared generator rather than edited by hand.

## Agent-facing generalization review

- After materially changing Agent-visible Help, bundled documentation, examples, or Skills, and before freezing an experiment or committing the change, run an independent read-only review for evaluation contamination and overfitting.
- Use a fresh subagent without the design conversation as context. Give it the changed Agent-visible surfaces, relevant task Prompts and evaluator-only references, and observed failure traces when the change was motivated by an experiment.
- The reviewer must distinguish direct leakage from soft overfitting. Check for task-specific names, dates, ids, answers, counts, and discriminating clues; also check whether a supposedly general rule merely restates one observed failure, embeds unexplained magic parameters, or adds complexity that other realistic workflows do not need.
- Prefer general invariants and parameterized examples over incident-shaped wording and fixed recipes. A search hit being a candidate, evidence scope governing relevance, and ambiguity remaining explicit are general invariants; a particular neighborhood size or one benchmark's failure sequence is not.
- Treat the review as a real gate. Resolve every material finding or record why the rule is independently justified by the public contract or multiple workflows. A clean keyword scan alone is insufficient.
- Any post-run change to the Agent-visible evaluation path invalidates direct comparison with earlier Runs. Freeze the revised path and rerun every affected arm; never rescore an old output as though it had seen the new guidance.

## Verification

- Test the compiled CLI as an external process after Help or interface changes.
- Keep generated Markdown contract regions current.
- Run Rustdoc with private items and warnings denied so broken links and stale code documentation fail verification.
- Treat Help, bundled docs, Schema discovery, the Trace Index skill, and machine-readable output as one version-matched Agent-facing contract.
