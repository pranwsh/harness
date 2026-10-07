//! `skills`: Agent Skills registry over `~/.config/harness/skills`.
//!
//! One skill per immediate child directory holding a `SKILL.md` (Agent
//! Skills format: frontmatter + instructions), plus optional `scripts/`,
//! `references/`, `assets/`, or any other bundled files. Startup discovery
//! is fail-isolated per skill: one bad `SKILL.md` never blocks its
//! siblings, and a missing skills directory is a valid empty catalog.
//!
//! Progressive disclosure, mirroring the spec: level-1 metadata
//! (name + description) is injected into the prompt so the model knows
//! what exists; the full `SKILL.md` body loads on demand via `skill_read`;
//! bundled files (`references/…`, `scripts/…`) resolve through the same
//! tool. Execution reuses `shell` / `hashline-read` — this plugin is
//! read-only and never executes skill code itself.

mod discover;
mod validate;

pub use discover::{resolve_dir, scan_dir, setup_dir};
pub use validate::{MAX_BODY_BYTES, check_meta, check_name, check_skill, parse_skill};

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use harness_contracts::{
    BoxFuture, CH_SKILL_REGISTERED, ConfigHandle, KEY_CONFIG, KEY_SKILL_CATALOG, KEY_TOOL_REGISTRY,
    SkillCatalogApi, SkillCatalogHandle, SkillDetail, SkillMeta, SkillRegistered, ToolError,
    ToolRegistryHandle, ToolSpec,
};
use harness_core::{Context, Result};

/// Cap for one bundled file read via `skill_read(path)`.
const MAX_READ_BYTES: usize = 64 * 1024;

fn tool_err(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError {
        tool: tool.to_owned(),
        message: message.into(),
    }
}

/// In-memory catalog of validated skills, keyed by name.
///
/// Roots are canonical skill directories captured at discovery; file reads
/// re-contain every path before touching disk.
pub struct SkillCatalog {
    ctx: Context,
    skills: Mutex<BTreeMap<String, SkillDetail>>,
}

impl SkillCatalog {
    pub fn new(ctx: Context) -> Self {
        SkillCatalog {
            ctx,
            skills: Mutex::new(BTreeMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, SkillDetail>> {
        self.skills.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Bulk-inserts discovery results. Duplicates (two dirs yielding the
    /// same valid name — e.g. case-only differences on some filesystems)
    /// keep the first and report the rest as warnings.
    pub fn insert_all(&self, details: Vec<SkillDetail>) -> Vec<String> {
        let mut warnings = Vec::new();
        for d in details {
            let name = d.meta.name.clone();
            let mut skills = self.lock();
            if skills.contains_key(&name) {
                warnings.push(format!("skills: duplicate skill `{name}`; keeping first"));
                continue;
            }
            skills.insert(name.clone(), d);
            drop(skills);
            let _ = self
                .ctx
                .emit_key(CH_SKILL_REGISTERED, SkillRegistered { name });
        }
        warnings
    }
}

impl SkillCatalogApi for SkillCatalog {
    fn list(&self) -> Vec<SkillMeta> {
        self.lock().values().map(|d| d.meta.clone()).collect()
    }

    fn get(&self, name: &str) -> Option<SkillDetail> {
        self.lock().get(name).cloned()
    }

    fn register(&self, detail: SkillDetail) -> std::result::Result<(), String> {
        validate::check_meta(&detail.meta)?;
        let mut skills = self.lock();
        if skills.contains_key(&detail.meta.name) {
            return Err(format!("duplicate skill `{}`", detail.meta.name));
        }
        let name = detail.meta.name.clone();
        skills.insert(name.clone(), detail);
        drop(skills);
        let _ = self
            .ctx
            .emit_key(CH_SKILL_REGISTERED, SkillRegistered { name });
        Ok(())
    }

    fn read_file(&self, name: &str, path: &str) -> std::result::Result<String, String> {
        let detail = self
            .lock()
            .get(name)
            .cloned()
            .ok_or_else(|| format!("unknown skill `{name}`"))?;
        let rel = path.trim();
        if rel.is_empty() {
            return Err("path must not be empty".to_owned());
        }
        let rel_path = PathBuf::from(rel);
        if rel_path.is_absolute() {
            return Err(format!("path `{path}` must be relative to the skill root"));
        }
        // Fast lexical fail: `..` can only escape (or look like it), so
        // deny up front for a deterministic error. Symlink games below
        // are still caught by the canonical containment check.
        if rel_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!("path `{path}` escapes skill `{name}`; denied"));
        }
        let joined = detail.root.join(&rel_path);
        let canonical = joined
            .canonicalize()
            .map_err(|e| format!("cannot resolve `{path}` in skill `{name}`: {e}"))?;
        if !canonical.starts_with(&detail.root) {
            return Err(format!("path `{path}` escapes skill `{name}`; denied"));
        }
        let meta = canonical
            .symlink_metadata()
            .map_err(|_| format!("`{path}` is not a file in skill `{name}`"))?;
        if !meta.is_file() {
            return Err(format!("`{path}` is not a file in skill `{name}`"));
        }
        if meta.len() > MAX_READ_BYTES as u64 {
            return Err(format!(
                "`{path}` is {}B (max {MAX_READ_BYTES}B)",
                meta.len()
            ));
        }
        std::fs::read_to_string(&canonical)
            .map_err(|e| format!("cannot read `{path}` in skill `{name}`: {e}"))
    }
}

/// Level-1 catalog as injected into the system prompt alongside the
/// model-visible `skill_list` / `skill_read` tools.
///
/// Empty when no skills are installed — the loop then behaves exactly as
/// before, so the assembler skips the block on `None` or empty.
/// Convenience over [`harness_contracts::skill_catalog_block`], which is
/// what the system-prompt plugin calls directly (contracts-only, so the
/// crate graph stays acyclic).
pub fn catalog_block(catalog: &SkillCatalogHandle) -> Option<String> {
    harness_contracts::skill_catalog_block(&catalog.list())
}

pub fn skill_list_spec() -> ToolSpec {
    ToolSpec {
        name: "skill_list".to_owned(),
        description: "Lists installed Agent Skills (name + description). Call skill_read(name) for the full instructions before using one.".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    }
}

pub fn skill_read_spec() -> ToolSpec {
    ToolSpec {
        name: "skill_read".to_owned(),
        description: "Reads an installed Agent Skill: the full SKILL.md instructions by default, or a bundled file (references/…, scripts/…) via `path`. Call this before using the skill.".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill name from skill_list" },
                "path": { "type": "string", "description": "Optional bundled file relative to the skill root (default: SKILL.md)" }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
    }
}

pub fn skill_rescan_spec() -> ToolSpec {
    ToolSpec {
        name: "skill_rescan".to_owned(),
        description: "Re-scans the skills directory for new/changed skills. Returns counts plus any warnings for invalid skills.".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    }
}

fn skill_list_handler(
    catalog: Arc<SkillCatalog>,
) -> impl Fn(String) -> BoxFuture<std::result::Result<String, ToolError>> {
    move |args: String| {
        let catalog = Arc::clone(&catalog);
        Box::pin(async move {
            if !args.trim().is_empty() {
                let v: serde_json::Value = serde_json::from_str(&args)
                    .map_err(|e| tool_err("skill_list", format!("invalid arguments: {e}")))?;
                if !v.is_object() {
                    return Err(tool_err("skill_list", "arguments must be a JSON object"));
                }
            }
            let list: Vec<_> = catalog
                .list()
                .into_iter()
                .map(|m| serde_json::json!({"name": m.name, "description": m.description}))
                .collect();
            serde_json::to_string_pretty(&list)
                .map_err(|e| tool_err("skill_list", format!("encode failed: {e}")))
        }) as BoxFuture<std::result::Result<String, ToolError>>
    }
}

fn skill_read_handler(
    catalog: Arc<SkillCatalog>,
) -> impl Fn(String) -> BoxFuture<std::result::Result<String, ToolError>> {
    move |args: String| {
        let catalog = Arc::clone(&catalog);
        Box::pin(async move {
            #[derive(serde::Deserialize)]
            struct Raw {
                name: String,
                #[serde(default)]
                path: Option<String>,
            }
            let raw: Raw = serde_json::from_str(&args)
                .map_err(|e| tool_err("skill_read", format!("invalid arguments: {e}")))?;
            if raw.name.trim().is_empty() {
                return Err(tool_err("skill_read", "name must not be empty"));
            }
            match raw.path {
                None => {
                    let detail = catalog.get(&raw.name).ok_or_else(|| {
                        tool_err("skill_read", format!("unknown skill `{}`", raw.name))
                    })?;
                    let mut out = String::from("¶skill/");
                    out.push_str(&detail.meta.name);
                    out.push_str("/SKILL.md\n");
                    out.push_str(&detail.body);
                    Ok(out)
                }
                Some(path) => {
                    let text = catalog
                        .read_file(&raw.name, &path)
                        .map_err(|e| tool_err("skill_read", e))?;
                    let mut out = String::from("¶skill/");
                    out.push_str(&raw.name);
                    out.push('/');
                    out.push_str(path.trim());
                    out.push('\n');
                    out.push_str(&text);
                    Ok(out)
                }
            }
        }) as BoxFuture<std::result::Result<String, ToolError>>
    }
}

fn skill_rescan_handler(
    catalog: Arc<SkillCatalog>,
    dir: Option<PathBuf>,
) -> impl Fn(String) -> BoxFuture<std::result::Result<String, ToolError>> {
    move |_args: String| {
        let catalog = Arc::clone(&catalog);
        let dir = dir.clone();
        Box::pin(async move {
            let Some(dir) = dir else {
                return Ok(
                    "skills discovery is not backed by a directory (in-memory catalog)".to_owned(),
                );
            };
            let (fresh, warnings) = discover::scan_dir(&dir);
            let n = fresh.len();
            let dup_warnings = catalog.insert_all(fresh);
            let mut out = format!("rescanned {} ({} new)", dir.display(), n);
            let mut all = warnings;
            all.extend(dup_warnings);
            if !all.is_empty() {
                out.push_str(":\n");
                out.push_str(&all.join("\n"));
            }
            Ok(out)
        }) as BoxFuture<std::result::Result<String, ToolError>>
    }
}

pub struct SkillsPlugin;

impl harness_core::Plugin for SkillsPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("skills")
            .provides(KEY_SKILL_CATALOG)
            .injects(KEY_TOOL_REGISTRY)
            .emits::<SkillRegistered>(CH_SKILL_REGISTERED)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        // Optional on purpose (no `injects` on `config.app`, so parking is
        // unchanged): without a config service (tests/embedded) the catalog
        // stays in-memory and never touches the filesystem. With a config
        // but no backing file, only an explicit `[skills] dir` enables disk
        // discovery — default scanning anchors to file-backed configs so
        // test/library use stays hermetic. Same idiom as session journals.
        let (config, anchored) = match ctx.try_inject_key::<ConfigHandle>(KEY_CONFIG) {
            Some(handle) => {
                let anchored = handle.config_path().is_some();
                (Some(handle.get().skills), anchored)
            }
            None => (None, false),
        };
        let dir = discover::setup_dir(config, anchored);
        let catalog = Arc::new(SkillCatalog::new(ctx.clone()));
        if let Some(ref dir) = dir {
            let (fresh, warnings) = discover::scan_dir(dir);
            for w in warnings {
                eprintln!("{w}");
            }
            for w in catalog.insert_all(fresh) {
                eprintln!("{w}");
            }
        }
        ctx.provide_key(
            KEY_SKILL_CATALOG,
            Arc::new(SkillCatalogHandle(
                catalog.clone() as Arc<dyn SkillCatalogApi>
            )),
        );
        let registry: Arc<ToolRegistryHandle> = ctx.inject_key(KEY_TOOL_REGISTRY)?;
        registry
            .register(
                skill_list_spec(),
                Box::new(skill_list_handler(Arc::clone(&catalog))),
            )
            .map_err(|e| harness_core::Error::ServiceConflict {
                key: "skill_list".into(),
                provider: e,
            })?;
        registry
            .register(
                skill_read_spec(),
                Box::new(skill_read_handler(Arc::clone(&catalog))),
            )
            .map_err(|e| harness_core::Error::ServiceConflict {
                key: "skill_read".into(),
                provider: e,
            })?;
        registry
            .register(
                skill_rescan_spec(),
                Box::new(skill_rescan_handler(catalog, dir)),
            )
            .map_err(|e| harness_core::Error::ServiceConflict {
                key: "skill_rescan".into(),
                provider: e,
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn test_ctx_with_tools() -> Context {
        let ctx = Context::root();
        ctx.load(harness_tools::ToolsPlugin).unwrap();
        ctx.load(SkillsPlugin).unwrap();
        ctx
    }

    fn write_skill(root: &std::path::Path, name: &str, front: &str, body: &str) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        let mut f = std::fs::File::create(d.join("SKILL.md")).unwrap();
        write!(f, "---\n{front}---\n{body}").unwrap();
    }

    fn unique_configured_ctx(tag: &str, extra_skills: &[(&str, &str, &str)]) -> (Context, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "harness-skills-ctx-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let skills_dir = dir.join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        for (name, front, body) in extra_skills {
            write_skill(&skills_dir, name, front, body);
        }
        let raw = format!(
            "[llm]\nbase_url = \"u\"\nmodel = \"m\"\napi_key = \"k\"\n\n[skills]\nenabled = true\ndir = \"{}\"\n",
            skills_dir.display()
        );
        let ctx = Context::root();
        // Embedded configs have no file backing, so the explicit `dir`
        // above is what enables disk discovery.
        ctx.load(harness_config::ConfigPlugin::from_toml(&raw).unwrap())
            .unwrap();
        ctx.load(harness_tools::ToolsPlugin).unwrap();
        ctx.load(SkillsPlugin).unwrap();
        (ctx, skills_dir)
    }

    #[test]
    fn empty_catalog_by_default() {
        let ctx = test_ctx_with_tools();
        let catalog: Arc<SkillCatalogHandle> = ctx.inject_key(KEY_SKILL_CATALOG).unwrap();
        assert!(catalog.list().is_empty());
        assert!(catalog_block(&catalog).is_none());
    }

    #[test]
    fn configured_dir_discovers_skills() {
        let (ctx, _dir) = unique_configured_ctx(
            "discover",
            &[(
                "pdf",
                "name: pdf\ndescription: Works with PDFs.\n",
                "Extract text with pdftotext.\n",
            )],
        );
        let catalog: Arc<SkillCatalogHandle> = ctx.inject_key(KEY_SKILL_CATALOG).unwrap();
        let list = catalog.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "pdf");
        let detail = catalog.get("pdf").unwrap();
        assert!(detail.body.contains("pdftotext"));
        let block = catalog_block(&catalog).unwrap();
        assert!(block.contains("pdf: Works with PDFs."), "{block:?}");
    }

    #[test]
    fn bad_sibling_does_not_block_good_one() {
        let (ctx, _dir) = unique_configured_ctx(
            "sibling",
            &[
                ("good", "name: good\ndescription: Good.\n", "Body.\n"),
                ("bad", "name: bad\n", ""),
            ],
        );
        let catalog: Arc<SkillCatalogHandle> = ctx.inject_key(KEY_SKILL_CATALOG).unwrap();
        assert_eq!(catalog.list().len(), 1);
        assert_eq!(catalog.list()[0].name, "good");
    }

    #[test]
    fn read_file_containment() {
        let (ctx, dir) =
            unique_configured_ctx("contain", &[("a", "name: a\ndescription: A.\n", "Body.\n")]);
        std::fs::write(dir.join("a").join("references.md"), "ref").unwrap();
        std::fs::write(dir.join("outside.txt"), "nope").unwrap();
        let catalog: Arc<SkillCatalogHandle> = ctx.inject_key(KEY_SKILL_CATALOG).unwrap();
        assert_eq!(catalog.read_file("a", "references.md").unwrap(), "ref");
        assert!(catalog.read_file("a", "../outside.txt").is_err());
        assert!(catalog.read_file("a", "/abs").is_err());
        assert!(catalog.read_file("a", "").is_err());
        assert!(catalog.read_file("nope", "SKILL.md").is_err());
    }

    #[tokio::test]
    async fn tools_execute_through_registry() {
        use harness_contracts::{KEY_TOOLS, ToolCall};
        let (ctx, dir) = unique_configured_ctx(
            "tools",
            &[(
                "pdf",
                "name: pdf\ndescription: Works with PDFs.\n",
                "Body text.\n",
            )],
        );
        std::fs::write(dir.join("pdf").join("notes.md"), "notes body").unwrap();
        let tools: Arc<harness_tools::Tools> = ctx.inject_key(KEY_TOOLS).unwrap();

        let call = |name: &str, arguments: &str| ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: arguments.into(),
        };
        let list = tools
            .execute("a", "s", 1, call("skill_list", "{}"))
            .await
            .unwrap();
        assert!(list.contains("pdf"), "{list:?}");

        let body = tools
            .execute("a", "s", 1, call("skill_read", r#"{"name":"pdf"}"#))
            .await
            .unwrap();
        assert!(body.starts_with("¶skill/pdf/SKILL.md\n"), "{body:?}");
        assert!(body.contains("Body text."));

        let bundled = tools
            .execute(
                "a",
                "s",
                1,
                call("skill_read", r#"{"name":"pdf","path":"notes.md"}"#),
            )
            .await
            .unwrap();
        assert!(bundled.contains("notes body"), "{bundled:?}");

        let err = tools
            .execute("a", "s", 1, call("skill_read", r#"{"name":"nope"}"#))
            .await
            .unwrap_err();
        assert_eq!(err.tool, "skill_read");

        let escape = tools
            .execute(
                "a",
                "s",
                1,
                call("skill_read", r#"{"name":"pdf","path":"../x"}"#),
            )
            .await
            .unwrap_err();
        assert!(escape.message.contains("escapes"), "{escape:?}");

        let rescan = tools
            .execute("a", "s", 1, call("skill_rescan", "{}"))
            .await
            .unwrap();
        assert!(rescan.contains("rescanned"), "{rescan:?}");
    }

    #[test]
    fn skill_registered_event_fires() {
        let ctx = Context::root();
        let fired = Arc::new(std::sync::atomic::AtomicU64::new(0));
        ctx.on_sync_key::<SkillRegistered, _>(CH_SKILL_REGISTERED, {
            let n = fired.clone();
            move |_| {
                n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        })
        .unwrap();
        ctx.load(harness_tools::ToolsPlugin).unwrap();
        ctx.load(SkillsPlugin).unwrap();
        let catalog: Arc<SkillCatalogHandle> = ctx.inject_key(KEY_SKILL_CATALOG).unwrap();
        catalog
            .register(SkillDetail {
                meta: SkillMeta {
                    name: "x".into(),
                    description: "Does x.".into(),
                    license: None,
                    compatibility: None,
                    metadata: Default::default(),
                    allowed_tools: None,
                },
                body: String::new(),
                root: PathBuf::from("/skills/x"),
            })
            .unwrap();
        assert_eq!(fired.load(std::sync::atomic::Ordering::Relaxed), 1);
        // Duplicate rejected.
        assert!(
            catalog
                .register(SkillDetail {
                    meta: SkillMeta {
                        name: "x".into(),
                        description: "Does x.".into(),
                        license: None,
                        compatibility: None,
                        metadata: Default::default(),
                        allowed_tools: None,
                    },
                    body: String::new(),
                    root: PathBuf::from("/skills/x"),
                })
                .is_err()
        );
    }
}
