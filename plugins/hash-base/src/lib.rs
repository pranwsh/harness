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

/// Fallback hasher: 64-bit FNV-1a over trimmed line bytes.
#[derive(Debug, Default, Clone, Copy)]
pub struct Fnv1aHasher;

impl LineHasher for Fnv1aHasher {
    fn hash_normalized(&self, line: &[u8]) -> u64 {
        const OFFSET: u64 = 0xcbf29ce484222325;
        const PRIME: u64 = 0x100000001b3;
        let mut h = OFFSET;
        for &b in trim_ws(line) {
            h ^= b as u64;
            h = h.wrapping_mul(PRIME);
        }
        h
    }
    fn name(&self) -> &'static str {
        "fnv1a64"
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
    if start < data.len() || data.is_empty() && ranges.is_empty() {
        // Non-empty tail without trailing newline, or empty file -> 0 lines.
        if start < data.len() {
            ranges.push((start, data.len()));
        }
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
    mtime: SystemTime,
    len: u64,
}

/// Threshold above which line hashing fans out over `thread::scope`.
pub const PARALLEL_MIN_BYTES: usize = 256 * 1024;

/// Shared hashing service. Clone via `Arc<HashStore>`.
pub struct HashStore {
    hasher: Arc<dyn LineHasher>,
    state: Mutex<HashMap<PathBuf, RevState>>,
    edit_mu: Mutex<()>,
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
            edit_mu: Mutex::new(()),
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

    /// Serialize whole edit transactions in-process. Reads never take this.
    pub fn lock_edit(&self) -> MutexGuard<'_, ()> {
        self.edit_mu.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Current in-memory revision, if this process has read the file before.
    pub fn current_rev(&self, canonical: &Path) -> Option<u64> {
        self.lock_state().get(canonical).map(|s| s.rev)
    }

    /// Validate `req` against stored rev; emits a re-read hint on mismatch.
    pub fn check_rev(&self, canonical: &Path, req: u64, tool: &str) -> Result<u64, HashError> {
        match self.lock_state().get(canonical) {
            Some(s) if s.rev == req => Ok(req),
            Some(s) => Err(HashError::Stale {
                path: canonical.display().to_string(),
                expected: s.rev,
                got: req,
                tool: tool.to_owned(),
            }),
            None => Err(HashError::Stale {
                path: canonical.display().to_string(),
                expected: 0,
                got: req,
                tool: tool.to_owned(),
            }),
        }
    }

    /// Bump revision after a successful atomic write.
    pub fn commit_rev(&self, canonical: &Path, mtime: SystemTime, len: u64) -> u64 {
        let mut st = self.lock_state();
        let next = st.get(canonical).map(|s| s.rev + 1).unwrap_or(1);
        st.insert(
            canonical.to_path_buf(),
            RevState {
                rev: next,
                mtime,
                len,
            },
        );
        next
    }

    /// Best-effort external-modification guard for cross-process writes
    /// (in-memory revs alone cannot see other processes).
    pub fn check_external(
        &self,
        canonical: &Path,
        disk_mtime: SystemTime,
        disk_len: u64,
        tool: &str,
    ) -> Result<(), HashError> {
        let st = self.lock_state();
        if let Some(s) = st.get(canonical)
            && (s.mtime != disk_mtime || s.len != disk_len)
        {
            // Disk changed under us since our last read/commit.
            return Err(HashError::External {
                path: canonical.display().to_string(),
                tool: tool.to_owned(),
            });
        }
        Ok(())
    }

    /// Per-file cross-process exclusive lock (blocking) via a temp lockfile.
    pub fn exclusive_file_lock(&self, canonical: &Path) -> Result<File, HashError> {
        let mut h = self
            .hasher
            .hash_normalized(canonical.as_os_str().as_encoded_bytes());
        // Mix length to avoid trivial collisions across similar prefixes.
        h ^= canonical.as_os_str().len() as u64;
        let name = format!("hashline-{:016x}.lock", h);
        let lock_path = std::env::temp_dir().join(name);
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        f.lock_exclusive()?;
        Ok(f)
    }

    /// Mmap + hash a file, updating the in-memory rev table (reads never
    /// bump the counter; first read creates `REV:0`).
    ///
    /// Returns the snapshot plus the raw bytes read (owned) so callers can
    /// render `content` without re-reading. The vec is the mmap copy only
    /// when the file is non-empty; empty files return an empty vec.
    pub fn snapshot(&self, path: &Path) -> Result<(FileSnapshot, Vec<u8>), HashError> {
        let canonical = self.canonicalize(path);
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
            let rev = self.observe(&canonical, mtime, 0);
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
        let bytes = mmap[..].to_vec();
        // Shared lock releases on `file` drop here.
        let rev = self.observe(&canonical, mtime, len);
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

    /// Record an observation; create at 0 or refresh mtime/len keeping rev.
    fn observe(&self, canonical: &Path, mtime: SystemTime, len: u64) -> u64 {
        let mut st = self.lock_state();
        match st.get_mut(canonical) {
            Some(s) => {
                s.mtime = mtime;
                s.len = len;
                s.rev
            }
            None => {
                st.insert(canonical.to_path_buf(), RevState { rev: 0, mtime, len });
                0
            }
        }
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
        let mut out = Vec::new();
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
    fn hashers_are_whitespace_insensitive() {
        for h in [
            Arc::new(Xxh3Hasher) as Arc<dyn LineHasher>,
            Arc::new(Fnv1aHasher),
        ] {
            assert_eq!(h.hash_normalized(b"  x"), h.hash_normalized(b"x\t"));
            assert_ne!(h.hash_normalized(b"a b"), h.hash_normalized(b"ab"));
        }
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
