//! Optional LLM HTTP header checks: inject + validate (pre) + observe (post).
//!
//! Fully decoupled: the only contact with the model plugin is the
//! `llm.request_headers` waterfall (pre-send hook, may merge headers or veto
//! the send) and the `llm.response_headers` emit (post hook, observation
//! only). Neither crate imports the other; both depend only on
//! `harness-contracts` payloads and the core bus.
//!
//! Presence-based opt-out: an empty `[llm.headers]` table is a pass-through
//! (auth validation still applies), and omitting this plugin from the
//! harness changes nothing — the waterfall is a no-op without handlers.
//!
//! The post-hook log line is opt-in via `HARNESS_LOG_LLM_HEADERS`: anything
//! written to stderr while the TUI owns the screen corrupts the display
//! (it shows up as ghost text in the input box), so observation stays silent
//! by default.

use std::sync::Arc;

use harness_config::AppConfig;
use harness_contracts::{
    CH_LLM_REQUEST_HEADERS, CH_LLM_RESPONSE_HEADERS, KEY_CONFIG, LlmRequestHeaders,
    LlmResponseHeaders,
};
use harness_core::{Context, Result};
/// Response headers worth one log line (allowlist; never dumps everything).
const OBSERVED_HEADERS: &[&str] = &[
    "x-request-id",
    "x-ratelimit-limit-requests",
    "x-ratelimit-remaining-requests",
    "x-ratelimit-reset-requests",
    "x-ratelimit-limit-tokens",
    "x-ratelimit-remaining-tokens",
    "retry-after",
];

/// Opt-in stderr logging for the post hook. Off by default: writing to
/// stderr while the TUI owns the terminal sprays ghost text into the input
/// box, so observation only prints when explicitly enabled.
pub fn observe_enabled() -> bool {
    std::env::var_os("HARNESS_LOG_LLM_HEADERS").is_some()
}

/// Generates one session id (UUID4) per plugin load. The id is reused for
/// every request of the process lifetime, matching the official client's
/// `X-Session-ID` attribution header.
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Merges configured headers, stamps the session id, and validates auth.
/// Pure: unit-tested without DI or HTTP.
///
/// Configured names merge case-insensitively, except `authorization` and
/// `content-type`, which stay owned by the model plugin and are never
/// overridden from config. An empty bearer vetoes the send.
pub fn check_request(
    config: &AppConfig,
    mut req: LlmRequestHeaders,
    session_id: &str,
) -> LlmRequestHeaders {
    for (name, value) in &config.llm.headers {
        if name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("content-type") {
            continue;
        }
        req.set(name.clone(), value.clone());
    }
    req.set("X-Session-ID", session_id);
    match req.get("authorization").map(str::trim) {
        Some(value) if !value.is_empty() && value != "Bearer" => req,
        _ => LlmRequestHeaders::deny(
            req.model.clone(),
            req.headers,
            "missing Authorization header",
        ),
    }
}

/// Formats observed response headers as one log line (status, model, and
/// the allowlisted headers actually present). Pure.
pub fn format_observed(resp: &LlmResponseHeaders) -> String {
    let mut parts = vec![format!("llm {} {}", resp.status, resp.model)];
    for name in OBSERVED_HEADERS {
        if let Some((_, value)) = resp
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
        {
            parts.push(format!("{name}={value}"));
        }
    }
    parts.join(" ")
}

pub struct ModelHeadersPlugin;

impl harness_core::Plugin for ModelHeadersPlugin {
    fn meta(&self) -> harness_core::PluginMeta {
        harness_core::PluginMeta::new("model-headers")
            .injects(KEY_CONFIG)
            .waterfalls::<LlmRequestHeaders>(CH_LLM_REQUEST_HEADERS)
            .listens::<LlmResponseHeaders>(CH_LLM_RESPONSE_HEADERS)
    }

    fn build(&self, ctx: Context) -> Result<()> {
        let config: Arc<AppConfig> = ctx.inject_key(KEY_CONFIG)?;
        // One session id per plugin load, reused for every request.
        let session_id = new_session_id();
        ctx.on_waterfall_key::<LlmRequestHeaders, _, _>(CH_LLM_REQUEST_HEADERS, move |req| {
            let config = config.clone();
            let session_id = session_id.clone();
            async move { check_request(&config, (*req).clone(), &session_id) }
        })?;
        ctx.on_key::<LlmResponseHeaders, _, _>(CH_LLM_RESPONSE_HEADERS, |resp| async move {
            if observe_enabled() {
                eprintln!("{}", format_observed(&resp));
            }
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(headers: &[(&str, &str)]) -> AppConfig {
        AppConfig::from_toml(
            &format!(
                "[llm]\nbase_url = \"u\"\nmodel = \"m\"\napi_key = \"k\"\nuser_agent = \"a\"\n[llm.headers]\n{}",
                headers
                    .iter()
                    .map(|(k, v)| format!("{k} = \"{v}\"\n"))
                    .collect::<String>(),
            ),
            "test",
        )
        .unwrap()
    }

    fn plain_config() -> AppConfig {
        AppConfig::from_toml(
            "[llm]\nbase_url = \"u\"\nmodel = \"m\"\napi_key = \"k\"\nuser_agent = \"a\"\n",
            "test",
        )
        .unwrap()
    }

    fn seeded(model: &str) -> LlmRequestHeaders {
        LlmRequestHeaders::allow(
            model,
            vec![("authorization".to_owned(), "Bearer k".to_owned())],
        )
    }

    #[test]
    fn merges_configured_headers_case_insensitively() {
        let config = config_with(&[("X-Title", "agent")]);
        let mut seed = seeded("m");
        // Differently-cased seed entry is replaced, not duplicated.
        seed.set("x-TITLE", "seed");
        let out = check_request(&config, seed, "s1");
        assert!(!out.is_denied());
        let titles: Vec<_> = out
            .headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("x-title"))
            .collect();
        assert_eq!(titles.len(), 1);
        assert_eq!(titles[0].1, "agent");
    }

    #[test]
    fn auth_and_content_type_are_immune() {
        let config = config_with(&[
            ("authorization", "Bearer evil"),
            ("Content-Type", "text/plain"),
        ]);
        let out = check_request(&config, seeded("m"), "s1");
        assert!(!out.is_denied());
        assert_eq!(out.get("authorization"), Some("Bearer k"));
        assert!(out.get("content-type").is_none());
    }

    #[test]
    fn empty_table_is_a_pass_through() {
        let out = check_request(&plain_config(), seeded("m"), "s1");
        assert!(!out.is_denied());
        // Seeded auth plus the stamped session id.
        assert_eq!(out.headers.len(), 2);
    }

    #[test]
    fn missing_or_bare_bearer_is_denied() {
        let config = plain_config();
        let no_auth = LlmRequestHeaders::allow("m", vec![]);
        let denied = check_request(&config, no_auth, "s1");
        assert!(denied.is_denied());

        let bare = LlmRequestHeaders::allow("m", vec![("authorization".into(), "Bearer".into())]);
        assert!(check_request(&config, bare, "s1").is_denied());
    }

    #[test]
    fn session_id_is_stamped() {
        let out = check_request(&plain_config(), seeded("m"), "session-123");
        assert_eq!(out.get("X-Session-ID"), Some("session-123"));
        // Denied responses carry the session id too.
        let no_auth = LlmRequestHeaders::allow("m", vec![]);
        let denied = check_request(&plain_config(), no_auth, "session-123");
        assert!(denied.is_denied());
        assert_eq!(denied.get("x-session-id"), Some("session-123"));
    }

    #[test]
    fn new_session_id_is_uuid_v4() {
        let id = new_session_id();
        let parsed = uuid::Uuid::parse_str(&id).expect("hyphenated uuid");
        assert_eq!(parsed.get_version(), Some(uuid::Version::Random));
        assert_ne!(new_session_id(), id);
    }

    #[test]
    fn format_observed_keeps_allowlist_only() {
        let resp = LlmResponseHeaders {
            model: "m".into(),
            status: 200,
            headers: vec![
                ("x-request-id".into(), "abc".into()),
                ("x-ratelimit-remaining-requests".into(), "42".into()),
                ("set-cookie".into(), "secret".into()),
            ],
        };
        let line = format_observed(&resp);
        assert!(line.contains("200"), "{line:?}");
        assert!(line.contains("x-request-id=abc"), "{line:?}");
        assert!(
            line.contains("x-ratelimit-remaining-requests=42"),
            "{line:?}"
        );
        assert!(!line.contains("secret"), "{line:?}");
    }

    #[tokio::test]
    async fn waterfall_end_to_end_through_di() {
        let ctx = Context::root();
        ctx.load(
            harness_config::ConfigPlugin::from_toml(
                "[llm]\nbase_url = \"u\"\nmodel = \"m\"\napi_key = \"k\"\nuser_agent = \"a\"\n[llm.headers]\nx-title = \"agent\"\n",
            )
            .unwrap(),
        )
        .unwrap();
        ctx.load(ModelHeadersPlugin).unwrap();

        let out: LlmRequestHeaders = ctx
            .waterfall_key(CH_LLM_REQUEST_HEADERS, seeded("m"))
            .await
            .unwrap();
        assert!(!out.is_denied());
        assert_eq!(out.get("x-title"), Some("agent"));
        // Session id is stamped per plugin load and parses as UUID v4.
        let session = out.get("x-session-id").expect("session id stamped");
        let parsed = uuid::Uuid::parse_str(session).expect("hyphenated uuid");
        assert_eq!(parsed.get_version(), Some(uuid::Version::Random));
    }

    #[test]
    fn parks_until_config_is_loaded() {
        let ctx = Context::root();
        let outcome = ctx.load(ModelHeadersPlugin).unwrap();
        assert!(matches!(outcome, harness_core::LoadOutcome::Pending { .. }));
    }
}
