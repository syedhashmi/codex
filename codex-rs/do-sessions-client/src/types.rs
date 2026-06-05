//! Wire types mirroring `harness-api/public/*.proto` as emitted by
//! grpc-gateway proto3 JSON.
//!
//! Conventions that matter for (de)serialization:
//! - This harness-api gateway emits proto field names verbatim (snake_case),
//!   so message fields are snake_case on the wire. proto3 JSON parsers accept
//!   both casings on input, so snake_case request bodies are safe too.
//! - Proto enums serialize as their full SCREAMING_SNAKE name
//!   (e.g. `AGENT_KIND_CODEX_CLI`); we map those explicitly.
//! - 64-bit ints (`int64`/`uint64`) render as JSON strings; modeled as
//!   `String`/`Option<String>` to avoid decode failures.
//! - `google.protobuf.Timestamp` renders as an RFC3339 string.

use serde::Deserialize;
use serde::Serialize;

/// proto 64-bit ints are rendered as JSON strings by spec, but some gateways
/// emit them as numbers. Accept either and normalize to `Option<String>`.
fn de_string_or_number<'de, D>(deserializer: D) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StrOrNum {
        S(String),
        I(i64),
        U(u64),
        F(f64),
    }
    let opt = Option::<StrOrNum>::deserialize(deserializer)?;
    Ok(opt.map(|v| match v {
        StrOrNum::S(s) => s,
        StrOrNum::I(i) => i.to_string(),
        StrOrNum::U(u) => u.to_string(),
        StrOrNum::F(f) => f.to_string(),
    }))
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentKind {
    #[serde(rename = "AGENT_KIND_UNSPECIFIED")]
    Unspecified,
    #[serde(rename = "AGENT_KIND_CLAUDE_CODE")]
    ClaudeCode,
    #[serde(rename = "AGENT_KIND_OPENCODE")]
    OpenCode,
    #[serde(rename = "AGENT_KIND_CODEX_CLI")]
    CodexCli,
    #[serde(rename = "AGENT_KIND_CURSOR_CLI")]
    CursorCli,
    #[serde(rename = "AGENT_KIND_NONE")]
    None,
    #[serde(rename = "AGENT_KIND_CUSTOM")]
    Custom,
}

impl AgentKind {
    /// Parse a loose, user/manifest-supplied label into an [`AgentKind`].
    pub fn from_label(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "codex" | "codex-cli" | "codex_cli" => AgentKind::CodexCli,
            "claude" | "claude-code" | "claude_code" | "claudecode" => AgentKind::ClaudeCode,
            "opencode" | "open-code" => AgentKind::OpenCode,
            "cursor" | "cursor-cli" | "cursor_cli" => AgentKind::CursorCli,
            "none" | "bare" => AgentKind::None,
            "custom" => AgentKind::Custom,
            _ => return None,
        })
    }

    /// Short, user-facing label for table output.
    pub fn label(&self) -> &'static str {
        match self {
            AgentKind::Unspecified => "unspecified",
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::OpenCode => "opencode",
            AgentKind::CodexCli => "codex",
            AgentKind::CursorCli => "cursor",
            AgentKind::None => "none",
            AgentKind::Custom => "custom",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    #[serde(rename = "SESSION_STATUS_UNSPECIFIED")]
    Unspecified,
    #[serde(rename = "SESSION_STATUS_PROVISIONING")]
    Provisioning,
    #[serde(rename = "SESSION_STATUS_READY")]
    Ready,
    #[serde(rename = "SESSION_STATUS_DETACHED")]
    Detached,
    #[serde(rename = "SESSION_STATUS_DESTROYING")]
    Destroying,
    #[serde(rename = "SESSION_STATUS_DESTROYED")]
    Destroyed,
    #[serde(rename = "SESSION_STATUS_FAILED")]
    Failed,
}

impl SessionStatus {
    pub fn label(&self) -> &'static str {
        match self {
            SessionStatus::Unspecified => "unspecified",
            SessionStatus::Provisioning => "provisioning",
            SessionStatus::Ready => "ready",
            SessionStatus::Detached => "detached",
            SessionStatus::Destroying => "destroying",
            SessionStatus::Destroyed => "destroyed",
            SessionStatus::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderAuthState {
    #[serde(rename = "PROVIDER_AUTH_STATE_UNSPECIFIED")]
    Unspecified,
    #[serde(rename = "PROVIDER_AUTH_STATE_NONE")]
    None,
    #[serde(rename = "PROVIDER_AUTH_STATE_PENDING")]
    Pending,
    #[serde(rename = "PROVIDER_AUTH_STATE_AUTHORIZED")]
    Authorized,
    #[serde(rename = "PROVIDER_AUTH_STATE_EXPIRED")]
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HitlOutcome {
    #[serde(rename = "HITL_OUTCOME_UNSPECIFIED")]
    Unspecified,
    #[serde(rename = "HITL_OUTCOME_APPROVE")]
    Approve,
    #[serde(rename = "HITL_OUTCOME_REJECT")]
    Reject,
    #[serde(rename = "HITL_OUTCOME_DEFER")]
    Defer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionSource {
    #[serde(rename = "RESOLUTION_SOURCE_UNSPECIFIED")]
    Unspecified,
    #[serde(rename = "RESOLUTION_SOURCE_INLINE_KEYSTROKE")]
    InlineKeystroke,
    #[serde(rename = "RESOLUTION_SOURCE_OUT_OF_BAND")]
    OutOfBand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OAuthProvider {
    #[serde(rename = "OAUTH_PROVIDER_UNSPECIFIED")]
    Unspecified,
    #[serde(rename = "OAUTH_PROVIDER_GITHUB")]
    Github,
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct Session {
    pub session_id: String,
    #[serde(default, deserialize_with = "de_string_or_number")]
    pub team_id: Option<String>,
    pub agent_kind: Option<AgentKind>,
    pub status: Option<SessionStatus>,
    pub sandbox_id: Option<String>,
    pub created_at: Option<String>,
    pub last_event_at: Option<String>,
    pub repo_hint: Option<String>,
    #[serde(default)]
    pub provider_auth: std::collections::BTreeMap<String, ProviderAuthState>,
}

// ---------------------------------------------------------------------------
// Requests / responses
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CreateSessionRequest {
    pub agent_kind: AgentKind,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub repo_hint: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub idle_timeout_seconds: i64,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CreateSessionResponse {
    pub session: Session,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GetSessionResponse {
    pub session: Session,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct ListSessionsResponse {
    pub sessions: Vec<Session>,
    pub next_page_token: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SendInputRequest {
    pub text: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct SendInputResponse {
    pub run_id: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ResolveHitlRequest {
    pub outcome: HitlOutcome,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
    pub source: ResolutionSource,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct StartOAuthFlowRequest {
    pub provider: OAuthProvider,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub requested_scopes: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case", default)]
pub struct StartOAuthFlowResponse {
    pub authorize_url: String,
    pub flow_kind: Option<String>,
    pub device_user_code: String,
}

// ---------------------------------------------------------------------------
// Events (StreamSession SSE)
// ---------------------------------------------------------------------------

/// One event off the `StreamSession` SSE feed.
///
/// The harness wire shape is a tagged envelope: a `type` discriminator string
/// (e.g. `"run.started"`, `"session.updated"`) plus a free-form `data` object
/// whose shape depends on `type`. Field accessors below dig into `data`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Event {
    pub event_id: String,
    pub session_id: String,
    pub run_id: String,
    pub tenant_id: Option<String>,
    pub timestamp: Option<String>,
    pub seq: Option<i64>,
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: serde_json::Value,
}

/// Coarse classification derived from [`Event::event_type`]. Matching is by
/// known names with substring fallbacks so minor server naming differences
/// (`run.token_delta` vs `token.chunk`) still classify correctly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    RunStarted,
    TokenChunk,
    ToolCallStarted,
    ToolCallCompleted,
    HitlRequested,
    HitlResolved,
    RunCompleted,
    RunFailed,
    RunPaused,
    RunResumed,
    SessionUpdated,
    Unknown,
}

impl Event {
    pub fn kind(&self) -> EventKind {
        let t = self.event_type.as_str();
        match t {
            // Canonical Plano OAH taxonomy (the harness emits these names).
            "run.started" => EventKind::RunStarted,
            "run.token_delta" => EventKind::TokenChunk,
            "run.tool_call_started" => EventKind::ToolCallStarted,
            "run.tool_call_completed" => EventKind::ToolCallCompleted,
            "run.human_input_requested" => EventKind::HitlRequested,
            "run.human_input_received" => EventKind::HitlResolved,
            "run.completed" => EventKind::RunCompleted,
            "run.failed" => EventKind::RunFailed,
            "run.paused" => EventKind::RunPaused,
            "run.resumed" => EventKind::RunResumed,
            "session.updated" => EventKind::SessionUpdated,
            _ => {
                // Substring fallbacks for naming drift.
                let has = |needle: &str| t.contains(needle);
                if has("token") {
                    EventKind::TokenChunk
                } else if has("tool") && has("complet") {
                    EventKind::ToolCallCompleted
                } else if has("tool") {
                    EventKind::ToolCallStarted
                } else if has("human_input") && has("received") {
                    EventKind::HitlResolved
                } else if has("human_input") || has("hitl") {
                    EventKind::HitlRequested
                } else if has("run") && has("start") {
                    EventKind::RunStarted
                } else if has("run") && has("complet") {
                    EventKind::RunCompleted
                } else if has("run") && has("fail") {
                    EventKind::RunFailed
                } else if has("session") {
                    EventKind::SessionUpdated
                } else {
                    EventKind::Unknown
                }
            }
        }
    }

    /// Streamed assistant text, from `data.text` or `data.delta`.
    pub fn text(&self) -> Option<&str> {
        self.data
            .get("text")
            .or_else(|| self.data.get("delta"))
            .and_then(serde_json::Value::as_str)
    }

    /// Tool name for tool-call events (`data.tool_name` or `data.name`).
    pub fn tool_name(&self) -> Option<&str> {
        self.data
            .get("tool_name")
            .or_else(|| self.data.get("name"))
            .and_then(serde_json::Value::as_str)
    }

    /// HITL correlation id, from `data.hitl_id` (preferred) or the proto-shaped
    /// `data.request.request_id` / `data.request_id`.
    pub fn hitl_request_id(&self) -> Option<String> {
        self.data
            .get("hitl_id")
            .or_else(|| self.data.get("request").and_then(|r| r.get("request_id")))
            .or_else(|| self.data.get("request_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    }

    /// The HITL `payload` object, when present and non-null.
    pub fn hitl_payload(&self) -> Option<&serde_json::Value> {
        self.data.get("payload").filter(|v| !v.is_null())
    }

    /// The command awaiting approval (`data.payload.command`).
    pub fn hitl_command(&self) -> Option<&str> {
        self.hitl_payload()
            .and_then(|p| p.get("command"))
            .and_then(serde_json::Value::as_str)
    }

    /// The HITL action kind (`data.payload.kind`, e.g. `command_execution`).
    pub fn hitl_action(&self) -> Option<&str> {
        self.hitl_payload()
            .and_then(|p| p.get("kind"))
            .and_then(serde_json::Value::as_str)
    }

    /// Best-effort string field lookup inside `data`.
    pub fn data_str(&self, key: &str) -> Option<&str> {
        self.data.get(key).and_then(serde_json::Value::as_str)
    }

    /// Compact one-line JSON of `data` for diagnostic rendering.
    pub fn data_compact(&self) -> String {
        if self.data.is_null() {
            String::new()
        } else {
            self.data.to_string()
        }
    }
}
