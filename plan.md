# Subagent implementation plan

## Phase 1: Per-run context and policy
- [x] Add per-run prompt, model and iteration configuration without changing existing defaults.
- [x] Filter advertised tools and enforce the same allowlist on execution.
- [ ] Test isolation and compatibility with single-agent runs.

## Phase 2: One child at a time
- [ ] Add a coordinator and delegation tool with separate child sessions and explicit context.
- [ ] Capture structured terminal results; drain streams, enforce limits, and handle failures, timeouts and cancellation.
- [ ] Track parent-child relationships and test read-only isolation.

## Phase 3: Parent delegation context and verification
- [ ] Keep delegation interactions in persistent session history and model context without compaction.
- [ ] Verify complete tool-call/result pairs and provider-compatible message sequences.
- [ ] Test failure reporting, ordering, and parent/child context isolation.

## Phase 4: Parallel delegation
- [ ] Add spawn/wait, concurrency and aggregate budgets, cancellation and isolation tests.

## Phase 5: Evaluate and expand
- [ ] Evaluate quality, latency and cost against single-agent runs.
- [ ] Consider nested agents, sibling messaging and isolated write-capable children only when justified.

## Progress notes

- Per-run options are available on `AgentLoop::run_with_options`; legacy `run` keeps its defaults. Tool denials are recorded as tool results.
- Context compaction has been removed. Delegation interactions will follow the normal tool-call/result history path; verify provider compatibility once delegation is implemented.
- Coordinator and tool are not implemented. The existing tool handler API only receives argument JSON, not the caller's agent/session/turn; extend the tool contract safely before implementing parent-child linkage. Define cancellation, timeout and persistent run metadata before enabling delegation.
- The loop-level tool allowlist is not a security boundary against independent direct use of the shared executor; delegation must use the scoped path and avoid granting child code direct executor access.
