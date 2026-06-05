use crate::error::DoSessionsError;
use crate::error::Result;

/// Environment variable that overrides the harness-api base URL. Lets the
/// same binary target the public endpoint or staging without a rebuild.
pub const BASE_URL_ENV: &str = "DO_HARNESS_BASE_URL";

/// Default public DigitalOcean API endpoint.
pub const DEFAULT_BASE_URL: &str = "https://api.digitalocean.com";

/// Resolve the harness-api base URL.
///
/// Precedence: explicit `flag` (`--base-url`) > `$DO_HARNESS_BASE_URL` >
/// [`DEFAULT_BASE_URL`]. The result is validated as an http(s) URL with a
/// host and returned without a trailing slash so callers can append paths.
pub fn resolve_base_url(flag: Option<&str>) -> Result<String> {
    let raw = flag
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var(BASE_URL_ENV)
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

    let parsed = url::Url::parse(raw.trim()).map_err(|e| {
        DoSessionsError::Config(format!("invalid base URL `{raw}`: {e}"))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(DoSessionsError::Config(format!(
            "base URL `{raw}` must be http or https"
        )));
    }
    if parsed.host_str().is_none() {
        return Err(DoSessionsError::Config(format!(
            "base URL `{raw}` is missing a host"
        )));
    }

    Ok(raw.trim().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_wins_and_trailing_slash_trimmed() {
        let got = resolve_base_url(Some("https://staging.example.com/")).unwrap();
        assert_eq!(got, "https://staging.example.com");
    }

    #[test]
    fn default_when_unset() {
        // Note: relies on the env var not being set in the test environment.
        let got = resolve_base_url(None).unwrap();
        assert!(got.starts_with("https://"));
    }

    #[test]
    fn rejects_non_http_scheme() {
        assert!(resolve_base_url(Some("ftp://example.com")).is_err());
    }
}
