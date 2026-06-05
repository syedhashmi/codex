use std::path::PathBuf;

use crate::error::DoSessionsError;
use crate::error::Result;

/// Environment variable holding a DO IAM token. Checked before the doctl
/// config file so CI / scripted runs can inject a token directly.
const TOKEN_ENV: &str = "DIGITALOCEAN_ACCESS_TOKEN";

/// Resolve a DO IAM bearer token.
///
/// Precedence:
/// 1. `$DIGITALOCEAN_ACCESS_TOKEN`
/// 2. The `access-token` from the active context in the `doctl` config file
///    (`<config_dir>/doctl/config.yaml`), as written by `doctl auth login`.
pub fn resolve_token() -> Result<String> {
    if let Ok(token) = std::env::var(TOKEN_ENV) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }

    let path = doctl_config_path().ok_or_else(|| {
        DoSessionsError::MissingToken(
            "could not locate the doctl config directory; set $DIGITALOCEAN_ACCESS_TOKEN or run `doctl auth login`".to_string(),
        )
    })?;

    let raw = std::fs::read_to_string(&path).map_err(|e| {
        DoSessionsError::MissingToken(format!(
            "set $DIGITALOCEAN_ACCESS_TOKEN or run `doctl auth login` (failed to read {}: {e})",
            path.display()
        ))
    })?;

    token_from_doctl_config(&raw).ok_or_else(|| {
        DoSessionsError::MissingToken(format!(
            "no access token found in {}; run `doctl auth login`",
            path.display()
        ))
    })
}

fn doctl_config_path() -> Option<PathBuf> {
    // doctl stores its config under the platform config dir:
    //   Linux:   ~/.config/doctl/config.yaml
    //   macOS:   ~/Library/Application Support/doctl/config.yaml
    dirs::config_dir().map(|dir| dir.join("doctl").join("config.yaml"))
}

/// Extract the active access token from a doctl config.yaml.
///
/// doctl supports multiple auth contexts: the top-level `access-token` is the
/// default context, and `context` names the active one. When a non-default
/// context is active its token lives under `auth-contexts.<name>`.
fn token_from_doctl_config(raw: &str) -> Option<String> {
    let value: serde_yaml::Value = serde_yaml::from_str(raw).ok()?;

    let active_context = value
        .get("context")
        .and_then(serde_yaml::Value::as_str)
        .map(str::to_string);

    if let Some(ctx) = active_context {
        if !ctx.eq_ignore_ascii_case("default") {
            if let Some(token) = value
                .get("auth-contexts")
                .and_then(|c| c.get(&ctx))
                .and_then(serde_yaml::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(token.to_string());
            }
        }
    }

    value
        .get("access-token")
        .and_then(serde_yaml::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_default_access_token() {
        let raw = "access-token: dop_v1_default\ncontext: default\n";
        assert_eq!(
            token_from_doctl_config(raw).as_deref(),
            Some("dop_v1_default")
        );
    }

    #[test]
    fn reads_named_context_token() {
        let raw = "access-token: dop_v1_default\ncontext: staging\nauth-contexts:\n  staging: dop_v1_staging\n";
        assert_eq!(
            token_from_doctl_config(raw).as_deref(),
            Some("dop_v1_staging")
        );
    }
}
