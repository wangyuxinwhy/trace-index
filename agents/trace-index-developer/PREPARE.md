# Local binding

This role is prepared after the owner's AgentID creation approval. The requesting Agent executes its approved request with its own credential, opens the sealed initial credential using the retained request key, and saves that credential outside the repository with mode 600. Never print the credential or sealed result into a transcript.

The ignored `agent.json` has `issuer`, `name` (the permanent canonical name, no `id`), and `credential`; the credential is a `file:` pointer to the role's own existing file under `/Users/bytedance/.config/agentid/credentials/<issuer-host>/` (its filename may still contain the old `ag_` id; do not move or recreate it). `workspace/bin/token` is the unified name-only template from agent-net `agents/_template` (activated in the 2026-09-07 AgentID/AgentNet API-2 cutover; no old-client-ID grace period) and caches tokens under `~/.cache/agentid/tokens/<issuer-host>/<name>/<audience>.json`. It reads this binding, not another role's configuration.

AgentNet admission is a separate operator action. Start the runtime in this repository, explicitly instruct it to read `agents/trace-index-developer/AGENT.md`, and verify its self-check and actual task reply. Readiness must not be inferred solely from a successful HERDR start.
