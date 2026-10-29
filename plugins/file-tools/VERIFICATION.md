# Verification — read/edit overhaul

Commands run on the implementation branch:

- `cargo test -p harness-file-tools`: 16 passed, 1 ignored manual benchmark.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo test --workspace`: fails in 10 `harness-tui::view` layout tests.
- `cargo test --workspace --exclude harness-tui`: also exposes 3 failures in
  `harness-tui-commands` branch-command expectations.
- `cargo test --workspace --exclude harness-tui --exclude harness-tui-commands`:
  passed.
- Both failing suites were independently run in a detached worktree at base
  commit `b797303`; they fail there as well. No TUI implementation is changed.
- `git diff --check`: passed.

Regression coverage includes registered tool execution, generated schemas,
multiple read ranges, symbols, outlines, regex/glob search, search modes, path
batches, caps, conventions/encoding preservation, stale versions/anchors,
ambiguous text recovery, whitespace fallback, initial-snapshot line addressing,
dry-run, insertion boundaries, overlap rejection, file lifecycle, identified
undo, undo conflict refusal, validation rejection/report-only mode, sorted-lock
concurrency, and injected second-publication failure with full rollback.

## Performance

Manual debug-profile benchmark (`benchmark_windowed_read`) uses 4,900,000 bytes,
100,000 lines, 5 iterations, tokenizer warmed before measurement. Last local run:

| Operation | Time for 5 iterations |
|---|---:|
| Legacy `HashStore::snapshot` (hashes all lines, no rendering) | 239.9 ms |
| New `read` (51 selected lines, exact version, decoding, rendering, tokenizer cap) | 384.2 ms |

This is not an apples-to-apples throughput benchmark: the new path includes
rendering/tokenization and the baseline does not. It does **not** establish a
latency improvement, and this PR makes no such claim. The new read avoids the
legacy mmap-to-Vec snapshot and per-line hash-entry allocations; it still reads
and decodes the bounded file for a version/symbol/search-capable snapshot.
Peak allocations were not instrumented, so no measured allocation improvement
is claimed. Read rows stream through the decoded text rather than allocating a
pointer table for all lines. Parser, diff and tokenizer costs remain part of the
new feature contract.

## Explicit limitations

- Multi-file publication provides validation-before-write and rollback on
  observed errors, not instantaneous visibility or crash atomicity. See README.
- Parser support is explicitly language-limited; no repository linter executes.
- Undo is bounded and process-local, not a durable journal.
- Output budgets can omit anchors/diff regions; truncation is explicit.
- UTF-8 and BOM UTF-16 are preserved; other encodings/mixed line endings fail.
- Hashes are noncryptographic; external programs that ignore locks can race.
