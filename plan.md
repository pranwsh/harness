# Subagent implementation plan

## Phase 1: Per-run context and policy
- [x] Add per-run prompt, model and iteration configuration without changing existing defaults.
- [x] Filter advertised tools and enforce the same allowlist on execution.
- [x] Test isolation and compatibility with single-agent runs.

## Phase 2: One child at a time
- [x] Add a delegation tool with separate child sessions and explicit context (currently coordinated in agent-loop).
- [ ] Capture structured terminal results; drain streams, enforce limits, and handle failures, timeouts and cancellation.
- [ ] Track parent-child relationships durably, including interrupted runs.

## Phase 3: Parent delegation context and verification
- [x] Keep delegation interactions in persistent session history and model context without compaction.
- [ ] Verify complete tool-call/result pairs and provider-compatible message sequences.
- [ ] Extend tests for failure reporting and cancellation propagation; parent/child isolation is covered.

## Phase 4: Parallel delegation
- [x] Add explicit parallel batch delegation and isolation tests.
- [ ] Add independently managed spawn/wait, persistent run tracking, and cancellation UI.
- [ ] Decide whether optional configurable concurrency and aggregate budgets are desirable (no hard subagent limits by default).

## Phase 5: Evaluate and expand
- [ ] Evaluate quality, latency and cost against single-agent runs.
- [ ] Consider nested agents, sibling messaging and isolated write-capable children only when justified.

## Progress notes

- Per-run options are available on `AgentLoop::run_with_options`; legacy `run` keeps its defaults. Explicit policies are inherited by children.
- `delegate_task` runs one child and `delegate_tasks` runs an explicit concurrent batch. Each child has its own session, receives supplied context, and returns a result with parent and child IDs. Nested delegation and all registered tools are available by default. All agents share the working directory; concurrent writes may conflict.
- No child-specific hard timeout, output, tool, recursion, or concurrency restrictions are applied. Existing global iteration limits and tool approval policies remain in effect. Users can configure explicit per-run options through the API.
- `run_cancellable` exposes cancellation to API callers; dropping a delegation future signals its children. The TUI does not yet offer cancellation. Aggregate budgets and independently managed background spawn/wait are not implemented.
- Parent-child links are returned in the parent tool result and thus persist on successful tool completion; a standalone durable run catalog, interrupted-run recovery and UI navigation remain unimplemented.
- No context compaction is performed. Full tool call/result pairs stay in parent history and model context.
- Existing global execution paths do not provide OS-level isolation. Provider integration, cancellation propagation and real-workload evaluation still need additional tests.

## Child shell state
- [x] Give each conversation session its own persistent `shell_exec` Bash process and initialize child shells from the parent's current directory, exported variables, and functions.
- [x] Test inheritance and independent subsequent state changes.
- [ ] Add snapshot-cancellation coverage. End-to-end parent/child delegation and concurrent siblings with the real shell plugin are covered.
- [ ] Decide how to preserve additional Bash state (unexported variables, aliases, shell options, traps, and non-UTF-8 values). Current snapshot is not a complete clone of Bash state.
- [ ] Decide whether privileged execution should inherit scoped working directory and environment. Background `shell_start` now inherits session cwd and exported environment by default; jobs remain shared and explicit env overrides inheritance.
- Files and filesystem changes remain shared; this is shell-state isolation, not a sandbox. Child shell state is released after the delegation finishes, so resuming a historical child session starts a fresh shell.

- Snapshot serialization cannot faithfully clone all Bash process state; non-UTF-8, large snapshots, cancellation during capture, and restarted shells need additional handling.

- Cancellation of a running shell command now marks its protocol stream dirty; the next command resets the process rather than consuming stale output. This loses that shell's state on cancellation.
- Workspace test run currently fails 10 TUI view layout tests; investigate baseline/configuration before merge.
