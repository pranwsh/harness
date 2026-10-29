# Read/edit tool overhaul — issue #7 (file-tool portion)

The new model-facing tools are `read` and `edit`. Legacy crates remain available
for embedders but are not loaded by the default harness. Existing unrelated
working-tree changes are not part of this branch.

Implementation checkpoints:
1. Shared text/snapshot layer: exact content versions, UTF-8/UTF-16 conventions,
   parser-backed symbols/validation, bounded output.
2. `read`: batched paths, multiple ranges, symbols, around, regex/glob search,
   outline, anchors, token/width caps.
3. `edit`: one-snapshot anchored/text operations, multi-path validated commit,
   file lifecycle, conflict recovery, fresh anchors/diffs, syntax checks and undo.
4. Default wiring, contract documentation, regression tests, verification and PR.

Safety contract: deterministic canonical lock ordering, validate all operations
before publishing, stage/fsync every replacement, rollback on observed commit
failure, and retain explicit recovery information if rollback itself fails.
Multi-file publication is not a filesystem-wide instantaneous rename and is not
crash-atomic; cooperating tools serialize with per-path locks. Uncooperative
external writers can still race. This limitation must be documented, not hidden.

Versions identify exact raw bytes (not whitespace-normalized content). Anchors
pair a 1-based line with an exact-content hash. Normalized text fallback must be
unique or explicitly selected and reported. No silent lossy encoding conversion.
Undo records are process-local, bounded, identified, and refuse to overwrite
subsequent changes. Validation uses tree-sitter, never executes repository code.
Token caps use an explicit tokenizer, not a characters/4 estimate.
