/*
This module implements the DigitalOcean hosted-agent app-server transport.

`DoSessionAppServerClient` speaks the Codex app-server protocol surface
(`AppServerEvent` out, typed `ClientRequest` in) but is backed by the DO
harness-api REST + SSE feed via `codex-do-sessions-client`. It lets the native
Codex TUI drive a DO-hosted session without an embedded or remote app-server.

The shim has two halves:

- A background worker task drains the DO SSE stream
  (`DoSessionsClient::stream_session`), translates each DO `run.*` event into a
  codex `ServerNotification` / `ServerRequest`, and pushes it onto an
  `mpsc::UnboundedSender<AppServerEvent>` consumed by `next_event()`.
- `request()` either synthesizes a handshake/bootstrap response (initialize,
  account, model list, thread/start, ...) or translates a turn request into a
  DO `POST /input` call. `resolve_server_request()` / `reject_server_request()`
  map the typed decision back onto a DO `POST /hitl/{id}` resolution.

All codex-specific translation lives here. `codex-do-sessions-client` stays
codex-agnostic.
*/

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Error as IoError;
use std::io::Result as IoResult;
use std::sync::Arc;
use std::sync::Mutex;

use crate::AppServerEvent;
use crate::RequestResult;
use crate::TypedRequestError;
use crate::request_method_name;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result as JsonRpcResult;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_do_sessions_client::DoSessionsClient;
use codex_do_sessions_client::Event;
use codex_do_sessions_client::HitlOutcome;
use codex_do_sessions_client::ResolutionSource;
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::warn;

const DO_CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Absolute path reported as the server `$CODEX_HOME` and thread cwd. The DO
/// workspace lives on the droplet, so the local TUI only needs a syntactically
/// valid absolute path here.
const DO_REMOTE_CWD: &str = "/";

/// Connection inputs for a DO-hosted session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoSessionConnectArgs {
    /// Existing harness-api session id to attach to.
    pub session_id: String,
    /// Resolved harness-api base URL (see `do_sessions_client::resolve_base_url`).
    pub base_url: String,
    /// DO IAM bearer token (see `do_sessions_client::resolve_token`).
    pub token: String,
}

/// Shared map from a synthesized client-facing `RequestId` to the DO `hitl_id`
/// it corresponds to. Populated by the SSE worker when it emits a
/// `ServerRequest`; read by `resolve_server_request` / `reject_server_request`.
type HitlMap = Arc<Mutex<HashMap<RequestId, String>>>;

/// App-server client backed by the DigitalOcean harness-api.
pub struct DoSessionAppServerClient {
    session_id: String,
    client: DoSessionsClient,
    event_rx: mpsc::UnboundedReceiver<AppServerEvent>,
    pending_events: VecDeque<AppServerEvent>,
    hitl_map: HitlMap,
    worker_handle: tokio::task::JoinHandle<()>,
}

/// Clone-able request handle mirroring `RemoteAppServerRequestHandle`.
#[derive(Clone)]
pub struct DoSessionAppServerRequestHandle {
    session_id: String,
    client: DoSessionsClient,
}

impl DoSessionAppServerClient {
    pub async fn connect(args: DoSessionConnectArgs) -> IoResult<Self> {
        let DoSessionConnectArgs {
            session_id,
            base_url,
            token,
        } = args;

        let client = DoSessionsClient::new(base_url, token)
            .map_err(|err| IoError::other(format!("failed to build DO sessions client: {err}")))?
            .with_user_agent(format!("codex-do-session/{DO_CLIENT_VERSION}"));

        let (event_tx, event_rx) = mpsc::unbounded_channel::<AppServerEvent>();
        let hitl_map: HitlMap = Arc::new(Mutex::new(HashMap::new()));

        let worker_handle = tokio::spawn(run_event_worker(
            client.clone(),
            session_id.clone(),
            event_tx,
            hitl_map.clone(),
        ));

        Ok(Self {
            session_id,
            client,
            event_rx,
            pending_events: VecDeque::new(),
            hitl_map,
            worker_handle,
        })
    }

    pub fn request_handle(&self) -> DoSessionAppServerRequestHandle {
        DoSessionAppServerRequestHandle {
            session_id: self.session_id.clone(),
            client: self.client.clone(),
        }
    }

    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        handle_client_request(&self.client, &self.session_id, request).await
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        request_typed_via(&self.client, &self.session_id, request).await
    }

    pub async fn notify(&self, notification: ClientNotification) -> IoResult<()> {
        // The DO transport only needs the `initialized` handshake notification,
        // which is a no-op. Everything else is silently accepted.
        let _ = notification;
        Ok(())
    }

    pub async fn resolve_server_request(
        &self,
        request_id: RequestId,
        result: JsonRpcResult,
    ) -> IoResult<()> {
        let outcome = decision_to_outcome(&result);
        self.resolve_hitl(&request_id, outcome).await
    }

    pub async fn reject_server_request(
        &self,
        request_id: RequestId,
        error: JSONRPCErrorError,
    ) -> IoResult<()> {
        let _ = error;
        self.resolve_hitl(&request_id, HitlOutcome::Reject).await
    }

    async fn resolve_hitl(&self, request_id: &RequestId, outcome: HitlOutcome) -> IoResult<()> {
        let hitl_id = {
            let mut map = self
                .hitl_map
                .lock()
                .map_err(|_| IoError::other("DO session hitl map poisoned"))?;
            map.remove(request_id)
        };
        let Some(hitl_id) = hitl_id else {
            warn!(%request_id, "no DO hitl_id mapped for server request id");
            return Ok(());
        };
        self.client
            .resolve_hitl(
                &self.session_id,
                &hitl_id,
                outcome,
                None,
                ResolutionSource::InlineKeystroke,
            )
            .await
            .map_err(|err| IoError::other(format!("failed to resolve DO hitl `{hitl_id}`: {err}")))
    }

    pub async fn next_event(&mut self) -> Option<AppServerEvent> {
        if let Some(event) = self.pending_events.pop_front() {
            return Some(event);
        }
        self.event_rx.recv().await
    }

    pub async fn shutdown(self) -> IoResult<()> {
        self.worker_handle.abort();
        let _ = self.worker_handle.await;
        Ok(())
    }
}

impl DoSessionAppServerRequestHandle {
    pub async fn request(&self, request: ClientRequest) -> IoResult<RequestResult> {
        handle_client_request(&self.client, &self.session_id, request).await
    }

    pub async fn request_typed<T>(&self, request: ClientRequest) -> Result<T, TypedRequestError>
    where
        T: DeserializeOwned,
    {
        request_typed_via(&self.client, &self.session_id, request).await
    }
}

async fn request_typed_via<T>(
    client: &DoSessionsClient,
    session_id: &str,
    request: ClientRequest,
) -> Result<T, TypedRequestError>
where
    T: DeserializeOwned,
{
    let method = request_method_name(&request);
    let response = handle_client_request(client, session_id, request)
        .await
        .map_err(|source| TypedRequestError::Transport {
            method: method.clone(),
            source,
        })?;
    let result = response.map_err(|source| TypedRequestError::Server {
        method: method.clone(),
        source,
    })?;
    serde_json::from_value(result)
        .map_err(|source| TypedRequestError::Deserialize { method, source })
}

/// Synthesize a handshake/bootstrap response or translate a turn request into a
/// DO REST call.
async fn handle_client_request(
    client: &DoSessionsClient,
    session_id: &str,
    request: ClientRequest,
) -> IoResult<RequestResult> {
    let value = serde_json::to_value(&request)
        .map_err(|err| IoError::other(format!("failed to serialize client request: {err}")))?;
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params = value.get("params").cloned().unwrap_or(Value::Null);

    let result: Value = match method.as_str() {
        "initialize" => json!({
            "userAgent": format!("codex-do-session/{DO_CLIENT_VERSION} (DigitalOcean) rust"),
            "codexHome": DO_REMOTE_CWD,
            "platformFamily": "unix",
            "platformOs": "linux",
        }),
        // DO sessions authenticate server-side (the session VM holds the
        // OPENAI_API_KEY), so report an already-authenticated account. This
        // makes the TUI skip the API-key onboarding screen entirely.
        "account/read" => json!({
            "account": { "type": "apiKey" },
            "requiresOpenaiAuth": false,
        }),
        // Defensive: if onboarding is ever reached, accept an apiKey login so
        // the response decodes (LoginAccountResponse::ApiKey -> {"type":"apiKey"}).
        "account/login/start" => json!({ "type": "apiKey" }),
        "model/list" => json!({
            "data": [synth_model_json()],
            "nextCursor": null,
        }),
        // Plugin discovery: the DO session manages its own skills/hooks, so
        // report none. The TUI decodes `{ "data": [...] }` for both.
        "skills/list" | "hooks/list" => json!({ "data": [] }),
        "thread/start" | "thread/resume" => {
            thread_start_response_json(&codex_thread_id(session_id))
        }
        "thread/read" => json!({ "thread": thread_json(&codex_thread_id(session_id)) }),
        "turn/start" => {
            let text = extract_input_text(&params);
            let run_id = client
                .send_input(session_id, &text)
                .await
                .map_err(|err| IoError::other(format!("DO send_input failed: {err}")))?;
            json!({ "turn": turn_json(&run_id, "inProgress") })
        }
        "turn/steer" => {
            let text = extract_input_text(&params);
            let run_id = client
                .send_input(session_id, &text)
                .await
                .map_err(|err| IoError::other(format!("DO send_input failed: {err}")))?;
            json!({ "turnId": run_id })
        }
        "turn/interrupt" => json!({}),
        other => {
            warn!("unhandled DoSession ClientRequest method: {other}");
            json!({})
        }
    };

    Ok(Ok(result))
}

/// Concatenate the text content of `turn/start` / `turn/steer` input items.
fn extract_input_text(params: &Value) -> String {
    params
        .get("input")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    if item.get("type").and_then(Value::as_str) == Some("text") {
                        item.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Map a typed approval decision JSON (`{ "decision": "accept" | ... }`) onto a
/// DO HITL outcome. Unknown shapes default to approval.
fn decision_to_outcome(result: &Value) -> HitlOutcome {
    match result.get("decision").and_then(Value::as_str) {
        Some("decline") => HitlOutcome::Reject,
        Some("cancel") => HitlOutcome::Defer,
        // "accept" / "acceptForSession" / anything else approves.
        _ => HitlOutcome::Approve,
    }
}

// ---------------------------------------------------------------------------
// JSON shape helpers (camelCase, matching the app-server-protocol serde shapes)
// ---------------------------------------------------------------------------

/// Deterministic codex thread id (UUID) derived from the DO session id. The
/// TUI parses `thread.id` as a UUID, so the `sess_...` id can't be used
/// directly; v5 keeps it stable across reconnects/resume of the same session.
fn codex_thread_id(session_id: &str) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, session_id.as_bytes()).to_string()
}

fn thread_json(session_id: &str) -> Value {
    json!({
        "id": session_id,
        "sessionId": session_id,
        "forkedFromId": null,
        "preview": "",
        "ephemeral": false,
        "modelProvider": "openai",
        "createdAt": 0,
        "updatedAt": 0,
        "status": { "type": "idle" },
        "path": null,
        "cwd": DO_REMOTE_CWD,
        "cliVersion": DO_CLIENT_VERSION,
        "source": "appServer",
        "threadSource": null,
        "agentNickname": null,
        "agentRole": null,
        "gitInfo": null,
        "name": null,
        "turns": [],
    })
}

fn thread_start_response_json(session_id: &str) -> Value {
    json!({
        "thread": thread_json(session_id),
        "model": "codex",
        "modelProvider": "openai",
        "serviceTier": null,
        "cwd": DO_REMOTE_CWD,
        "runtimeWorkspaceRoots": [],
        "instructionSources": [],
        "approvalPolicy": "on-request",
        "approvalsReviewer": "user",
        "sandbox": { "type": "dangerFullAccess" },
        "activePermissionProfile": null,
        "reasoningEffort": null,
    })
}

fn turn_json(turn_id: &str, status: &str) -> Value {
    json!({
        "id": turn_id,
        "items": [],
        "itemsView": "full",
        "status": status,
        "error": null,
        "startedAt": null,
        "completedAt": null,
        "durationMs": null,
    })
}

fn failed_turn_json(turn_id: &str, message: &str) -> Value {
    json!({
        "id": turn_id,
        "items": [],
        "itemsView": "full",
        "status": "failed",
        "error": {
            "message": message,
            "codexErrorInfo": null,
            "additionalDetails": null,
        },
        "startedAt": null,
        "completedAt": null,
        "durationMs": null,
    })
}

fn synth_model_json() -> Value {
    json!({
        "id": "codex",
        "model": "codex",
        "upgrade": null,
        "upgradeInfo": null,
        "availabilityNux": null,
        "displayName": "Codex (DO session)",
        "description": "Model served by the DigitalOcean hosted agent.",
        "hidden": false,
        "supportedReasoningEfforts": [],
        "defaultReasoningEffort": "medium",
        "inputModalities": ["text"],
        "supportsPersonality": false,
        "additionalSpeedTiers": [],
        "serviceTiers": [],
        "defaultServiceTier": null,
        "isDefault": true,
    })
}

fn command_execution_item_json(item_id: &str, command: &str, status: &str) -> Value {
    json!({
        "type": "commandExecution",
        "id": item_id,
        "command": command,
        "cwd": DO_REMOTE_CWD,
        "processId": null,
        "source": "agent",
        "status": status,
        "commandActions": [],
        "aggregatedOutput": null,
        "exitCode": null,
        "durationMs": null,
    })
}

// ---------------------------------------------------------------------------
// Event translation: DO `run.*` -> AppServerEvent
// ---------------------------------------------------------------------------

async fn run_event_worker(
    client: DoSessionsClient,
    session_id: String,
    event_tx: mpsc::UnboundedSender<AppServerEvent>,
    hitl_map: HitlMap,
) {
    let stream = match client.stream_session(&session_id, None, false).await {
        Ok(stream) => stream,
        Err(err) => {
            let _ = event_tx.send(AppServerEvent::Disconnected {
                message: format!("failed to open DO session stream: {err}"),
            });
            return;
        }
    };
    tokio::pin!(stream);

    let mut thread_started = false;
    let mut next_request_id: i64 = 1;

    while let Some(item) = stream.next().await {
        match item {
            Ok(event) => {
                let events = translate_event(
                    &session_id,
                    &event,
                    &mut thread_started,
                    &hitl_map,
                    &mut next_request_id,
                );
                for app_event in events {
                    if event_tx.send(app_event).is_err() {
                        return;
                    }
                }
            }
            Err(err) => {
                let _ = event_tx.send(AppServerEvent::Disconnected {
                    message: format!("DO session stream error: {err}"),
                });
                return;
            }
        }
    }

    let _ = event_tx.send(AppServerEvent::Disconnected {
        message: format!("DO session `{session_id}` stream closed"),
    });
}

fn translate_event(
    session_id: &str,
    ev: &Event,
    thread_started: &mut bool,
    hitl_map: &HitlMap,
    next_request_id: &mut i64,
) -> Vec<AppServerEvent> {
    let thread_id_owned = codex_thread_id(session_id);
    let thread_id = thread_id_owned.as_str();
    let turn_id = ev.run_id.as_str();

    match ev.event_type.as_str() {
        "run.started" => {
            let mut out = Vec::new();
            if !*thread_started {
                *thread_started = true;
                if let Some(event) =
                    notification_event("thread/started", json!({ "thread": thread_json(thread_id) }))
                {
                    out.push(event);
                }
            }
            if let Some(event) = notification_event(
                "turn/started",
                json!({
                    "threadId": thread_id,
                    "turn": turn_json(turn_id, "inProgress"),
                }),
            ) {
                out.push(event);
            }
            out
        }
        "run.token_delta" => {
            let delta = ev.text().unwrap_or_default();
            if is_reasoning_channel(ev) {
                let item_id = format!("{turn_id}-reasoning");
                notification_event(
                    "item/reasoning/textDelta",
                    json!({
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "itemId": item_id,
                        "delta": delta,
                        "contentIndex": 0,
                    }),
                )
                .into_iter()
                .collect()
            } else {
                let item_id = format!("{turn_id}-message");
                notification_event(
                    "item/agentMessage/delta",
                    json!({
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "itemId": item_id,
                        "delta": delta,
                    }),
                )
                .into_iter()
                .collect()
            }
        }
        "run.tool_call_started" => {
            let item_id = tool_call_item_id(ev);
            let command = ev.tool_name().unwrap_or("tool").to_string();
            notification_event(
                "item/started",
                json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "item": command_execution_item_json(&item_id, &command, "inProgress"),
                    "startedAtMs": 0,
                }),
            )
            .into_iter()
            .collect()
        }
        "run.tool_call_completed" => {
            let item_id = tool_call_item_id(ev);
            let command = ev.tool_name().unwrap_or("tool").to_string();
            notification_event(
                "item/completed",
                json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "item": command_execution_item_json(&item_id, &command, "completed"),
                    "completedAtMs": 0,
                }),
            )
            .into_iter()
            .collect()
        }
        "run.usage_recorded" => notification_event(
            "thread/tokenUsage/updated",
            json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "tokenUsage": token_usage_json(ev),
            }),
        )
        .into_iter()
        .collect(),
        "run.completed" => notification_event(
            "turn/completed",
            json!({
                "threadId": thread_id,
                "turn": turn_json(turn_id, "completed"),
            }),
        )
        .into_iter()
        .collect(),
        "run.failed" => {
            let message = ev
                .data_str("error")
                .or_else(|| ev.data_str("message"))
                .unwrap_or("run failed");
            notification_event(
                "turn/completed",
                json!({
                    "threadId": thread_id,
                    "turn": failed_turn_json(turn_id, message),
                }),
            )
            .into_iter()
            .collect()
        }
        "run.human_input_requested" => translate_hitl_request(
            thread_id,
            turn_id,
            ev,
            hitl_map,
            next_request_id,
        )
        .into_iter()
        .collect(),
        _ => Vec::new(),
    }
}

/// Translate a DO `run.human_input_requested` event into a codex
/// `ServerRequest`, assigning a fresh `RequestId` and recording the
/// `RequestId -> hitl_id` mapping used by `resolve_server_request`.
fn translate_hitl_request(
    thread_id: &str,
    turn_id: &str,
    ev: &Event,
    hitl_map: &HitlMap,
    next_request_id: &mut i64,
) -> Option<AppServerEvent> {
    let hitl_id = ev.hitl_request_id()?;
    let payload = ev.hitl_payload().cloned().unwrap_or(Value::Null);
    let kind = ev.hitl_action().unwrap_or("command_execution");
    let item_id = payload
        .get("itemId")
        .and_then(Value::as_str)
        .or_else(|| payload.get("item_id").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| format!("{turn_id}-hitl"));

    let request_id = RequestId::Integer(*next_request_id);
    *next_request_id += 1;

    let (method, params) = match kind {
        "file_change" => (
            "item/fileChange/requestApproval",
            json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "itemId": item_id,
                "startedAtMs": 0,
                "reason": payload.get("reason").and_then(Value::as_str),
                "grantRoot": null,
            }),
        ),
        "requestUserInput" | "request_user_input" => (
            "item/tool/requestUserInput",
            json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "itemId": item_id,
                "questions": [],
            }),
        ),
        // command_execution and unknown kinds map to a command approval prompt.
        _ => (
            "item/commandExecution/requestApproval",
            json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "itemId": item_id,
                "startedAtMs": 0,
                "command": ev.hitl_command(),
                "cwd": payload.get("cwd").and_then(Value::as_str),
                "availableDecisions": payload.get("availableDecisions").cloned(),
            }),
        ),
    };

    let event = request_event(request_id.clone(), method, params)?;
    if let Ok(mut map) = hitl_map.lock() {
        map.insert(request_id, hitl_id);
    }
    Some(event)
}

fn is_reasoning_channel(ev: &Event) -> bool {
    let matches = |v: Option<&str>| matches!(v, Some(c) if c.eq_ignore_ascii_case("reasoning"));
    matches(ev.data_str("channel")) || matches(ev.data_str("field"))
}

fn tool_call_item_id(ev: &Event) -> String {
    ev.data_str("tool_call_id")
        .or_else(|| ev.data_str("id"))
        .map(str::to_string)
        .unwrap_or_else(|| format!("{}-tool", ev.run_id))
}

fn token_usage_json(ev: &Event) -> Value {
    let read = |key: &str| ev.data.get(key).and_then(Value::as_i64).unwrap_or(0);
    let breakdown = json!({
        "totalTokens": read("total_tokens"),
        "inputTokens": read("input_tokens"),
        "cachedInputTokens": read("cached_input_tokens"),
        "outputTokens": read("output_tokens"),
        "reasoningOutputTokens": read("reasoning_output_tokens"),
    });
    json!({
        "total": breakdown,
        "last": breakdown,
        "modelContextWindow": null,
    })
}

/// Build a `ServerNotification`-carrying `AppServerEvent` by round-tripping a
/// `JSONRPCNotification` through `ServerNotification::try_from`.
fn notification_event(method: &str, params: Value) -> Option<AppServerEvent> {
    let notification = JSONRPCNotification {
        method: method.to_string(),
        params: Some(params),
    };
    match ServerNotification::try_from(notification) {
        Ok(notification) => Some(AppServerEvent::ServerNotification(notification)),
        Err(err) => {
            warn!(%err, method, "failed to build DO session server notification");
            None
        }
    }
}

/// Build a `ServerRequest`-carrying `AppServerEvent` by round-tripping a
/// `JSONRPCRequest` through `ServerRequest::try_from`.
fn request_event(request_id: RequestId, method: &str, params: Value) -> Option<AppServerEvent> {
    let request = JSONRPCRequest {
        id: request_id,
        method: method.to_string(),
        params: Some(params),
        trace: None,
    };
    match ServerRequest::try_from(request) {
        Ok(request) => Some(AppServerEvent::ServerRequest(request)),
        Err(err) => {
            warn!(%err, method, "failed to build DO session server request");
            None
        }
    }
}
