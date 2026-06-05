use std::collections::VecDeque;

use futures::Stream;
use futures::StreamExt;

use crate::error::DoSessionsError;
use crate::error::Result;
use crate::types::*;

/// Client for the harness-api REST surface. Cheap to clone (wraps a
/// `reqwest::Client`).
#[derive(Clone)]
pub struct DoSessionsClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
    user_agent: String,
}

impl DoSessionsClient {
    /// `base_url` should be the resolved host (no trailing slash); `token` a
    /// DO IAM bearer token. See `resolve_base_url` / `resolve_token`.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| DoSessionsError::Transport(e.to_string()))?;
        Ok(Self {
            base_url: base_url.into(),
            token: token.into(),
            http,
            user_agent: format!("codex-do-sessions/{}", env!("CARGO_PKG_VERSION")),
        })
    }

    pub fn with_user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn authed(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder
            .bearer_auth(&self.token)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(reqwest::header::ACCEPT, "application/json")
    }

    async fn decode<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| DoSessionsError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(DoSessionsError::Api {
                status: status.as_u16(),
                body,
            });
        }
        serde_json::from_str::<T>(&body).map_err(|e| {
            DoSessionsError::Decode(format!("{e}; body was: {}", truncate(&body, 500)))
        })
    }

    async fn expect_ok(resp: reqwest::Response) -> Result<()> {
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(DoSessionsError::Api {
            status: status.as_u16(),
            body,
        })
    }

    // --- session lifecycle -------------------------------------------------

    pub async fn create_session(&self, req: &CreateSessionRequest) -> Result<Session> {
        let resp = self
            .authed(self.http.post(self.url("/v2/agents/sessions")))
            .json(req)
            .send()
            .await
            .map_err(|e| DoSessionsError::Transport(e.to_string()))?;
        let parsed: CreateSessionResponse = Self::decode(resp).await?;
        Ok(parsed.session)
    }

    /// Create a session from an inline YAML `Agent` manifest. The body is sent
    /// verbatim with `Content-Type: application/x-yaml`; the server parses the
    /// manifest (runtime adapter, sandbox, env injected into the VM).
    pub async fn create_session_from_manifest(&self, yaml: &str) -> Result<Session> {
        let resp = self
            .authed(self.http.post(self.url("/v2/agents/sessions")))
            .header(reqwest::header::CONTENT_TYPE, "application/x-yaml")
            .body(yaml.to_string())
            .send()
            .await
            .map_err(|e| DoSessionsError::Transport(e.to_string()))?;
        let parsed: CreateSessionResponse = Self::decode(resp).await?;
        Ok(parsed.session)
    }

    pub async fn list_sessions(
        &self,
        status: Option<SessionStatus>,
        page_token: Option<&str>,
        page_size: Option<i32>,
    ) -> Result<ListSessionsResponse> {
        let mut req = self.authed(self.http.get(self.url("/v2/agents/sessions")));
        let mut query: Vec<(String, String)> = Vec::new();
        if let Some(status) = status {
            // Enum query params use the proto SCREAMING_SNAKE name.
            if let Ok(serde_json::Value::String(s)) = serde_json::to_value(status) {
                query.push(("status".to_string(), s));
            }
        }
        if let Some(token) = page_token {
            query.push(("page_token".to_string(), token.to_string()));
        }
        if let Some(size) = page_size {
            query.push(("page_size".to_string(), size.to_string()));
        }
        if !query.is_empty() {
            req = req.query(&query);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| DoSessionsError::Transport(e.to_string()))?;
        Self::decode(resp).await
    }

    pub async fn get_session(&self, session_id: &str) -> Result<Session> {
        let resp = self
            .authed(
                self.http
                    .get(self.url(&format!("/v2/agents/sessions/{session_id}"))),
            )
            .send()
            .await
            .map_err(|e| DoSessionsError::Transport(e.to_string()))?;
        let parsed: GetSessionResponse = Self::decode(resp).await?;
        Ok(parsed.session)
    }

    pub async fn destroy_session(&self, session_id: &str) -> Result<()> {
        let resp = self
            .authed(
                self.http
                    .delete(self.url(&format!("/v2/agents/sessions/{session_id}"))),
            )
            .send()
            .await
            .map_err(|e| DoSessionsError::Transport(e.to_string()))?;
        Self::expect_ok(resp).await
    }

    // --- interaction -------------------------------------------------------

    /// Send a chat message; returns the attributed `run_id`.
    pub async fn send_input(&self, session_id: &str, text: &str) -> Result<String> {
        let resp = self
            .authed(
                self.http
                    .post(self.url(&format!("/v2/agents/sessions/{session_id}/input"))),
            )
            .json(&SendInputRequest {
                text: text.to_string(),
            })
            .send()
            .await
            .map_err(|e| DoSessionsError::Transport(e.to_string()))?;
        let parsed: SendInputResponse = Self::decode(resp).await?;
        Ok(parsed.run_id)
    }

    pub async fn resolve_hitl(
        &self,
        session_id: &str,
        request_id: &str,
        outcome: HitlOutcome,
        reason: Option<&str>,
        source: ResolutionSource,
    ) -> Result<()> {
        let resp = self
            .authed(self.http.post(self.url(&format!(
                "/v2/agents/sessions/{session_id}/hitl/{request_id}"
            ))))
            .json(&ResolveHitlRequest {
                outcome,
                reason: reason.unwrap_or("").to_string(),
                source,
            })
            .send()
            .await
            .map_err(|e| DoSessionsError::Transport(e.to_string()))?;
        Self::expect_ok(resp).await
    }

    pub async fn start_oauth(
        &self,
        session_id: &str,
        provider: OAuthProvider,
        requested_scopes: Vec<String>,
    ) -> Result<StartOAuthFlowResponse> {
        let provider_path = match provider {
            OAuthProvider::Github => "github",
            _ => "unspecified",
        };
        let resp = self
            .authed(self.http.post(self.url(&format!(
                "/v2/agents/sessions/{session_id}/oauth/{provider_path}"
            ))))
            .json(&StartOAuthFlowRequest {
                provider,
                requested_scopes,
            })
            .send()
            .await
            .map_err(|e| DoSessionsError::Transport(e.to_string()))?;
        Self::decode(resp).await
    }

    // --- event stream ------------------------------------------------------

    /// URL for the SSE stream, including replay parameters.
    pub fn stream_url(
        &self,
        session_id: &str,
        replay_from: Option<&str>,
        replay_only: bool,
    ) -> String {
        let mut url = self.url(&format!("/v2/agents/sessions/{session_id}/stream"));
        let mut params: Vec<String> = Vec::new();
        if let Some(cursor) = replay_from {
            params.push(format!("replay_from={cursor}"));
        }
        if replay_only {
            params.push("replay_only=true".to_string());
        }
        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }
        url
    }

    /// Open the `StreamSession` feed and yield parsed [`Event`]s.
    ///
    /// The harness gateway may frame the stream as SSE (`data: {...}`) or as
    /// newline-delimited JSON (`{"result": {...}}\n`). This reader handles both:
    /// it splits the body on newlines, strips any SSE field prefix, and unwraps
    /// the grpc-gateway `{"result": ...}` / `{"error": ...}` envelope.
    pub async fn stream_session(
        &self,
        session_id: &str,
        replay_from: Option<&str>,
        replay_only: bool,
    ) -> Result<impl Stream<Item = Result<Event>>> {
        let url = self.stream_url(session_id, replay_from, replay_only);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(|e| DoSessionsError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(DoSessionsError::Api {
                status: status.as_u16(),
                body,
            });
        }

        struct StreamState {
            inner: std::pin::Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
            buf: String,
            pending: VecDeque<Result<Event>>,
            done: bool,
        }

        let state = StreamState {
            inner: Box::pin(resp.bytes_stream()),
            buf: String::new(),
            pending: VecDeque::new(),
            done: false,
        };

        let stream = futures::stream::unfold(state, |mut state| async move {
            loop {
                if let Some(item) = state.pending.pop_front() {
                    return Some((item, state));
                }
                if state.done {
                    return None;
                }
                match state.inner.next().await {
                    Some(Ok(bytes)) => {
                        state.buf.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(idx) = state.buf.find('\n') {
                            let line: String = state.buf.drain(..=idx).collect();
                            if let Some(ev) = parse_stream_line(line.trim_end_matches(['\r', '\n'])) {
                                state.pending.push_back(ev);
                            }
                        }
                        // Loop to drain pending or read more bytes.
                    }
                    Some(Err(e)) => {
                        state.done = true;
                        return Some((Err(DoSessionsError::Stream(e.to_string())), state));
                    }
                    None => {
                        state.done = true;
                        let leftover = state.buf.trim().to_string();
                        state.buf.clear();
                        if let Some(ev) = parse_stream_line(&leftover) {
                            return Some((ev, state));
                        }
                        return None;
                    }
                }
            }
        });
        Ok(stream)
    }
}

/// Parse a single stream line into an [`Event`], tolerating SSE field prefixes
/// and newline-delimited JSON. Returns `None` for blank lines, SSE comments
/// (`:`), and non-data SSE fields (`event:`/`id:`/`retry:`).
fn parse_stream_line(line: &str) -> Option<Result<Event>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return None;
    }
    let data = if let Some(rest) = line.strip_prefix("data:") {
        rest.trim()
    } else if line.starts_with("event:")
        || line.starts_with("id:")
        || line.starts_with("retry:")
    {
        return None;
    } else {
        line
    };
    if data.is_empty() {
        return None;
    }
    Some(parse_event_frame(data))
}

/// Parse one SSE `data:` payload into an [`Event`], unwrapping the
/// grpc-gateway `{"result": ...}` envelope when present.
fn parse_event_frame(data: &str) -> Result<Event> {
    #[derive(serde::Deserialize)]
    struct Envelope {
        result: Option<Event>,
        error: Option<serde_json::Value>,
    }

    if let Ok(env) = serde_json::from_str::<Envelope>(data) {
        if let Some(err) = env.error {
            return Err(DoSessionsError::Stream(err.to_string()));
        }
        if let Some(event) = env.result {
            return Ok(event);
        }
    }

    serde_json::from_str::<Event>(data)
        .map_err(|e| DoSessionsError::Decode(format!("{e}; frame was: {}", truncate(data, 500))))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}
