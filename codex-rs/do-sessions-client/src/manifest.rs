//! Best-effort parsing of a local `agents.yaml` into [`CreateSession`] hints.
//!
//! The current harness-api `CreateSessionRequest` does not accept a manifest
//! on the wire (the `manifest_ref` field is reserved for the future
//! `AGENT_KIND_CUSTOM` / BYO-container path). Until that lands, we let callers
//! drive `create` from an `agents.yaml` by translating the manifest into the
//! supported request fields (agent kind, repo hint, idle timeout).
//!
//! Parsing is intentionally lenient: it accepts both Plano-style manifests
//! (`spec.adapter`) and flat shapes (`agent_kind`/`repo_hint`), and ignores
//! unknown keys so future manifest fields don't break older clients.

use crate::error::DoSessionsError;
use crate::error::Result;
use crate::types::AgentKind;

/// Fields extracted from an `agents.yaml` that map onto `CreateSessionRequest`.
#[derive(Clone, Debug, Default)]
pub struct ManifestHints {
    pub agent_kind: Option<AgentKind>,
    pub repo_hint: Option<String>,
    pub idle_timeout_seconds: Option<i64>,
    /// Raw manifest, retained for the future inline/manifest_ref wire field.
    pub raw_yaml: String,
}

/// Parse an `agents.yaml` document into [`ManifestHints`].
pub fn parse_manifest(yaml: &str) -> Result<ManifestHints> {
    let value: serde_yaml::Value = serde_yaml::from_str(yaml)
        .map_err(|e| DoSessionsError::Config(format!("invalid agents.yaml: {e}")))?;

    let spec = value.get("spec");

    let agent_label = first_str(
        &value,
        spec,
        &["adapter", "agent", "agent_kind", "agentKind"],
    );
    let agent_kind = agent_label.as_deref().and_then(AgentKind::from_label);

    let repo_hint = first_str(
        &value,
        spec,
        &["repo_hint", "repoHint", "repo", "repository"],
    );

    let idle_timeout_seconds = first_i64(
        &value,
        spec,
        &["idle_timeout_seconds", "idleTimeoutSeconds", "idle_timeout"],
    );

    Ok(ManifestHints {
        agent_kind,
        repo_hint,
        idle_timeout_seconds,
        raw_yaml: yaml.to_string(),
    })
}

/// Look up the first present string key under `spec` then the document root.
fn first_str(
    root: &serde_yaml::Value,
    spec: Option<&serde_yaml::Value>,
    keys: &[&str],
) -> Option<String> {
    for key in keys {
        if let Some(v) = spec.and_then(|s| s.get(key)).and_then(as_trimmed_string) {
            return Some(v);
        }
        if let Some(v) = root.get(key).and_then(as_trimmed_string) {
            return Some(v);
        }
    }
    None
}

fn first_i64(
    root: &serde_yaml::Value,
    spec: Option<&serde_yaml::Value>,
    keys: &[&str],
) -> Option<i64> {
    for key in keys {
        if let Some(v) = spec.and_then(|s| s.get(key)).and_then(serde_yaml::Value::as_i64) {
            return Some(v);
        }
        if let Some(v) = root.get(key).and_then(serde_yaml::Value::as_i64) {
            return Some(v);
        }
    }
    None
}

fn as_trimmed_string(v: &serde_yaml::Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plano_style_manifest() {
        let yaml = r#"
apiVersion: agents.do/v1
kind: Agent
metadata:
  name: demo
spec:
  adapter: opencode
  repo: acme/payments
  idle_timeout_seconds: 1800
"#;
        let hints = parse_manifest(yaml).unwrap();
        assert_eq!(hints.agent_kind, Some(AgentKind::OpenCode));
        assert_eq!(hints.repo_hint.as_deref(), Some("acme/payments"));
        assert_eq!(hints.idle_timeout_seconds, Some(1800));
    }

    #[test]
    fn parses_flat_manifest() {
        let yaml = "agent_kind: codex\nrepo_hint: foo/bar\n";
        let hints = parse_manifest(yaml).unwrap();
        assert_eq!(hints.agent_kind, Some(AgentKind::CodexCli));
        assert_eq!(hints.repo_hint.as_deref(), Some("foo/bar"));
    }
}
