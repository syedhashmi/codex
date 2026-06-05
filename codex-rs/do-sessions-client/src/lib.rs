//! Client for the DigitalOcean Hosted Agents (Open Harness Server) API.
//!
//! Wraps the `harness-api` REST surface (emitted by grpc-gateway from
//! `harness.proto`) that backs the `doctl agents` verbs:
//!
//! - `CreateSession`  -> `POST   /v2/agents/sessions`
//! - `ListSessions`   -> `GET    /v2/agents/sessions`
//! - `GetSession`     -> `GET    /v2/agents/sessions/{id}`
//! - `DestroySession` -> `DELETE /v2/agents/sessions/{id}`
//! - `StreamSession`  -> `GET    /v2/agents/sessions/{id}/stream` (SSE)
//! - `SendInput`      -> `POST   /v2/agents/sessions/{id}/input`
//! - `ResolveHITL`    -> `POST   /v2/agents/sessions/{id}/hitl/{request_id}`
//! - `StartOAuthFlow` -> `POST   /v2/agents/sessions/{id}/oauth/{provider}`
//!
//! The base URL is configurable (public vs staging) via [`config`].
//! Authentication reuses the caller's DO IAM token from `doctl`.

mod auth;
mod client;
mod config;
mod error;
mod manifest;
mod types;

pub use auth::resolve_token;
pub use client::DoSessionsClient;
pub use manifest::ManifestHints;
pub use manifest::parse_manifest;
pub use config::DEFAULT_BASE_URL;
pub use config::resolve_base_url;
pub use config::BASE_URL_ENV;
pub use error::DoSessionsError;
pub use error::Result;
pub use types::*;
