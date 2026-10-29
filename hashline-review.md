# hashline read/edit review

Twenty-two findings from reading `plugins/hashline-read`, `plugins/hashline-edit`,
and the `plugins/hash-base` they share (`file-write` sits on the same base and
inherits most of it).

Scope note: there is no `hashline_write` tool — the write side is
`hashline_edit` plus the standalone `file_write`.

Status: **20 of 22 landed**, one open by choice (15), one partly open (13).
Every landed item has a test; `cargo clippy --workspace --all-targets` is clean
and the four hashline crates' suites are green.

## P0 — correctness / data-safety

- [x] **1. `if_rev_match` lied after an external edit.** `render` compared only
  `snap.rev`, but `observe` refreshed mtime/len and kept the rev, so a read after
  someone else rewrote the file returned `UNCHANGED`. Revisions are now derived
  from a content digest and advance on observed drift.
  Test: `if_rev_match_does_not_elide_after_an_external_write`.
- [x] **2. Rev-less `*_n` ops edited the wrong line after the tool's own edit.**
  The store tracks `read_rev` (set on read, deliberately not advanced by edits);
  a line-number batch whose read lags the current rev is refused with a re-read
  hint. Test: `line_number_ops_after_a_prior_edit_are_refused`.
- [x] **3. `rev` was validated against the pre-snapshot counter.** The check now
  runs against the in-transaction snapshot — the bytes actually being edited.
- [x] **4. `atomic_write` never fsynced.** Staged file is `sync_all`ed before the
  rename, and the parent directory is fsynced after it.
- [x] **5. A write could succeed and still be reported as failed.** Post-rename
  `stat` is best-effort: it warns, never fails the call after the bytes landed.
- [x] **6. Any edit converted CRLF to LF.** The file's dominant ending is
  detected and re-applied; a file that mixes endings is refused rather than
  silently normalized. Tests: `crlf_line_endings_round_trip`,
  `lf_line_endings_are_not_converted_to_crlf`,
  `mixed_line_endings_are_refused_rather_than_normalized`.
- [x] **7. No binary guard.** `looks_binary` (NUL in the first 8 KiB, the same
  heuristic `git` uses) lives in `hash-base` and both tools call it, so a PNG is
  refused instead of split on newlines and echoed back.
- [x] **8. No undo.** `{op: revert}` restores the pre-edit bytes, one step deep,
  in memory only. Tests: `revert_restores_the_previous_content`,
  `revert_must_stand_alone`.

## P1 — ergonomics that change behavior

- [x] **9. 4-char tags plus whitespace-insensitive hashing meant frequent
  `ambiguous`.** Hash ops now accept the row's line number alongside the tag
  (`line` / `start_line` / `end_line`), which selects among duplicate tags and
  turns a stale tag into "hash ... is not on line 12 (it is on 3, 40)" instead of
  a silent wrong-line edit. Tests: `a_line_number_disambiguates_a_duplicate_hash`,
  `a_mismatched_hash_and_line_fails_loudly`.
- [x] **10. `hashes: false` by default was an edit footgun.** Hashes are on by
  default; `hashes: false` opts out.
- [x] **11. Unknown arguments were silently ignored.** Both levels now reject
  them — `deny_unknown_fields` on the read args, and a hand-rolled key check for
  ops (serde's internally-tagged enums forbid the attribute).
  Test: `unknown_arguments_are_rejected_not_ignored`,
  `rejects_unknown_keys_at_both_levels`.
- [x] **12. Whole-line `set` was the typo source.** `{op: sub, line|hash, old,
  new, count}` changes part of a line; `count` must match exactly, so a mistaken
  `old` is refused rather than corrupting the line.
- [ ] **13. Read was O(file) regardless of `limit`.** Two of the three parts
  landed: a `max_bytes` guard refuses oversized files before hashing, and
  `FileSnapshot` now carries the line ranges so the renderer does not scan the
  file a second time. **Still open:** `snapshot` copies the whole mmap into a
  `Vec` before returning, so a windowed read still allocates the file. Fixing
  that means the renderer borrowing the mmap instead of owning `bytes`; worth
  doing when someone profiles it, not before.
- [x] **14. No `dry_run`.** `dry_run: true` validates the batch and returns the
  would-be diff. Tests: `dry_run_reports_a_diff_and_writes_nothing`,
  `dry_run_still_validates`.
- [ ] **15. No multi-file batching.** `path` is still top-level, so N files is N
  calls. **Deliberately not done:** each file needs its own rev, its own
  lockfile, and its own all-or-nothing transaction, so a multi-file envelope
  turns every failure into "which of these five rolled back?". The single-file
  contract is the thing that makes the atomicity story simple. Revisit if the
  call overhead actually shows up.

## P2 — polish

- [x] **16. `check_rev` on a never-observed path said "sent REV:0 but current is
  REV:0".** `req == 0` against an unknown path is now accepted.
- [x] **17. `grep` was thin and its numbering lied.** `ignore_case` and
  `context: K` added; the header's window reports real file line numbers rather
  than positions in the filtered list, and `matches=` counts real hits only.
  Tests: `grep_window_reports_real_line_numbers`, `grep_can_ignore_case`,
  `grep_context_includes_neighbours`.
- [x] **18. A directory was an error telling the caller to run `ls`.** It is now
  listed, with a trailing `/` marking directories.
- [x] **19. The header was prose, not fields.** `¶path#REV:rev lines=N` plus
  `matches=` / `window=a-b` / `next=K`, so a caller can parse it without
  guessing at suffixed English.
- [x] **20. Lockfile hygiene and lock granularity.** Lockfiles are hidden
  siblings of the target (`.name.hash.lock`) named from the verbatim path bytes,
  not from a whitespace-trimmed hash, and the temp-dir fallback only applies when
  the target's directory is unwritable. In-process locking is per-file, so edits
  to different files do not head-of-line block.
  Test: `per_file_locks_do_not_block_each_other` (in `hash-base`).
- [x] **21. Nits.** Line ops on an empty file say the file is empty instead of
  reporting a range against `total.max(1)`; overlap and insert-inside-range
  errors name the conflicting lines; `line_ranges` lost a dead branch;
  `hash_all_serial` sizes its vec up front.
- [x] **22. Test gaps.** Added coverage for ambiguous hashes, CRLF round-trip,
  missing trailing newline, empty-file line ops, `if_rev_match` after an external
  write, binary refusal on both tools, and stale line numbers after a prior edit.

## What was kept as-is

Validate-everything-first plus a single atomic write, bottom-up application with
same-anchor insert merging, the overlap and insert-inside-range rejects, `HEAD`
prepend, "create only for missing files", and the cursor-style error messages
that name the recovery step.
