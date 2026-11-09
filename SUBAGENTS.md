# Subagents: minimal background execution

## Tools

- `delegate_task({task, context?})`: run one child and wait for the result.
- `delegate_tasks({tasks: [{task, context?}, ...]})`: run children concurrently and wait for the batch.
- `spawn_agent({task, context?})`: fork the shell, start a child, and return `{run_id, session_id, status: "running"}` without waiting for its answer.
- `agent_wait({run_id})`: wait for a background child belonging to the calling session. Returns its structured terminal result and suppresses a subsequent automatic copy.
- `agent_cancel({run_id})`: request cancellation of that session's child and its background descendants. Acknowledgement is not proof of completion; wait for the terminal result when needed.

No UI notifications are generated. Immediately before a parent model request, completed undelivered reports are persisted as labelled conversation data and marked delivered. Delivery never interrupts streaming or splits a tool-call/result group. An idle parent is not woken; reports arrive on its next turn. If a report was already delivered, an explicit wait may return it again by request.

Background runs survive normal completion of their parent's turn. Explicit parent cancellation cancels descendants. The application calls `shutdown_background` on normal exit; embedders must do likewise. Results and ownership are managed in memory for this minimal iteration: active runs and undelivered results are not recovered after process restart. Reports already delivered remain in session history. No new UI controls are added by this iteration.

## Shell semantics

Child creation forks a Bash subshell from the parent's current session before returning a spawn handle. It does not serialize variables or exec a fresh interpreter. Unexported and exported variables, arrays, readonly attributes, functions, aliases, options, umask and directory are inherited under Bash's native subshell rules. Private 0700 directories and 0600 channels carry the child commands, and the registry routes calls by session identity.

Native differences remain: PID-related values change; Bash resets caught traps and job tables. Existing open file descriptions can remain shared at the OS level. Shell options such as `errexit` and readonly assignment errors can terminate a noninteractive shell. A terminated inherited shell fails instead of silently becoming a fresh shell. No arbitrary-Bash-state or crash-recovery guarantee is made.

Children mutate their own Bash state. Files are shared, not copied. Background one-shot shell jobs inherit cwd and exported environment by default, not unexported shell state. Privileged commands retain their separate approval and execution behavior. Explicit `env` requests use a clean one-shot environment.

This is routing and state separation, **not a security sandbox**: agents run as the same OS user with unrestricted tools by default and can deliberately inspect or signal other processes. Do not use it for mutually untrusted agents. Use containers or separate users for that requirement. Parallel file edits can conflict.

## Verification

Run:

```
cargo test -p harness-agent-loop -p harness-shell -p harness-tools
cargo check -p harness
```

Coverage includes blocking delegation, parallel session isolation, background handles/wait, delayed context delivery exactly once, explicit policy inheritance, nested cancellation signalling, tool-result pairing on cancellation, real-shell unexported-variable inheritance and independent mutations, native readonly behavior, non-UTF-8 variable bytes, nested forks, EOF after child death, and background-job ownership.

The broader branch remains a draft: full-workspace TUI layout failures, exhaustive fork-race testing, durable run recovery and live-provider evaluation are separate outstanding work tracked in plan.md. They are not prerequisites claimed complete by this minimal background iteration.
