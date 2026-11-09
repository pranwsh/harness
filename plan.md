# Subagent implementation plan

## Phase 1: Per-run context and policy
- [x] Add per-run prompt, model and iteration configuration without changing existing defaults.
- [x] Filter advertised tools and enforce the same allowlist on execution.
- [ ] Test isolation and compatibility with single-agent runs.

## Phase 2: One child at a time
- [x] Add a delegation tool with separate child sessions and explicit context (currently coordinated in agent-loop).
- [ ] Capture structured terminal results; drain streams, enforce limits, and handle failures, timeouts and cancellation.
- [ ] Track parent-child relationships and test read-only isolation.

## Phase 3: Parent delegation context and verification
- [x] Keep delegation interactions in persistent session history and model context without compaction.
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
- Synchronous `delegate_task` is implemented with caller-aware handlers, fresh child sessions, read-only tool names, a 120-second timeout, and bounded output. Parent-child IDs are recorded in the parent tool result; standalone durable run metadata and UI are not implemented. Parent turn cancellation is not exposed publicly; dropped delegation futures signal child cancellation. Timeout and cancellation require further end-to-end tests.
- Parallel delegation, aggregate budgets, restart recovery and real-provider validation remain unimplemented.
- The loop-level tool allowlist is not a security boundary against independent direct use of the shared executor; delegation must use the scoped path and avoid granting child code direct executor access.
