use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use harness_contracts::{KEY_CONFIG, KEY_CONFIG_SERVICE};
use harness_core::{Context, Plugin, PluginMeta};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_CONFIG_PATH: &str = "config.toml";

/// Fully parsed application configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AppConfig {
    pub llm: LlmConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub shell: ShellConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// Optional `User-Agent` sent on LLM HTTP requests. Empty/absent means
    /// no `User-Agent` header goes out at all — the right default for most
    /// providers. Note: the Zen free tier gates on an `opencode/...` value,
    /// so point `user_agent` at one (or set per-request `User-Agent` under
    /// `[llm.headers]`) when using `opencode.ai/zen`.
    #[serde(default)]
    pub user_agent: String,
    /// Extra headers merged into LLM HTTP requests by the optional
    /// model-headers plugin (`[llm.headers]` table). Empty/absent means
    /// pass-through. `authorization` and `content-type` keys are ignored
    /// there; auth stays owned by the model plugin.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    /// Model ids that must use the OpenAI Responses transport
    /// (`POST {base_url}/responses`) instead of chat completions. Extends
    /// the model plugin's built-in table (currently the `muse-spark-`
    /// family, which Zen serves on Responses only); empty/absent means
    /// "built-ins only". Lets future endpoint migrations be handled in
    /// config without a code change, as long as the model speaks a
    /// protocol the client already implements.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub responses_models: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AgentConfig {
    /// Hard cap on model iterations per turn.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            max_iterations: default_max_iterations(),
        }
    }
}

fn default_max_iterations() -> u32 {
    8
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionConfig {
    /// Storage backend; only "memory" is supported in v1.
    #[serde(default = "default_session_backend")]
    pub backend: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            backend: default_session_backend(),
        }
    }
}

fn default_session_backend() -> String {
    "memory".to_owned()
}

/// `[shell]` section: resource bounds only. No policy guardrails live here:
/// policy (allowlist/denylist/workdir/env filtering) is enforced by a future
/// guardrail plugin via the `tool.approval` waterfall, not by the shell.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ShellConfig {
    /// Sync default when the call omits `timeout_ms`.
    #[serde(default = "default_shell_timeout_ms")]
    pub default_timeout_ms: u64,
    /// Hard clamp for per-call `timeout_ms`.
    #[serde(default = "default_shell_max_timeout_ms")]
    pub max_timeout_ms: u64,
    /// Tail bytes kept per stream in formatted output.
    #[serde(default = "default_shell_max_output_bytes")]
    pub max_output_bytes: usize,
    /// Hard per-stream pipe cap while draining (bounds memory).
    #[serde(default = "default_shell_max_capture_bytes")]
    pub max_capture_bytes: usize,
    /// Max concurrent background jobs.
    #[serde(default = "default_shell_max_jobs")]
    pub max_jobs: usize,
    /// Max lifetime of one background job.
    #[serde(default = "default_shell_max_job_time_ms")]
    pub max_job_time_ms: u64,
    /// Ring-buffer cap per stream per background job.
    #[serde(default = "default_shell_max_job_output_bytes")]
    pub max_job_output_bytes: usize,
}

impl Default for ShellConfig {
    fn default() -> Self {
        ShellConfig {
            default_timeout_ms: default_shell_timeout_ms(),
            max_timeout_ms: default_shell_max_timeout_ms(),
            max_output_bytes: default_shell_max_output_bytes(),
            max_capture_bytes: default_shell_max_capture_bytes(),
            max_jobs: default_shell_max_jobs(),
            max_job_time_ms: default_shell_max_job_time_ms(),
            max_job_output_bytes: default_shell_max_job_output_bytes(),
        }
    }
}

fn default_shell_timeout_ms() -> u64 {
    30_000
}

fn default_shell_max_timeout_ms() -> u64 {
    120_000
}

fn default_shell_max_output_bytes() -> usize {
    65_536
}

fn default_shell_max_capture_bytes() -> usize {
    1_048_576
}

fn default_shell_max_jobs() -> usize {
    32
}

fn default_shell_max_job_time_ms() -> u64 {
    600_000
}

fn default_shell_max_job_output_bytes() -> usize {
    262_144
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("config file {path} is empty")]
    Empty { path: String },
    #[error("failed to write config file {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("config is not file-backed; save unsupported")]
    NoPath,
}

impl AppConfig {
    pub fn from_file(path: &str) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
        Self::from_toml(&raw, path)
    }

    pub fn from_toml(raw: &str, path: &str) -> Result<Self, ConfigError> {
        if raw.trim().is_empty() {
            return Err(ConfigError::Empty {
                path: path.to_owned(),
            });
        }
        toml::from_str(raw).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })
    }
}

/// Config lives at `./config.toml` (cwd-relative) only.
pub fn config_path() -> String {
    DEFAULT_CONFIG_PATH.to_owned()
}

/// General, domain-agnostic read/modify/persist access to `config.toml`.
///
/// The service owns the live in-memory state (`get`/`update` work on the
/// whole `AppConfig`, so future settings reuse the same shape) while file
/// persistence in v1 is surgical on the `llm.model` line only: the rest of
/// the file — comments, ordering, other keys — is preserved byte-for-byte.
/// Other fields mutated via `update` are session-scoped until persistence
/// widens.
///
/// Concurrency is last-writer-wins; callers needing atomicity should use
/// `set_llm_model`, which rolls the in-memory state back when the file
/// write fails. Writes go through a sibling temp file + rename, so a crash
/// never leaves a half-written `config.toml` (note: rename may reset file
/// mode/ownership on some platforms).
#[derive(Debug)]
pub struct ConfigService {
    path: Option<PathBuf>,
    state: RwLock<AppConfig>,
}

impl ConfigService {
    pub fn new(path: Option<PathBuf>, config: AppConfig) -> Self {
        ConfigService {
            path,
            state: RwLock::new(config),
        }
    }

    /// File backing this service, if any (`None` for embedded/test configs).
    pub fn path(&self) -> Option<PathBuf> {
        self.path.clone()
    }

    /// Current in-memory snapshot.
    pub fn get(&self) -> AppConfig {
        self.state.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Applies `f` to the in-memory state and returns the new snapshot.
    /// In-memory only; call `save` to persist (v1 persists `llm.model`).
    pub fn update(&self, f: impl FnOnce(&mut AppConfig)) -> AppConfig {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        f(&mut state);
        state.clone()
    }

    /// Persists the current state's `llm.model` to the backing file.
    /// `Err(NoPath)` when not file-backed.
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = self.path.clone().ok_or(ConfigError::NoPath)?;
        let model = self.get().llm.model;
        let display = path.display().to_string();
        let raw = std::fs::read_to_string(&path).map_err(|source| ConfigError::Io {
            path: display.clone(),
            source,
        })?;
        let next = splice_model_line(&raw, &model);
        // Sibling temp file keeps the rename atomic on the same filesystem.
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&tmp, next).map_err(|source| ConfigError::Write {
            path: display.clone(),
            source,
        })?;
        std::fs::rename(&tmp, &path).map_err(|source| ConfigError::Write {
            path: display,
            source,
        })?;
        Ok(())
    }

    /// Transactional model change: applies in-memory, attempts the file
    /// write, and rolls the in-memory state back to the previous snapshot
    /// when the write fails. Returns the new snapshot on success.
    pub fn set_llm_model(&self, model: &str) -> Result<AppConfig, ConfigError> {
        let prev = self.get();
        self.update(|cfg| cfg.llm.model = model.to_owned());
        match self.save() {
            Ok(()) => Ok(self.get()),
            Err(err) => {
                self.update(|cfg| *cfg = prev);
                Err(err)
            }
        }
    }
}

/// Rewrites only the `model = "…"` line under `[llm]`, preserving
/// indentation, trailing comments, and every other byte. Inserts the key
/// (or the whole `[llm]` section) when absent; an empty input yields a
/// minimal `[llm]` section. Always returns `Some`.
fn splice_model_line(raw: &str, model: &str) -> String {
    if raw.trim().is_empty() {
        return format!("[llm]\nmodel = \"{model}\"\n");
    }
    let escaped = model.replace('\\', "\\\\").replace('"', "\\\"");
    let mut in_llm = false;
    let mut llm_header: Option<usize> = None;
    let mut done = false;
    let mut out: Vec<String> = Vec::new();
    for line in raw.split('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_llm = is_llm_header(trimmed);
            if in_llm && llm_header.is_none() {
                // Remember the header line index for a later insertion.
                llm_header = Some(out.len());
            }
            out.push(line.to_owned());
            continue;
        }
        if in_llm && !done && is_model_key_line(line) {
            let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
            let suffix = model_line_suffix(line);
            out.push(format!("{indent}model = \"{escaped}\"{suffix}"));
            done = true;
            continue;
        }
        out.push(line.to_owned());
    }
    if done {
        return out.join("\n");
    }
    let entry = format!("model = \"{escaped}\"");
    match llm_header {
        Some(idx) => {
            out.insert(idx + 1, entry);
            out.join("\n")
        }
        None => {
            let mut text = out.join("\n");
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("\n[llm]\n{entry}\n"));
            text
        }
    }
}

/// True for an `[llm]` section header, tolerating trailing comments.
fn is_llm_header(trimmed: &str) -> bool {
    let rest = match trimmed.strip_prefix("[llm]") {
        Some(rest) => rest.trim(),
        None => return false,
    };
    rest.is_empty() || rest.starts_with('#')
}

/// True for an uncommented `model = …` assignment line.
fn is_model_key_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return false;
    }
    let Some(eq) = trimmed.find('=') else {
        return false;
    };
    trimmed[..eq].trim() == "model"
}

/// Trailing suffix (whitespace + comment) after the old quoted value, so
/// `model = "a" # keep me` stays commented. Falls back to any `#…` tail,
/// else empty.
fn model_line_suffix(line: &str) -> String {
    let Some(eq) = line.find('=') else {
        return String::new();
    };
    let right = &line[eq + 1..];
    let bytes = right.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '"' || c == '\'' {
            let quote = bytes[i];
            let mut j = i + 1;
            while j < bytes.len() {
                if bytes[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if bytes[j] == quote {
                    return right[j + 1..].to_owned();
                }
                j += 1;
            }
            return String::new();
        }
        i += 1;
    }
    match right.find('#') {
        Some(idx) => right[idx..].to_owned(),
        None => String::new(),
    }
}

pub struct ConfigPlugin {
    config: Arc<AppConfig>,
    path: Option<PathBuf>,
}

impl ConfigPlugin {
    pub fn from_file(path: &str) -> Result<Self, ConfigError> {
        Ok(ConfigPlugin {
            config: Arc::new(AppConfig::from_file(path)?),
            path: Some(PathBuf::from(path)),
        })
    }

    /// Builds the plugin from raw TOML (tests and embedded configs).
    /// Not file-backed: the service's `save` returns `NoPath`.
    pub fn from_toml(raw: &str) -> Result<Self, ConfigError> {
        Ok(ConfigPlugin {
            config: Arc::new(AppConfig::from_toml(raw, "<embedded>")?),
            path: None,
        })
    }
}

impl Plugin for ConfigPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("config")
            .provides(KEY_CONFIG)
            .provides(KEY_CONFIG_SERVICE)
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        ctx.provide_key(KEY_CONFIG, self.config.clone());
        ctx.provide_key(
            KEY_CONFIG_SERVICE,
            Arc::new(ConfigService::new(
                self.path.clone(),
                (*self.config).clone(),
            )),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = r#"
[llm]
base_url = "https://example.internal/v1"
model = "test-model"
api_key = "secret"
user_agent = "test/0.1"

[agent]
max_iterations = 3
"#;

    #[test]
    fn parses_full_config() {
        let cfg = AppConfig::from_toml(RAW, "test").unwrap();
        assert_eq!(cfg.llm.model, "test-model");
        assert_eq!(cfg.agent.max_iterations, 3);
        assert_eq!(cfg.session.backend, "memory");
    }

    #[test]
    fn defaults_kick_in_for_missing_sections() {
        let raw = r#"
[llm]
base_url = "u"
model = "m"
api_key = "k"
user_agent = "a"
"#;
        let cfg = AppConfig::from_toml(raw, "test").unwrap();
        assert_eq!(cfg.agent.max_iterations, 8);
        assert_eq!(cfg.session.backend, "memory");
    }

    #[test]
    fn rejects_empty_file() {
        assert!(matches!(
            AppConfig::from_toml("   \n", "test"),
            Err(ConfigError::Empty { .. })
        ));
    }

    #[test]
    fn rejects_missing_llm_section() {
        assert!(matches!(
            AppConfig::from_toml("[agent]\nmax_iterations = 2\n", "test"),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn shell_defaults_are_bounds_only() {
        let raw = r#"
[llm]
base_url = "u"
model = "m"
api_key = "k"
user_agent = "a"
"#;
        let cfg = AppConfig::from_toml(raw, "test").unwrap();
        assert_eq!(cfg.shell.default_timeout_ms, 30_000);
        assert_eq!(cfg.shell.max_timeout_ms, 120_000);
        assert_eq!(cfg.shell.max_jobs, 32);
        assert_eq!(cfg.shell.max_job_time_ms, 600_000);
    }

    #[test]
    fn shell_section_overrides() {
        let raw = r#"
[llm]
base_url = "u"
model = "m"
api_key = "k"
user_agent = "a"

[shell]
default_timeout_ms = 5000
max_jobs = 4
"#;
        let cfg = AppConfig::from_toml(raw, "test").unwrap();
        assert_eq!(cfg.shell.default_timeout_ms, 5000);
        assert_eq!(cfg.shell.max_jobs, 4);
    }

    #[test]
    fn shell_ignores_legacy_policy_keys() {
        // Pre-guardrail-removal configs with policy keys must still parse;
        // unknown fields are ignored, bounds apply.
        let raw = r#"
[llm]
base_url = "u"
model = "m"
api_key = "k"
user_agent = "a"

[shell]
allowlist = ["git", "ls"]
allow_shell = true
default_timeout_ms = 5000
max_jobs = 4

[shell.env]
mode = "clean"
denied_patterns = ["CUSTOM_*"]
"#;
        let cfg = AppConfig::from_toml(raw, "test").unwrap();
        assert_eq!(cfg.shell.default_timeout_ms, 5000);
        assert_eq!(cfg.shell.max_jobs, 4);
    }

    #[test]
    fn splice_preserves_comments_and_other_keys() {
        let raw = "# top comment\n[llm] # section comment\nbase_url = \"u\"\n  model = 'old'  # keep me\napi_key = \"k\"\nuser_agent = \"a\"\n\n[agent]\nmax_iterations = 3\n";
        let next = splice_model_line(raw, "new-model");
        assert!(
            next.contains("model = \"new-model\"  # keep me"),
            "{next:?}"
        );
        assert!(next.contains("# top comment"), "{next:?}");
        assert!(next.contains("# section comment"), "{next:?}");
        assert!(next.contains("base_url = \"u\""), "{next:?}");
        assert!(next.contains("max_iterations = 3"), "{next:?}");
        assert!(!next.contains("'old'"), "{next:?}");
        // Still parses, with the rest untouched.
        let cfg = AppConfig::from_toml(&next, "test").unwrap();
        assert_eq!(cfg.llm.model, "new-model");
        assert_eq!(cfg.llm.base_url, "u");
        assert_eq!(cfg.agent.max_iterations, 3);
    }

    #[test]
    fn splice_ignores_other_sections_and_comments() {
        let raw = "[agent]\n# model = \"trap\"\nmax_iterations = 3\n\n[llm]\nbase_url = \"u\"\napi_key = \"k\"\nuser_agent = \"a\"\n";
        let next = splice_model_line(raw, "m2");
        assert!(next.contains("# model = \"trap\""), "{next:?}");
        assert!(next.contains("model = \"m2\""), "{next:?}");
        let cfg = AppConfig::from_toml(&next, "test").unwrap();
        assert_eq!(cfg.llm.model, "m2");
    }

    #[test]
    fn splice_inserts_missing_key_or_section() {
        let no_key = "[llm]\nbase_url = \"u\"\napi_key = \"k\"\nuser_agent = \"a\"\n";
        let next = splice_model_line(no_key, "m2");
        let cfg = AppConfig::from_toml(&next, "test").unwrap();
        assert_eq!(cfg.llm.model, "m2");
        assert_eq!(cfg.llm.base_url, "u");

        let no_section = "[agent]\nmax_iterations = 3\n";
        let next = splice_model_line(no_section, "m2");
        assert!(next.contains("[llm]"), "{next:?}");
        assert!(next.contains("max_iterations = 3"), "{next:?}");
    }

    fn unique_tmp_dir(tag: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "harness-config-test-{}-{}-{tag}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_config(model: &str) -> AppConfig {
        AppConfig::from_toml(
            &format!(
                "# comment\n[llm]\nbase_url = \"u\"\nmodel = \"{model}\"\napi_key = \"k\"\nuser_agent = \"a\"\n"
            ),
            "test",
        )
        .unwrap()
    }

    #[test]
    fn service_round_trip_persists_model_and_keeps_comments() {
        let dir = unique_tmp_dir("roundtrip");
        let path = dir.join("config.toml");
        std::fs::write(&path, "# keep\n[llm]\nbase_url = \"u\"\nmodel = \"m1\"\napi_key = \"k\"\nuser_agent = \"a\"\n").unwrap();
        let svc = ConfigService::new(Some(path.clone()), test_config("m1"));
        let snap = svc.set_llm_model("m2").unwrap();
        assert_eq!(snap.llm.model, "m2");
        assert_eq!(svc.get().llm.model, "m2");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("model = \"m2\""), "{raw:?}");
        assert!(raw.contains("# keep"), "{raw:?}");
        assert!(
            AppConfig::from_file(path.to_str().unwrap())
                .unwrap()
                .llm
                .model
                == "m2"
        );
    }

    #[test]
    fn service_rolls_back_when_file_is_missing() {
        let dir = unique_tmp_dir("rollback");
        let svc = ConfigService::new(Some(dir.join("absent.toml")), test_config("m1"));
        let err = svc.set_llm_model("m2").unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }), "{err:?}");
        assert_eq!(svc.get().llm.model, "m1");
    }

    #[test]
    fn embedded_service_save_is_nopath_and_keeps_state() {
        let svc = ConfigService::new(None, test_config("m1"));
        assert!(matches!(svc.save(), Err(ConfigError::NoPath)));
        assert!(svc.set_llm_model("m2").is_err());
        assert_eq!(svc.get().llm.model, "m1");
    }

    #[test]
    fn plugin_provides_snapshot_and_service() {
        use harness_contracts::{KEY_CONFIG, KEY_CONFIG_SERVICE};

        let ctx = harness_core::Context::root();
        ctx.load(ConfigPlugin::from_toml(RAW).unwrap()).unwrap();
        let snap: Arc<AppConfig> = ctx.inject_key(KEY_CONFIG).unwrap();
        assert_eq!(snap.llm.model, "test-model");
        let svc: Arc<ConfigService> = ctx.inject_key(KEY_CONFIG_SERVICE).unwrap();
        assert!(svc.path().is_none());
        assert_eq!(svc.get().llm.model, "test-model");
    }

    #[test]
    fn llm_headers_default_to_empty_and_parse_when_present() {
        let cfg = AppConfig::from_toml(RAW, "test").unwrap();
        assert!(cfg.llm.headers.is_empty());

        let raw = "[llm]\nbase_url = \"u\"\nmodel = \"m\"\napi_key = \"k\"\nuser_agent = \"a\"\n[llm.headers]\nx-title = \"agent\"\n";
        let cfg = AppConfig::from_toml(raw, "test").unwrap();
        assert_eq!(
            cfg.llm.headers.get("x-title").map(String::as_str),
            Some("agent")
        );
    }

    #[test]
    fn user_agent_defaults_to_empty_when_absent() {
        let raw = "[llm]\nbase_url = \"u\"\nmodel = \"m\"\napi_key = \"k\"\n";
        let cfg = AppConfig::from_toml(raw, "test").unwrap();
        assert_eq!(cfg.llm.user_agent, "");
    }

    #[test]
    fn responses_models_default_to_empty_and_parse_when_present() {
        let cfg = AppConfig::from_toml(RAW, "test").unwrap();
        assert!(cfg.llm.responses_models.is_empty());

        let raw = "[llm]\nbase_url = \"u\"\nmodel = \"m\"\napi_key = \"k\"\nuser_agent = \"a\"\nresponses_models = [\"future-1\", \"future-2\"]\n";
        let cfg = AppConfig::from_toml(raw, "test").unwrap();
        assert_eq!(cfg.llm.responses_models, vec!["future-1", "future-2"]);
    }
}
