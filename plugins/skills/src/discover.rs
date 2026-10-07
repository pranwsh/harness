//! Filesystem discovery for the skills registry (I/O, no validation rules).
//!
//! Layout: one skills directory (`[skills].dir`, else
//! `$XDG_CONFIG_HOME/harness/skills`, else `$HOME/.config/harness/skills`)
//! holding one directory per skill, each with a `SKILL.md`. Only immediate
//! children are considered (no recursion); one bad skill never blocks its
//! siblings. Every path is canonicalized and contained before reading.

use std::path::{Path, PathBuf};

use super::validate::parse_skill;
use harness_contracts::SkillDetail;

/// Cap for one raw `SKILL.md` read. The instruction body itself is capped
/// separately in [`validate`](super::validate); this only bounds the read.
pub const MAX_FILE_BYTES: usize = 256 * 1024;

/// Resolves the skills directory: explicit config override, else
/// `$XDG_CONFIG_HOME/harness/skills`, else `$HOME/.config/harness/
/// skills`. `None` when nothing usable is configured (no `HOME`), in
/// which case the caller degrades to an empty in-memory catalog.
pub fn resolve_dir(configured: Option<String>) -> Option<PathBuf> {
    if let Some(dir) = configured.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("harness/skills"));
    }
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".config/harness/skills"))
}

/// Decides the skills directory. `None` means an empty in-memory catalog:
/// discovery disabled in config, no config service at all
/// (tests/embedded contexts must never touch `$HOME`), an embedded config
/// with no explicit `dir` (embedded configs don't invent disk state), or
/// an unresolvable home / uncreatable directory (warns, never a hard
/// error). Mirrors the session plugin's persistence decision.
///
/// Pure except for the `create_dir_all`, so the decision itself is
/// unit-testable.
pub fn setup_dir(
    config: Option<harness_contracts::SkillsConfig>,
    anchored: bool,
) -> Option<PathBuf> {
    let config = config.filter(|c| c.enabled)?;
    let dir = match config.dir.filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None if anchored => resolve_dir(None)?,
        None => return None,
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "skills: cannot create {}: {e}; continuing with an empty catalog",
            dir.display()
        );
        return None;
    }
    Some(dir)
}

/// Scans `dir` for skills. Returns `(registered, warnings)`: every valid
/// immediate child directory becomes a [`SkillDetail`]; every skip carries
/// a human-readable warning (bad sibling, escape, I/O). Missing `dir`
/// yields an empty catalog without warnings; a `dir` that exists but is
/// not a directory disables discovery with one warning.
pub fn scan_dir(dir: &Path) -> (Vec<SkillDetail>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let root = match dir.canonicalize() {
        Ok(r) => r,
        Err(e) => {
            // Missing dir is a valid empty catalog (fresh installs have
            // no skills yet); anything else is reported once.
            if dir.exists() {
                warnings.push(format!("skills: cannot resolve {}: {e}", dir.display()));
            }
            return (out, warnings);
        }
    };
    if !root.is_dir() {
        warnings.push(format!(
            "skills: {} is not a directory; skills discovery disabled",
            dir.display()
        ));
        return (out, warnings);
    }
    let entries = match std::fs::read_dir(&root) {
        Ok(rd) => rd,
        Err(e) => {
            warnings.push(format!("skills: cannot list {}: {e}", root.display()));
            return (out, warnings);
        }
    };
    let mut names: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    names.sort();
    for path in names {
        if !path.is_dir() {
            continue; // stray files (README, …) are not skills.
        }
        match load_one(&root, &path) {
            Ok(detail) => out.push(detail),
            Err(w) => warnings.push(w),
        }
    }
    out.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
    (out, warnings)
}

/// Loads one candidate skill directory. `Ok` on success; `Err(warning)`
/// for every skip shape (escape, missing/non-regular `SKILL.md`,
/// oversize, invalid frontmatter).
fn load_one(root: &Path, path: &Path) -> Result<SkillDetail, String> {
    let dir_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("skills: skipping non-UTF8 entry {}", path.display()))?;
    // Containment first: a symlinked child pointing outside the skills
    // root is denied before anything is read.
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("skills: skipping `{dir_name}`: cannot resolve: {e}"))?;
    if !canonical.starts_with(root) {
        return Err(format!(
            "skills: skipping `{dir_name}`: escapes the skills directory"
        ));
    }
    let md = canonical.join("SKILL.md");
    let meta = md
        .symlink_metadata()
        .map_err(|_| format!("skills: skipping `{dir_name}`: no SKILL.md"))?;
    if !meta.is_file() {
        return Err(format!(
            "skills: skipping `{dir_name}`: SKILL.md is not a regular file"
        ));
    }
    if meta.len() > MAX_FILE_BYTES as u64 {
        return Err(format!(
            "skills: skipping `{dir_name}`: SKILL.md is {}B (max {MAX_FILE_BYTES}B)",
            meta.len()
        ));
    }
    // Re-contain the file itself (a symlinked SKILL.md pointing out is
    // denied the same way as an escaped directory).
    let canonical_md = md
        .canonicalize()
        .map_err(|e| format!("skills: skipping `{dir_name}`: cannot resolve SKILL.md: {e}"))?;
    if !canonical_md.starts_with(&canonical) {
        return Err(format!(
            "skills: skipping `{dir_name}`: SKILL.md escapes the skill directory"
        ));
    }
    let text = std::fs::read_to_string(&canonical_md)
        .map_err(|e| format!("skills: skipping `{dir_name}`: cannot read SKILL.md: {e}"))?;
    parse_skill(dir_name, &canonical, &text)
        .map_err(|e| format!("skills: skipping `{dir_name}`: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "harness-skills-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(root: &Path, name: &str, front: &str, body: &str) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        let mut f = std::fs::File::create(d.join("SKILL.md")).unwrap();
        write!(f, "---\n{front}---\n{body}").unwrap();
    }

    #[test]
    fn missing_dir_is_empty_without_warnings() {
        let (skills, warnings) = scan_dir(Path::new("/nonexistent-harness-skills-dir-xyz"));
        assert!(skills.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn file_as_dir_is_a_warning() {
        let root = tmp();
        let f = root.join("file");
        std::fs::write(&f, "x").unwrap();
        let (skills, warnings) = scan_dir(&f);
        assert!(skills.is_empty());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn good_skills_load_bad_sibling_skipped() {
        let root = tmp();
        write_skill(
            &root,
            "good",
            "name: good\ndescription: Good skill.\n",
            "Body.\n",
        );
        write_skill(&root, "bad", "name: bad\n", "");
        std::fs::write(root.join("README.md"), "stray").unwrap();
        let (skills, warnings) = scan_dir(&root);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].meta.name, "good");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("bad"), "{warnings:?}");
    }

    #[test]
    fn no_recursion_into_nested_dirs() {
        let root = tmp();
        write_skill(&root, "outer", "name: outer\ndescription: Outer.\n", "");
        write_skill(
            &root.join("outer"),
            "inner",
            "name: inner\ndescription: Inner.\n",
            "",
        );
        let (skills, _) = scan_dir(&root);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].meta.name, "outer");
    }

    #[test]
    fn dir_without_skill_md_is_skipped() {
        let root = tmp();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        let (skills, warnings) = scan_dir(&root);
        assert!(skills.is_empty());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn setup_dir_creates_explicit_dir() {
        let dir = tmp().join("sub");
        let got = setup_dir(
            Some(harness_contracts::SkillsConfig {
                enabled: true,
                dir: Some(dir.display().to_string()),
            }),
            false,
        );
        assert_eq!(got, Some(dir.clone()));
        assert!(dir.is_dir());
    }

    #[test]
    fn setup_dir_disabled_is_none() {
        let got = setup_dir(
            Some(harness_contracts::SkillsConfig {
                enabled: false,
                dir: Some("/tmp/whatever".to_owned()),
            }),
            true,
        );
        assert_eq!(got, None);
    }

    #[test]
    fn setup_dir_unanchored_without_override_is_none() {
        let got = setup_dir(Some(harness_contracts::SkillsConfig::default()), false);
        assert_eq!(got, None);
    }
}
