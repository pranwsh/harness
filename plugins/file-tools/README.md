# `read` and `edit`

The default harness exposes these two file tools instead of `hashline_read`,
`hashline_edit`, and `file_write`. Old crates remain buildable for embedders, but
are not registered by the default binary. Old four-character tags and numeric
REV values are **not** interchangeable with the new anchors/versions.

## Read

```json
{"path":"src/parser.py","ranges":[{"start":1,"end":12},{"start":90,"end":110}],"anchors":true}
{"paths":["src/a.rs","src/b.rs"],"outline":true,"anchors":false}
{"path":"src/parser.py","symbol":"Parser.parse_header"}
{"path":"src/parser.py","around":{"pattern":"parse_header","before":3,"after":12}}
{"path":"src/parser.py","around":{"anchor":{"line":42,"hash":"012345abcdef"},"before":3,"after":5}}
{"path":"src","search":{"regex":"TODO|FIXME","glob":"**/*.rs","context":2,"max_matches":50,"output":"matches"}}
```

Choose one selector: ranges, symbol, around, search, or outline. Ranges are
inclusive and 1-based. Multiple ranges are merged/deduplicated, retaining real
file line numbers. Path batches share the selector; use another call when files
need different selectors. A plain read returns the first 50 lines. A directory
without a search returns a sorted listing. Search supports `matches`, `files`,
and `count`; matches/count are line-based, not number of regex occurrences.
Around patterns are literal substrings and include every matching region.

Each file header includes an exact-byte content version. Anchors default on:
`42:012345abcdef|source`. Use `{ "line": 42, "hash": "012345abcdef" }` in edits.
Hashes cover the entire decoded line, excluding its line ending, including its
indentation. They are 48-bit exact-text xxh3 tags, not cryptographic signatures;
use expected_version for stronger whole-file change detection (64-bit xxh3).

Symbols and outlines use tree-sitter: Rust, Python, JavaScript/JSX,
TypeScript/TSX, and JSON grammars are bundled. JSON has no named code symbols.
Qualified names use dots (e.g. `Parser.parse_header`); ambiguous short names
return candidates, never a guessed body. Unsupported languages fail explicitly
for symbol/outline requests. There is no requirement for a running LSP.

`max_tokens` defaults to 4096, accepts 64–32768, and is a hard **cl100k_base**
tokenizer cap, including overflow notes. A provider using another tokenizer may
count differently. `max_line_width` defaults to 300 Unicode scalar values,
accepts 1–10000. Truncated rows say that their anchor covers the full line.
Token truncation drops partial rows. Overflow is explicit; request narrower
regions when necessary. Caps apply to the whole call, not each file.

Other limits: 64 input paths, 64 ranges, 32 MiB per file, 10000 search files,
128 MiB aggregate scanned bytes, 100 default search matches (up to 10000).
Search does not follow directory symlinks and reports skipped binary, oversized,
or undecodable files. Glob filtering uses paths or relative/file names. Search
and output are bounded even when the requested regex matches everything.

## Edit

```json
{"edits":[
  {"path":"src/a.rs","expected_version":"0123456789abcdef","op":"replace",
   "start":{"line":12,"hash":"012345abcdef"},"end":{"line":15,"hash":"abcdef012345"},
   "text":"fn replacement() {}"},
  {"path":"src/b.rs","op":"insert_after","anchor":{"line":3,"hash":"abcdef012345"},
   "text":"// added comment"}
],"dry_run":true}
```

Operations:

| op | Fields | Semantics |
|---|---|---|
| replace | start, end, text | Replace inclusive anchored line range. |
| delete | start, end | Delete inclusive anchored line range. |
| insert_before / insert_after | anchor, text | Insert whole-line text. |
| replace_text | old, new, occurrence?, all? | Replace substring(s); occurrence is 1-based. A unique match needs no selector. |
| create | text | Create only if absent; parent must exist. Empty files are allowed. |
| move | to | Move to an absent target, preserving bytes/mode; both parents must exist. |
| delete_file | — | Remove an existing regular file. |

Each entry also requires `path` and accepts `expected_version`. All anchors for
a file resolve against **one initial snapshot**; edits apply bottom-up. Both
ends of ranges verify line and hash. Overlaps and duplicate insertion positions
are rejected before writing. File-level operations are exclusive on their source
and destination. Symlink paths are refused for editing; read/edit the actual
file instead. New files use restrictive temporary-file permissions (0600 on
Unix); existing modes are preserved.

Exact text matching is tried first. Only if there are no exact matches is
Unicode-whitespace-normalized matching tried. It preserves the unselected bytes
and reports the fallback. Multiple matches require `occurrence` or `all:true`;
ambiguity returns candidate anchored regions, bounded to 100 candidates and the
response budget. Anchor/version failures include current versions and nearby
fresh anchors. A hash mismatch never silently relocates the edit.

Success returns `edit_id`, a compact unified diff, current versions and fresh
anchors around changes. If the response exceeds max_tokens, it is truncated
explicitly; use read to retrieve omitted regions. Dry-run returns proposed
versions/anchors, **not** live ones, and does not create undo history.

### Validation and conventions

`validate` defaults true. Bundled tree-sitter grammars check proposed content
before publication. `auto_rollback` defaults true: syntax errors prevent **all**
files from changing. `auto_rollback:false` applies and reports syntax errors;
`validate:false` explicitly skips parsing. Unsupported extensions are reported
as unvalidated, never silently called valid. No project code or linter is
executed. Existing syntax errors also prevent commit unless explicitly opted out.

UTF-8, UTF-8 BOM, UTF-16 LE/BE BOM are supported and preserved exactly outside
changed text. LF/CRLF and trailing-newline state are retained. Mixed/CR-only
line endings, invalid encodings, and NUL text are refused, not normalized
silently. Tabs/spaces indentation mismatches are warnings, not auto-formatting.

### Transactions and undo

All paths lock in canonical sorted order. Every operation, version, conflict,
parser check and staged write is validated before publication. Each replacement
is written/fsynced to a sibling temporary file, then renamed. Observed publication
failures restore already-published files; rollback failures explicitly identify
paths needing recovery. Parent directories are synced after publication.

**Filesystem limitation:** a multi-file batch is not instantaneously visible or
crash-atomic across files. Cooperating tool operations serialize on per-path
locks; external programs ignoring these locks can still race. There is no claim
that process termination/power loss during publication rolls back a batch.

`{"undo":"edit-id"}` restores a specific transaction only if every involved path
still has the bytes produced by that edit. It supports dry_run. Undoing an undo
is possible via its new edit_id. Records are process-local and expire after
32 transactions or 64 MiB of stored before/after data. Restart loses history;
unknown/expired IDs fail explicitly. Unrelated files are never touched.
Transactions accept at most 256 operations / 64 paths, 64 MiB input and output.

## Compatibility and measurements

The old plugins remain in the workspace for downstream callers, but are not
loaded alongside the new tools by default. File versions here are content hashes,
not the legacy HashStore revision counter. The new reader does not call the
legacy snapshot path or allocate per-line hash entries for unselected lines.
It still reads/decodes up to the configured file-size cap to identify exact
versions and allow parsing/search. No mmap lifetime optimization is claimed.

Run `cargo test -p harness-file-tools`; the ignored read benchmark can be run
with `cargo test -p harness-file-tools benchmark_windowed_read -- --ignored --nocapture`.
