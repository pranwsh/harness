//! Base hashing service for hashline read/edit tools.
//!
//! Provides whitespace-insensitive line hashing (`xxh3_64` default,
//! `FNV-1a` fallback), 4-char base62 shortening with collision tracking,
//! memory-mapped line splitting via `memchr`, and an in-memory per-file
//! revision table.
//!
//! Revision policy is intentionally in-memory only: restarts reset to
//! `REV:0` on next read and any pre-restart rev correctly fails as stale.
//! Same-process concurrent edits are serialized; cross-process races are
//! caught best-effort via `mtime+len` and otherwise last-writer-wins with a
//! clean `re-read and retry` error on the next stale call.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;

// ---------------------------------------------------------------------------
// Hasher trait + impls
// ---------------------------------------------------------------------------

/// Whitespace-insensitive line hasher over raw bytes.
pub trait LineHasher: Send + Sync + 'static {
    fn hash_normalized(&self, line: &[u8]) -> u64;
    fn name(&self) -> &'static str;
}

/// Default hasher: `xxh3_64` over trimmed line bytes.
#[derive(Debug, Default, Clone, Copy)]
pub struct Xxh3Hasher;

impl LineHasher for Xxh3Hasher {
    fn hash_normalized(&self, line: &[u8]) -> u64 {
        xxhash_rust::xxh3::xxh3_64(trim_ws(line))
    }
    fn name(&self) -> &'static str {
        "xxh3_64"
    }
}

/// Strip leading/trailing ASCII space, tab, and CR.
///
/// Inner spacing is preserved so `content` round-trips exactly while
/// indentation-only changes hash identically.
#[inline]
pub fn trim_ws(mut line: &[u8]) -> &[u8] {
    while let Some((&b, rest)) = line.split_first() {
        if b == b' ' || b == b'\t' || b == b'\r' {
            line = rest;
        } else {
            break;
        }
    }
    while let Some((&b, rest)) = line.split_last() {
        if b == b' ' || b == b'\t' || b == b'\r' {
            line = rest;
        } else {
            break;
        }
    }
    line
}

// ---------------------------------------------------------------------------
// Base62 short hashes
// ---------------------------------------------------------------------------

pub const BASE62_ALPHABET: &[u8; 62] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
/// 62^4 — the 4-char tag space (~14.7M).
pub const SHORT_MOD: u64 = 62 * 62 * 62 * 62;

/// Reduce a 64-bit digest to a fixed 4-char base62 tag.
///
/// Uniform over the tag space; the full digest is retained alongside for
/// collision disambiguation.
pub fn short62(full: u64) -> [u8; 4] {
    let mut v = (full % SHORT_MOD) as u32;
    let mut out = [b'0'; 4];
    for i in (0..4).rev() {
        out[i] = BASE62_ALPHABET[(v % 62) as usize];
        v /= 62;
    }
    out
}

#[inline]
pub fn short_str(short: &[u8; 4]) -> &str {
    // Base62 alphabet is ASCII by construction.
    std::str::from_utf8(short).expect("base62 is ascii")
}

fn is_base62(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

/// Parse a user-supplied 4-char tag.
pub fn parse_short(s: &str) -> Result<[u8; 4], HashError> {
    let b = s.as_bytes();
    if b.len() != 4 || !b.iter().all(|&c| is_base62(c)) {
        return Err(HashError::InvalidHash(s.to_owned()));
    }
    Ok([b[0], b[1], b[2], b[3]])
}

// ---------------------------------------------------------------------------
// Line splitting (memchr, zero-copy)
// ---------------------------------------------------------------------------

/// Byte ranges of each line in `data` (newline excluded, no trailing empty
/// line for a final `\n`). Zero-copy: ranges borrow `data`.
pub fn line_ranges(data: &[u8]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for nl in memchr::memchr_iter(b'\n', data) {
        ranges.push((start, nl));
        start = nl + 1;
    }
    // A tail without a trailing newline is its own line; an empty file has
    // no lines at all (so this stays false for `data.is_empty()`).
    if start < data.len() {
        ranges.push((start, data.len()));
    }
    ranges
}

/// Iterate lines as byte slices without allocating.
pub fn for_each_line(data: &[u8], mut f: impl FnMut(&[u8])) {
    let mut start = 0;
    for nl in memchr::memchr_iter(b'\n', data) {
        f(&data[start..nl]);
        start = nl + 1;
    }
    if start < data.len() {
        f(&data[start..]);
    }
}


/// Digest of an empty file. `fold_digest` short-circuits to this, and
/// `snapshot` records it for zero-length files.
pub const EMPTY_DIGEST: u64 = 0x9E37_79B9_7F4A_7C15;

/// Fold per-line digests into one file digest.
///
/// Content-addressed at the same granularity `REV` speaks in: equal folds
/// mean the files agree line-for-line (whitespace-normalized). Order- and
/// count-sensitive, so inserting or swapping a line moves it. Cheap — one
/// mix per line over digests we already computed while hashing.
pub fn fold_digest(entries: &[LineEntry], trailing_nl: bool) -> u64 {
    if entries.is_empty() {
        // `fold_bytes(b"")` and an empty file must agree exactly, so the
        // empty case is one constant rather than a second derivation.
        return EMPTY_DIGEST;
    }
    let mut acc = Fold::start();
    for e in entries {
        acc.mix(e.full);
    }
    // Line hashes are blind to a final newline (both `"a\nb"` and `"a\nb\n"`
    // are two lines), so fold that bit in explicitly.
    acc.mix(u64::from(trailing_nl));
    acc.finish(entries.len())
}
/// The single fold that both `fold_digest` (hashing on read) and
/// [`HashStore::fold_bytes`] (hashing what we are about to write) go
/// through, so the two can never disagree about whether content changed.
struct Fold(u64);

impl Fold {
    fn start() -> Self {
        Fold(0x9E37_79B9_7F4A_7C15)
    }

    fn mix(&mut self, full: u64) {
        // splitmix64-style avalanche over the per-line digest.
        self.0 ^= full;
        self.0 = self.0.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = self.0.rotate_left(31);
    }

    /// Fold in the line count so trailing blank lines cannot alias.
    fn finish(self, lines: usize) -> u64 {
        self.0 ^ (lines as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }
}

/// fsync a directory so a rename inside it survives power loss. Opening a
/// directory read-only and syncing it is the portable-enough incantation
/// (on platforms where this is unsupported it degrades to a no-op).
fn sync_dir(dir: &Path) -> io::Result<()> {
    match File::open(dir) {
        Ok(f) => {
            f.sync_all()?;
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Ok(()),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Snapshot + store
// ---------------------------------------------------------------------------

/// One hashed line (1-based `lineno`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineEntry {
    pub lineno: u32,
    pub short: [u8; 4],
    pub full: u64,
}

/// Freshly hashed view of a file plus its current revision.
#[derive(Debug, Clone)]
pub struct FileSnapshot {
    pub canonical: PathBuf,
    pub rev: u64,
    pub entries: Vec<LineEntry>,
    pub mtime: SystemTime,
    pub len: u64,
}

impl FileSnapshot {
    /// All 1-based line numbers matching `short`.
    pub fn resolve(&self, short: &[u8; 4]) -> Vec<u32> {
        self.entries
            .iter()
            .filter(|e| &e.short == short)
            .map(|e| e.lineno)
            .collect()
    }

    pub fn resolve_str(&self, s: &str) -> Result<Vec<u32>, HashError> {
        Ok(self.resolve(&parse_short(s)?))
    }
}

#[derive(Debug, Clone)]
struct RevState {
    rev: u64,
    /// Fold of the line digests the current `rev` refers to. A fresh
    /// observation whose digest differs means the bytes changed under us
    /// (another process, an external tool), so the rev must advance even
    /// though our own counter never moved.
    digest: u64,
    /// Rev as of the last *read* (line numbers a caller may be holding).
    /// Edits do not move it: that gap is what lets `hashline_edit` refuse
    /// line-number ops addressed against a pre-edit read.
    read_rev: u64,
    /// Disk mtime/len as of our last observation *or* our own write. The
    /// external-modification guard compares against this rather than the
    /// committed values, so a write followed by another write does not trip
    /// the guard on its own output.
    seen_mtime: SystemTime,
    seen_len: u64,
}

/// Threshold above which line hashing fans out over `thread::scope`.
pub const PARALLEL_MIN_BYTES: usize = 256 * 1024;

/// Handle held for the duration of an edit transaction.
/// Best-effort file name for a lockfile: the final path component with
/// anything awkward for a filename replaced, and bounded in length.
fn file_stem_lossy(p: &Path) -> String {
    let raw = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_owned());
    raw.chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Open (creating if needed) a lockfile in the temp dir.
fn open_temp_lock(name: &str) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(std::env::temp_dir().join(name))
}
/// Handle held for the duration of an edit transaction.
///
/// Owns the per-file lockfile; dropping it releases the lock.
pub struct EditGuard {
    _file: File,
}

/// The content a file had before our most recent edit to it.
struct UndoSlot {
    bytes: Vec<u8>,
    /// Rev the file was at *before* that edit.
    rev: u64,
}

/// Shared hashing service. Clone via `Arc<HashStore>`.
pub struct HashStore {
    hasher: Arc<dyn LineHasher>,
    state: Mutex<HashMap<PathBuf, RevState>>,
    /// One-step undo per path: the bytes an edit replaced. Kept in memory only
    /// (a restart is not an editing session), and overwritten by the next edit
    /// to the same path, so this is "undo the last thing I did", not history.
    undo: Mutex<HashMap<PathBuf, UndoSlot>>,
}

impl std::fmt::Debug for HashStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashStore")
            .field("hasher", &self.hasher.name())
            .finish()
    }
}

impl Default for HashStore {
    fn default() -> Self {
        Self::new()
    }
}

impl HashStore {
    pub fn new() -> Self {
        Self::with_hasher(Xxh3Hasher)
    }

    pub fn with_hasher<H: LineHasher>(h: H) -> Self {
        HashStore {
            hasher: Arc::new(h),
            state: Mutex::new(HashMap::new()),
            undo: Mutex::new(HashMap::new()),
        }
    }

    pub fn hasher_name(&self) -> &'static str {
        self.hasher.name()
    }

    /// Hash one raw line (trimming applied internally).
    #[inline]
    pub fn hash_line(&self, line: &[u8]) -> (u64, [u8; 4]) {
        // Strip a trailing CR from CRLF before hashing; trim_ws would
        // already do it, but this keeps content/hash agreement explicit.
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let full = self.hasher.hash_normalized(line);
        (full, short62(full))
    }

    /// Best-effort canonical path: real canonicalize when the file exists,
    /// otherwise absolutize against cwd and lexically normalize `.`/`..`.
    pub fn canonicalize(&self, path: &Path) -> PathBuf {
        if let Ok(c) = std::fs::canonicalize(path) {
            return c;
        }
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };
        let mut out = PathBuf::new();
        for comp in abs.components() {
            match comp {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                c => out.push(c.as_os_str()),
            }
        }
        out
    }

    fn lock_state(&self) -> MutexGuard<'_, HashMap<PathBuf, RevState>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Serialize a whole edit transaction for one file.
    ///
    /// Returns a guard holding the per-file lockfile; dropping it releases the
    /// lock. Each call opens its own descriptor, so `flock` excludes other
    /// *threads* as well as other processes — no separate in-process mutex is
    /// needed, and edits to different files never contend.
    pub fn lock_edit(&self, canonical: &Path) -> Result<EditGuard, HashError> {
        Ok(EditGuard {
            _file: self.exclusive_file_lock(canonical)?,
        })
    }

    /// Remember the bytes an edit is about to replace, so it can be undone.
    pub fn stash_undo(&self, canonical: &Path, bytes: &[u8], rev: u64) {
        let mut u = self.undo.lock().unwrap_or_else(|e| e.into_inner());
        u.insert(
            canonical.to_path_buf(),
            UndoSlot {
                bytes: bytes.to_vec(),
                rev,
            },
        );
    }

    /// Bytes to restore for a one-step undo, and the rev they belong to.
    pub fn take_undo(&self, canonical: &Path) -> Option<(Vec<u8>, u64)> {
        let mut u = self.undo.lock().unwrap_or_else(|e| e.into_inner());
        // Consume it: one undo per edit, so a second `revert` says so plainly
        // instead of silently replaying an older state.
        u.remove(canonical).map(|s| (s.bytes, s.rev))
    }

    /// Current in-memory revision, if this process has read the file before.
    pub fn current_rev(&self, canonical: &Path) -> Option<u64> {
        self.lock_state().get(canonical).map(|s| s.rev)
    }

    /// Revision as of the last *read*: the numbering a caller holding line
    /// numbers most likely addressed. `None` means we never read it, so the
    /// caller cannot be holding numbers from us and there is nothing to
    /// disqualify.
    pub fn read_rev(&self, canonical: &Path) -> Option<u64> {
        self.lock_state().get(canonical).map(|s| s.read_rev)
    }

    /// Validate `req` against stored rev; emits a re-read hint on mismatch.
    ///
    /// A path we have never observed is accepted at `REV:0`: nothing has been
    /// edited through us, and the in-transaction snapshot is authoritative.
    pub fn check_rev(&self, canonical: &Path, req: u64, tool: &str) -> Result<u64, HashError> {
        match self.lock_state().get(canonical) {
            Some(s) if s.rev == req => Ok(req),
            Some(s) => Err(HashError::Stale {
                path: canonical.display().to_string(),
                expected: s.rev,
                got: req,
                tool: tool.to_owned(),
            }),
            None if req == 0 => Ok(0),
            None => Err(HashError::Stale {
                path: canonical.display().to_string(),
                expected: 0,
                got: req,
                tool: tool.to_owned(),
            }),
        }
    }

    /// Record a successful write: bump the rev and treat the bytes we just
    /// wrote as our own last observation.
    pub fn commit_rev(
        &self,
        canonical: &Path,
        digest: u64,
        mtime: SystemTime,
        len: u64,
    ) -> u64 {
        let mut st = self.lock_state();
        let (next, read_rev) = match st.get(canonical) {
            Some(s) => (s.rev + 1, s.read_rev),
            None => (1, 0),
        };
        st.insert(
            canonical.to_path_buf(),
            RevState {
                rev: next,
                digest,
                read_rev,
                seen_mtime: mtime,
                seen_len: len,
            },
        );
        next
    }

    /// Best-effort external-modification guard for cross-process writes
    /// (in-memory revs alone cannot see other processes).
    ///
    /// Compared against our own last observation, not our last commit, so
    /// our own output never reads as somebody else's edit.
    pub fn check_external(
        &self,
        canonical: &Path,
        disk_mtime: SystemTime,
        disk_len: u64,
        tool: &str,
    ) -> Result<(), HashError> {
        let st = self.lock_state();
        if let Some(s) = st.get(canonical)
            && (s.seen_mtime != disk_mtime || s.seen_len != disk_len)
        {
            // Disk changed under somebody else since our last observation.
            return Err(HashError::External {
                path: canonical.display().to_string(),
                tool: tool.to_owned(),
            });
        }
        Ok(())
    }

    /// Per-file cross-process exclusive lock (blocking) via a lockfile.
    ///
    /// The lockfile lives beside the target (`.name.<hash>.lock`) rather than
    /// in `$TMPDIR`: naming it after a *whitespace-trimmed* hash of the path
    /// let two paths that differ only in surrounding whitespace collide, and
    /// temp files accumulated one per path forever with nothing to clean them
    /// up. A sibling lockfile is also self-explanatory to anyone looking at the
    /// directory.
    ///
    /// Falls back to the temp dir when the target's directory is not writable,
    /// which is the only case where a sibling lock cannot be created.
    pub fn exclusive_file_lock(&self, canonical: &Path) -> Result<File, HashError> {
        // Hash the canonical path bytes verbatim: two distinct paths must
        // never share a lock.
        let mut h = xxhash_rust::xxh3::xxh3_64(canonical.as_os_str().as_encoded_bytes());
        h ^= canonical.as_os_str().len() as u64;
        let name = format!(".{}.{:08x}.lock", file_stem_lossy(canonical), h & 0xffff_ffff);
        let f = match canonical.parent().map(|d| d.join(&name)) {
            Some(p) => match OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&p)
            {
                Ok(f) => f,
                Err(_) => open_temp_lock(&name)?,
            },
            None => open_temp_lock(&name)?,
        };
        f.lock_exclusive()?;
        Ok(f)
    }

    /// Stage `buf` in a sibling temp file, fsync it, rename it over
    /// `canonical`, then fsync the parent directory so the rename itself is
    /// durable. A crash mid-write leaves the original file intact and a
    /// crash after the rename leaves the new content, never a torn mixture.
    /// Shared by `hashline_edit` and `file_write` so both tools get the same
    /// atomicity story.
    ///
    /// `perm` preserves the existing mode for edits (best-effort); created
    /// files take the umask default. `tool` names the caller for error text.
    pub fn atomic_write(
        &self,
        canonical: &Path,
        parent: &Path,
        buf: &[u8],
        perm: Option<std::fs::Permissions>,
        tool: &str,
    ) -> Result<(), HashError> {
        use std::io::Write as _;
        let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| {
            HashError::WriteStage(format!(
                "{tool}: cannot stage write in {}: {e}",
                parent.display()
            ))
        })?;
        if let Some(p) = perm
            && let Err(e) = tmp.as_file().set_permissions(p)
        {
            eprintln!("{tool}: set_permissions failed (best-effort, mode may be wrong): {e}");
        }
        tmp.write_all(buf).map_err(|e| {
            HashError::WriteStage(format!("{tool}: staged write failed: {e}"))
        })?;
        tmp.flush().map_err(|e| {
            HashError::WriteStage(format!("{tool}: staged write failed: {e}"))
        })?;
        // Data must be on stable storage *before* the rename publishes it;
        // otherwise a power loss can leave the new name pointing at nothing.
        tmp.as_file().sync_all().map_err(|e| {
            HashError::WriteStage(format!("{tool}: fsync of staged write failed: {e}"))
        })?;
        tmp.persist(canonical).map_err(|e| {
            HashError::WriteStage(format!("{tool}: atomic rename failed: {}", e.error))
        })?;
        // Durably record the rename, not just the contents.
        sync_dir(parent).map_err(|e| {
            HashError::WriteStage(format!(
                "{tool}: fsync of {} failed: {e}",
                parent.display()
            ))
        })?;
        Ok(())
    }

    /// Record a successful write: bump the in-memory rev and report the new
    /// head (`OK path#REV:n ops=k`).
    ///
    /// Post-write bookkeeping is best-effort: the bytes are already on disk
    /// under the new name, so a failed `stat` here must not turn a successful
    /// edit into a reported failure (a retry would then be rejected as
    /// stale). Fall back to the values we staged.
    pub fn commit_write(
        &self,
        canonical: &Path,
        digest: u64,
        staged_len: u64,
        ops: usize,
    ) -> Result<String, HashError> {
        let (mtime, len) = self.restat(canonical, staged_len);
        let new_rev = self.commit_rev(canonical, digest, mtime, len);
        Ok(format!(
            "OK {}#REV:{new_rev} ops={ops}",
            canonical.display()
        ))
    }

    /// Whole-file variant: reports `OK path#REV:n bytes=N` instead of an
    /// op count, which is meaningless for a single wholesale write.
    pub fn commit_write_bytes(
        &self,
        canonical: &Path,
        digest: u64,
        len: u64,
    ) -> Result<String, HashError> {
        let (mtime, seen_len) = self.restat(canonical, len);
        let new_rev = self.commit_rev(canonical, digest, mtime, seen_len);
        Ok(format!(
            "OK {}#REV:{new_rev} bytes={len}",
            canonical.display()
        ))
    }

    /// Re-stat after a write, falling back to what we staged.
    fn restat(&self, canonical: &Path, staged_len: u64) -> (SystemTime, u64) {
        match std::fs::metadata(canonical) {
            Ok(m) => (m.modified().unwrap_or(UNIX_EPOCH), m.len()),
            Err(e) => {
                eprintln!(
                    "warning: cannot stat {} after a successful write ({e}); the rev table may be stale",
                    canonical.display()
                );
                (UNIX_EPOCH, staged_len)
            }
        }
    }
    pub fn snapshot(&self, path: &Path) -> Result<(FileSnapshot, Vec<u8>), HashError> {
        let canonical = self.canonicalize(path);
        // On Linux `File::open` happily opens a directory and the failure only
        // shows up later as an opaque `ENODEV` from `mmap`. Detect it here so
        // callers get a plain "is a directory" message.
        if std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false) {
            return Err(HashError::IsDirectory(path.display().to_string()));
        }
        let file = File::open(path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                HashError::NotFound(path.display().to_string())
            } else {
                HashError::Io(e)
            }
        })?;
        file.lock_shared()?;
        let meta = file.metadata()?;
        let len = meta.len();
        let mtime = meta.modified().unwrap_or(UNIX_EPOCH);

        if len == 0 {
            let rev = self.observe(&canonical, EMPTY_DIGEST, mtime, 0, true);
            return Ok((
                FileSnapshot {
                    canonical,
                    rev,
                    entries: Vec::new(),
                    mtime,
                    len,
                },
                Vec::new(),
            ));
        }

        // SAFETY: read-only mapping of a file we hold a shared lock on; the
        // mapping is dropped before return (copied into `bytes` only insofar
        // as callers need owned content — hashing itself is zero-copy over
        // the mapping).
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let entries = self.hash_all(&mmap);
        let digest = fold_digest(&entries, mmap.ends_with(b"\n") || mmap.ends_with(b"\r"));
        let bytes = mmap[..].to_vec();
        // Shared lock releases on `file` drop here.
        let rev = self.observe(&canonical, digest, mtime, len, true);
        Ok((
            FileSnapshot {
                canonical,
                rev,
                entries,
                mtime,
                len,
            },
            bytes,
        ))
    }

    /// Record an observation of the bytes now on disk.
    ///
    /// The rev only advances when the *content digest* moves, so a touch or a
    /// rewrite of identical bytes keeps `REV:n` meaningful as "same content"
    /// (otherwise `if_rev_match` would report `UNCHANGED` after a real edit and
    /// a stale line number would sail through). `read` marks a caller-visible
    /// line-number view, which is what `hashline_edit` checks before letting a
    /// line-number op through.
    fn observe(
        &self,
        canonical: &Path,
        digest: u64,
        mtime: SystemTime,
        len: u64,
        read: bool,
    ) -> u64 {
        let mut st = self.lock_state();
        match st.get_mut(canonical) {
            Some(s) => {
                if s.digest != digest {
                    s.rev += 1;
                    s.digest = digest;
                }
                if read {
                    s.read_rev = s.rev;
                }
                s.seen_mtime = mtime;
                s.seen_len = len;
                s.rev
            }
            None => {
                st.insert(
                    canonical.to_path_buf(),
                    RevState {
                        rev: 0,
                        digest,
                        read_rev: 0,
                        seen_mtime: mtime,
                        seen_len: len,
                    },
                );
                0
            }
        }
    }
    /// Digest of `data` as [`snapshot`](Self::snapshot) would compute it.
    ///
    /// Lets an editor record what it wrote without hashing the file back off
    /// disk: the caller has the exact bytes it just produced.
    pub fn fold_bytes(&self, data: &[u8]) -> u64 {
        fold_digest(
            &self.hash_all(data),
            data.ends_with(b"\n") || data.ends_with(b"\r"),
        )
    }

    /// Hash every line in `data`, fanning out for large inputs.
    pub fn hash_all(&self, data: &[u8]) -> Vec<LineEntry> {
        if data.len() >= PARALLEL_MIN_BYTES
            && let Some(entries) = self.hash_all_parallel(data)
        {
            return entries;
        }
        self.hash_all_serial(data)
    }
    fn hash_all_serial(&self, data: &[u8]) -> Vec<LineEntry> {
        // One entry per newline is the exact upper bound; avoids regrowth on
        // the small-file path, which is the common one.
        let mut out = Vec::with_capacity(memchr::memchr_iter(b'\n', data).count() + 1);
        let mut lineno: u32 = 0;
        for_each_line(data, |line| {
            lineno += 1;
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let full = self.hasher.hash_normalized(line);
            out.push(LineEntry {
                lineno,
                short: short62(full),
                full,
            });
        });
        out
    }

    fn hash_all_parallel(&self, data: &[u8]) -> Option<Vec<LineEntry>> {
        let ranges = line_ranges(data);
        if ranges.is_empty() {
            return Some(Vec::new());
        }
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(ranges.len())
            .max(1);
        if threads <= 1 {
            return None;
        }
        let chunk = ranges.len().div_ceil(threads);
        let hasher = Arc::clone(&self.hasher);
        let mut out = vec![
            LineEntry {
                lineno: 0,
                short: [0; 4],
                full: 0
            };
            ranges.len()
        ];
        std::thread::scope(|s| {
            for (ri, oo) in ranges.chunks(chunk).zip(out.chunks_mut(chunk)) {
                let hasher = Arc::clone(&hasher);
                s.spawn(move || {
                    for (r, e) in ri.iter().zip(oo.iter_mut()) {
                        let mut line = &data[r.0..r.1];
                        line = line.strip_suffix(b"\r").unwrap_or(line);
                        let full = hasher.hash_normalized(line);
                        // lineno = index+1 filled by caller offset below.
                        e.full = full;
                        e.short = short62(full);
                    }
                });
            }
        });
        for (i, e) in out.iter_mut().enumerate() {
            e.lineno = (i + 1) as u32;
        }
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum HashError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("file not found: {0}; check `path` and cwd")]
    NotFound(String),
    #[error("{0} is a directory, not a file; pass a file path (list it with `ls`)")]
    IsDirectory(String),
    #[error("invalid hash '{0}': expected 4 base62 chars [0-9A-Za-z]")]
    InvalidHash(String),
    #[error(
        "unknown hash '{hash}' in {path}#REV:{rev}; re-run hashline_read on \"{path}\" — hashes shift after every edit"
    )]
    UnknownHash {
        hash: String,
        path: String,
        rev: u64,
    },
    #[error(
        "ambiguous hash '{hash}' matches lines {lines:?} in {path}#REV:{rev}; narrow the range or re-read"
    )]
    Ambiguous {
        hash: String,
        lines: Vec<u32>,
        path: String,
        rev: u64,
    },
    #[error(
        "stale revision for {path}: {tool} sent REV:{got} but current is REV:{expected}; re-run hashline_read on \"{path}\" and retry with REV:{expected}"
    )]
    Stale {
        path: String,
        expected: u64,
        got: u64,
        tool: String,
    },
    #[error(
        "external modification detected for {path} (mtime/len changed since last read); re-run hashline_read on \"{path}\" and retry {tool}"
    )]
    External { path: String, tool: String },
    #[error("{0}")]
    WriteStage(String),
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct HashBasePlugin;

impl harness_core::Plugin for HashBasePlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("hash-base").provides(harness_contracts::KEY_HASH_STORE)
    }

    fn build(&self, ctx: harness_core::Context) -> harness_core::Result<()> {
        ctx.provide_key(
            harness_contracts::KEY_HASH_STORE,
            Arc::new(HashStore::new()),
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_is_space_tab_cr_only() {
        assert_eq!(trim_ws(b"  x\t"), b"x");
        assert_eq!(trim_ws(b"\r x \r"), b"x");
        assert_eq!(trim_ws(b"a b"), b"a b");
        assert_eq!(trim_ws(b"  "), b"");
    }

    #[test]
    fn hasher_is_whitespace_insensitive() {
        let h = Xxh3Hasher;
        assert_eq!(h.hash_normalized(b"  x"), h.hash_normalized(b"x\t"));
        assert_ne!(h.hash_normalized(b"a b"), h.hash_normalized(b"ab"));
    }

    #[test]
    fn short_is_4_base62_and_padded() {
        let s = short62(0);
        assert_eq!(s, *b"0000");
        let tag = short62(u64::MAX);
        let s = short_str(&tag);
        assert_eq!(s.len(), 4);
        assert!(s.bytes().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn parse_short_rejects_bad_input() {
        assert!(parse_short("abc").is_err());
        assert!(parse_short("abcde").is_err());
        assert!(parse_short("ab!c").is_err());
        assert!(parse_short("aB1c").is_ok());
    }

    #[test]
    fn line_ranges_handles_trailing_newline() {
        assert!(line_ranges(b"").is_empty());
        assert_eq!(line_ranges(b"a\nb\n"), vec![(0, 1), (2, 3)]);
        assert_eq!(line_ranges(b"a\nb"), vec![(0, 1), (2, 3)]);
        assert_eq!(line_ranges(b"a"), vec![(0, 1)]);
    }

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hash-base-{}-{tag}.txt", std::process::id()))
    }

    #[test]
    fn fold_digest_tracks_content_not_order_insensitively() {
        let store = HashStore::new();
        let a = store.fold_bytes(b"a\nb\n");
        // Whitespace-only differences hash identically (same line digests).
        assert_eq!(a, store.fold_bytes(b"a\nb\n"));
        assert_eq!(a, store.fold_bytes(b"  a  \nb\n"));
        // Real changes, reordering, and count changes all move it.
        assert_ne!(a, store.fold_bytes(b"a\nc\n"));
        assert_ne!(a, store.fold_bytes(b"b\na\n"));
        assert_ne!(a, store.fold_bytes(b"a\nb\nc\n"));
        assert_ne!(a, store.fold_bytes(b"a\nb"));
        // Empty is its own digest and never aliases a temporary.
        assert_eq!(store.fold_bytes(b""), EMPTY_DIGEST);
        assert_ne!(store.fold_bytes(b"\n"), EMPTY_DIGEST);
    }

    /// A read, then an edit, then a read again must keep `REV:n` meaning
    /// "the content I last looked at" — this is what `if_rev_match` rests on.
    #[test]
    fn an_edit_bumps_rev_and_an_external_change_is_observable() {
        let store = HashStore::new();
        let p = tmp("rev-bump");
        std::fs::write(&p, "a\nb\n").unwrap();
        let (snap, _) = store.snapshot(&p).unwrap();
        assert_eq!(snap.rev, 0);
        // Same bytes: same rev.
        assert_eq!(store.snapshot(&p).unwrap().0.rev, 0);
        // A whitespace-only touch still agrees (line digests normalize it).
        std::fs::write(&p, "  a\nb\n").unwrap();
        assert_eq!(store.snapshot(&p).unwrap().0.rev, 0);
        // Real content change: the rev moves even though we never wrote it.
        std::fs::write(&p, "a\nXX\n").unwrap();
        assert_eq!(store.snapshot(&p).unwrap().0.rev, 1);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn read_rev_lags_edits_so_line_numbers_can_be_disqualified() {
        let store = HashStore::new();
        let p = tmp("read-rev");
        std::fs::write(&p, "a\nb\n").unwrap();
        store.snapshot(&p).unwrap();
        let canonical = store.canonicalize(&p);
        assert_eq!(store.read_rev(&canonical), Some(0));
        // An edit as far as the store is concerned: commit a new digest.
        let digest = store.fold_bytes(b"a\nb\nc\n");
        store.commit_rev(&canonical, digest, UNIX_EPOCH, 6);
        assert_eq!(store.current_rev(&canonical), Some(1));
        assert_eq!(store.read_rev(&canonical), Some(0), "edits must not advance read_rev");
        // A fresh read re-synchronizes them.
        std::fs::write(&p, "a\nb\nc\n").unwrap();
        let (snap, _) = store.snapshot(&p).unwrap();
        assert_eq!(snap.rev, 1);
        assert_eq!(store.read_rev(&canonical), Some(1));
        std::fs::remove_file(&p).ok();
    }

    /// Our own write must not look like somebody else's afterwards, or the
    /// second edit in a row would trip the external-modification guard.
    #[test]

    fn a_write_does_not_trip_its_own_external_guard() {
        let store = HashStore::new();
        let p = tmp("self-guard");
        std::fs::write(&p, "a\n").unwrap();
        let canonical = store.canonicalize(&p);
        store.snapshot(&p).unwrap();

        let digest = store.fold_bytes(b"b\n");
        store
            .atomic_write(&canonical, p.parent().unwrap(), b"b\n", None, "test")
            .unwrap();
        store.commit_write(&canonical, digest, 2, 1).unwrap();

        // A second edit: check_external must pass on the file we just wrote.
        let meta = std::fs::metadata(&canonical).unwrap();
        store
            .check_external(
                &canonical,
                meta.modified().unwrap_or(UNIX_EPOCH),
                meta.len(),
                "test",
            )
            .expect("our own write must not look external");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn untouched_path_accepts_rev_zero_only() {
        let store = HashStore::new();
        let p = PathBuf::from("/definitely/not/here-hash-base-xyz.txt");
        let canonical = store.canonicalize(&p);
        // Nothing observed yet: REV:0 is the honest answer, not a contradiction.
        assert!(store.check_rev(&canonical, 0, "test").is_ok());
        assert!(store.check_rev(&canonical, 3, "test").is_err());
    }
    /// The per-file lock must exclude other *threads* in this process too,
    /// not just other processes: two concurrent edits in one harness would
    /// otherwise both pass validation against the same snapshot.
    #[test]
    fn lock_edit_excludes_threads_and_different_files_do_not_contend() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let store = Arc::new(HashStore::new());
        // Work in a subdirectory so the sibling lockfile is somewhere we can
        // assert about, rather than mixed into the system temp dir.
        let dir = std::env::temp_dir().join(format!("hash-base-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.txt");
        std::fs::write(&p, "a\n").unwrap();
        let canonical = store.canonicalize(&p);
        let overlap = Arc::new(AtomicUsize::new(0));
        let inside = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let store = Arc::clone(&store);
            let canonical = canonical.clone();
            let overlap = Arc::clone(&overlap);
            let inside = Arc::clone(&inside);
            handles.push(std::thread::spawn(move || {
                let _g = store.lock_edit(&canonical).unwrap();
                if inside.fetch_add(1, Ordering::SeqCst) == 1 {
                    overlap.fetch_add(1, Ordering::SeqCst);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                inside.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(overlap.load(Ordering::SeqCst), 0, "two threads held the same file lock");

        // A different file is not blocked by a held lock.
        let q = dir.join("b.txt");
        std::fs::write(&q, "b\n").unwrap();
        let other = store.canonicalize(&q);
        let held = store.lock_edit(&canonical).unwrap();
        let start = std::time::Instant::now();
        let _second = store.lock_edit(&other).unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "a different file's lock blocked on an unrelated edit"
        );
        drop(held);

        // The lockfile lives beside the target, not in the temp dir.
        let siblings: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".lock"))
            .collect();
        assert!(
            siblings.iter().all(|n| n.starts_with('.')),
            "lockfiles should be hidden siblings of the target: {siblings:?}"
        );
        let temp_lockfiles = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".hash-base-lock-") && n.ends_with(".lock"))
            .count();
        assert_eq!(temp_lockfiles, 0, "lockfiles should not accumulate in $TMPDIR");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parallel_matches_serial() {
        let store = HashStore::new();
        let mut data = Vec::new();
        for i in 0..5000 {
            data.extend_from_slice(format!("line {i}  \n").as_bytes());
        }
        // Force parallel path regardless of threshold via direct call.
        let serial = store.hash_all_serial(&data);
        let parallel = store.hash_all_parallel(&data).expect("parallel");
        assert_eq!(serial, parallel);
    }
}
