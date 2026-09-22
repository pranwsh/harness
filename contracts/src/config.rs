use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Fully parsed application configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AppConfig {
    pub llm: LlmConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub shell: ShellConfig,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub mcp: McpConfig,
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

pub fn default_max_iterations() -> u32 {
    8
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

/// `[session]` section: conversation persistence. On by default; sessions
/// journal to one JSONL file each under the resolved session directory so
/// past conversations survive binary upgrades. The session plugin owns all
/// file I/O; this section only carries the toggle and an optional override.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionConfig {
    /// Write session journals to disk. `false` keeps everything in memory
    /// for the process lifetime (the pre-persistence behavior).
    #[serde(default = "default_session_enabled")]
    pub enabled: bool,
    /// Override for the session directory. Absent means the XDG data dir
    /// (`$XDG_DATA_HOME/harness/sessions`, else
    /// `$HOME/.local/share/harness/sessions`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            enabled: default_session_enabled(),
            dir: None,
        }
    }
}

fn default_session_enabled() -> bool {
    true
}

/// `[mcp]` section: Model Context Protocol servers (stdio transport).
/// Each `[mcp.servers.<name>]` entry spawns one persistent child process
/// speaking JSON-RPC 2.0 over stdio. Absent/empty means no MCP servers.
/// Fail-closed per server: a bad command never blocks other servers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct McpConfig {
    /// Server name -> server config (`[mcp.servers.<name>]`).
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub servers: HashMap<String, McpServerConfig>,
}

/// One MCP stdio server: spawned as `command args...` with piped stdio.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct McpServerConfig {
    /// Executable to spawn (e.g. `"npx"`).
    pub command: String,
    /// Extra argv (e.g. `["-y", "@modelcontextprotocol/server-github"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra env vars merged over the inherited environment.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    /// Per-call timeout for `tools/call`. Defaults to 30s when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// `disabled = true` skips this server without removing its config.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

impl McpServerConfig {
    /// Effective per-call timeout (default 30s, min 1ms).
    pub fn effective_timeout_ms(&self) -> u64 {
        self.timeout_ms.unwrap_or(30_000).max(1)
    }
}

#[derive(Debug, thiserror::Error)]
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

impl crate::services::ConfigApi for AppConfig {
    fn get(&self) -> AppConfig {
        self.clone()
    }
    fn set_llm_model(&self, _model: &str) -> Result<AppConfig, String> {
        Err("not file-backed".to_owned())
    }
}
