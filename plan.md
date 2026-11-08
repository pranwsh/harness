# Subagent implementation plan

## Phase 1: Per-run context and policy
- [ ] Add per-run prompt, model and iteration configuration without changing existing defaults.
- [ ] Filter advertised tools and enforce the same allowlist on execution.
- [ ] Test isolation and compatibility with single-agent runs.

## Phase 2: One child at a time
- [ ] Add a coordinator and delegation tool with separate child sessions and explicit context.
- [ ] Capture structured terminal results; drain streams, enforce limits, and handle failures, timeouts and cancellation.
- [ ] Track parent-child relationships and test read-only isolation.

## Phase 3: Parent context compaction
- [ ] Keep full delegation interactions in persistent session history.
- [ ] Replace completed delegation call/result pairs with concise, clearly labelled reports in the parent model context.
- [ ] Test mixed calls, failures, ordering and provider-compatible messages.

## Phase 4: Parallel delegation
- [ ] Add spawn/wait, concurrency and aggregate budgets, cancellation and isolation tests.

## Phase 5: Evaluate and expand
- [ ] Evaluate quality, latency and cost against single-agent runs.
- [ ] Consider nested agents, sibling messaging and isolated write-capable children only when justified.
