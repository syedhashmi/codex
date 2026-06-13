//! `codex do-sessions` — manage and interact with DigitalOcean-hosted agent
//! sessions through the harness-api REST surface.
//!
//! Lifecycle + interaction verbs map 1:1 onto the RPCs in
//! `harness-api/public/harness.proto`. The interactive `attach` (binding the
//! TUI's I/O to a session) lands in a later change; `stream` here is the raw
//! event feed used to validate the SSE path.

use anyhow::Context;
use anyhow::anyhow;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use codex_arg0::Arg0DispatchPaths;
use codex_do_sessions_client::AgentKind;
use codex_do_sessions_client::DoSessionsClient;
use codex_do_sessions_client::EventKind;
use codex_do_sessions_client::HitlOutcome;
use codex_do_sessions_client::OAuthProvider;
use codex_do_sessions_client::ResolutionSource;
use codex_do_sessions_client::SessionStatus;
use codex_do_sessions_client::resolve_base_url;
use codex_do_sessions_client::resolve_token;
use futures::StreamExt;

#[derive(Debug, Parser)]
pub struct DoSessionsCli {
    /// Override the harness-api base URL. Falls back to $DO_HARNESS_BASE_URL,
    /// then the public DigitalOcean endpoint.
    #[arg(long = "base-url", global = true, value_name = "URL")]
    pub base_url: Option<String>,

    #[command(subcommand)]
    pub command: DoSessionsCommand,
}

#[derive(Debug, Subcommand)]
pub enum DoSessionsCommand {
    /// List sessions for your team.
    #[clap(visible_alias = "ls")]
    List(ListArgs),
    /// Create a new hosted agent session.
    Create(CreateArgs),
    /// Show a single session.
    Show(ShowArgs),
    /// Destroy a session and reclaim its sandbox.
    #[clap(visible_alias = "rm")]
    Destroy(DestroyArgs),
    /// Send a chat message into a session.
    Send(SendArgs),
    /// Resolve a pending HITL request.
    Resolve(ResolveArgs),
    /// Start the GitHub OAuth flow for a session.
    AuthGithub(AuthGithubArgs),
    /// Stream raw events from a session (read-only).
    Stream(StreamArgs),
    /// Attach an interactive chat to a session (stream + input + HITL).
    Attach(AttachArgs),
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Filter by session status (provisioning|ready|detached|destroying|destroyed|failed).
    #[arg(long = "status", value_name = "STATUS")]
    pub status: Option<String>,
    /// Maximum number of sessions to return.
    #[arg(long = "page-size", value_name = "N")]
    pub page_size: Option<i32>,
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// Path to an agents.yaml manifest. POSTed inline as application/x-yaml;
    /// `${VAR}` placeholders are expanded from the environment. When set, the
    /// flags below are ignored (the manifest is authoritative).
    #[arg(short = 'f', long = "manifest", value_name = "PATH")]
    pub manifest: Option<std::path::PathBuf>,
    /// Agent to run in the session (codex|claude-code|opencode|cursor|none|custom).
    /// Ignored when --manifest is given.
    #[arg(long = "agent", value_name = "AGENT")]
    pub agent: Option<String>,
    /// Repository hint (e.g. owner/name). Ignored when --manifest is given.
    #[arg(long = "repo", value_name = "OWNER/NAME")]
    pub repo: Option<String>,
    /// Inactivity timeout override in seconds (0 = tenant default).
    /// Ignored when --manifest is given.
    #[arg(long = "idle-timeout", value_name = "SECONDS")]
    pub idle_timeout_seconds: Option<i64>,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    pub session_id: String,
}

#[derive(Debug, Args)]
pub struct DestroyArgs {
    pub session_id: String,
}

#[derive(Debug, Args)]
pub struct SendArgs {
    pub session_id: String,
    /// Message text. Multiple words are joined with spaces.
    #[arg(required = true, num_args = 1.., value_name = "TEXT")]
    pub text: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ResolveArgs {
    pub session_id: String,
    pub request_id: String,
    /// Decision: approve|reject|defer.
    #[arg(value_name = "OUTCOME")]
    pub outcome: String,
    /// Optional free-text reason recorded in the audit trail.
    #[arg(long = "reason", value_name = "TEXT")]
    pub reason: Option<String>,
}

#[derive(Debug, Args)]
pub struct AuthGithubArgs {
    pub session_id: String,
}

#[derive(Debug, Args)]
pub struct StreamArgs {
    pub session_id: String,
    /// Replay events strictly after this event id before going live.
    #[arg(long = "replay-from", value_name = "EVENT_ID")]
    pub replay_from: Option<String>,
    /// Only replay history, then close the stream.
    #[arg(long = "replay-only", default_value_t = false)]
    pub replay_only: bool,
}

#[derive(Debug, Args)]
pub struct AttachArgs {
    pub session_id: String,
}

pub async fn run(cli: DoSessionsCli, arg0_paths: Arg0DispatchPaths) -> anyhow::Result<()> {
    let base_url = resolve_base_url(cli.base_url.as_deref())
        .context("resolving harness-api base URL")?;
    let token = resolve_token().context("resolving DO IAM token")?;

    match cli.command {
        // `attach` launches the native TUI against the DO session rather than
        // hitting the REST surface directly, so it needs the raw base_url/token.
        DoSessionsCommand::Attach(args) => attach(args, base_url, token, arg0_paths).await,
        command => {
            let client = DoSessionsClient::new(base_url, token)?;
            match command {
                DoSessionsCommand::List(args) => list(&client, args).await,
                DoSessionsCommand::Create(args) => create(&client, args).await,
                DoSessionsCommand::Show(args) => show(&client, args).await,
                DoSessionsCommand::Destroy(args) => destroy(&client, args).await,
                DoSessionsCommand::Send(args) => send(&client, args).await,
                DoSessionsCommand::Resolve(args) => resolve(&client, args).await,
                DoSessionsCommand::AuthGithub(args) => auth_github(&client, args).await,
                DoSessionsCommand::Stream(args) => stream(&client, args).await,
                DoSessionsCommand::Attach(_) => unreachable!("attach handled above"),
            }
        }
    }
}

async fn list(client: &DoSessionsClient, args: ListArgs) -> anyhow::Result<()> {
    let status = args.status.as_deref().map(parse_status).transpose()?;
    let page = client.list_sessions(status, None, args.page_size).await?;
    if page.sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    println!(
        "{:<28} {:<14} {:<12} {:<24} {}",
        "SESSION", "STATUS", "AGENT", "REPO", "CREATED"
    );
    for s in &page.sessions {
        println!(
            "{:<28} {:<14} {:<12} {:<24} {}",
            s.session_id,
            s.status.map(|s| s.label()).unwrap_or("-"),
            s.agent_kind.map(|a| a.label()).unwrap_or("-"),
            s.repo_hint.as_deref().unwrap_or("-"),
            s.created_at.as_deref().unwrap_or("-"),
        );
    }
    if !page.next_page_token.is_empty() {
        println!("\n(more results; next page token: {})", page.next_page_token);
    }
    Ok(())
}

async fn create(client: &DoSessionsClient, args: CreateArgs) -> anyhow::Result<()> {
    // With `-f`, POST the YAML manifest inline (Content-Type: application/x-yaml).
    // `${VAR}` placeholders are expanded from the environment first, so the
    // committed template can carry secrets via env without inlining them.
    let session = if let Some(path) = &args.manifest {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        let yaml = expand_env_vars(&raw);
        client.create_session_from_manifest(&yaml).await?
    } else {
        // Flag-driven create (no manifest): build the typed request.
        let agent_kind = match args.agent.as_deref() {
            Some(agent) => parse_agent(agent)?,
            None => AgentKind::CodexCli,
        };
        let req = codex_do_sessions_client::CreateSessionRequest {
            agent_kind,
            repo_hint: args.repo.unwrap_or_default(),
            idle_timeout_seconds: args.idle_timeout_seconds.unwrap_or(0),
        };
        client.create_session(&req).await?
    };
    println!("created session {}", session.session_id);
    println!("  status: {}", session.status.map(|s| s.label()).unwrap_or("-"));
    println!("  agent:  {}", session.agent_kind.map(|a| a.label()).unwrap_or("-"));
    if let Some(repo) = &session.repo_hint {
        if !repo.is_empty() {
            println!("  repo:   {repo}");
        }
    }
    println!(
        "\nwatch it come up:  codex do-sessions stream {}",
        session.session_id
    );
    Ok(())
}

async fn show(client: &DoSessionsClient, args: ShowArgs) -> anyhow::Result<()> {
    let s = client.get_session(&args.session_id).await?;
    println!("session:    {}", s.session_id);
    println!("status:     {}", s.status.map(|s| s.label()).unwrap_or("-"));
    println!("agent:      {}", s.agent_kind.map(|a| a.label()).unwrap_or("-"));
    println!("repo:       {}", s.repo_hint.as_deref().unwrap_or("-"));
    println!("sandbox:    {}", s.sandbox_id.as_deref().unwrap_or("-"));
    println!("created:    {}", s.created_at.as_deref().unwrap_or("-"));
    println!("last event: {}", s.last_event_at.as_deref().unwrap_or("-"));
    if !s.provider_auth.is_empty() {
        println!("provider auth:");
        for (provider, state) in &s.provider_auth {
            println!("  {provider}: {state:?}");
        }
    }
    Ok(())
}

async fn destroy(client: &DoSessionsClient, args: DestroyArgs) -> anyhow::Result<()> {
    client.destroy_session(&args.session_id).await?;
    println!("destroyed session {}", args.session_id);
    Ok(())
}

async fn send(client: &DoSessionsClient, args: SendArgs) -> anyhow::Result<()> {
    let text = args.text.join(" ");
    let run_id = client.send_input(&args.session_id, &text).await?;
    println!("sent (run_id: {run_id})");
    Ok(())
}

async fn resolve(client: &DoSessionsClient, args: ResolveArgs) -> anyhow::Result<()> {
    let outcome = parse_outcome(&args.outcome)?;
    client
        .resolve_hitl(
            &args.session_id,
            &args.request_id,
            outcome,
            args.reason.as_deref(),
            ResolutionSource::OutOfBand,
        )
        .await?;
    println!("resolved {} -> {:?}", args.request_id, outcome);
    Ok(())
}

async fn auth_github(client: &DoSessionsClient, args: AuthGithubArgs) -> anyhow::Result<()> {
    let resp = client
        .start_oauth(&args.session_id, OAuthProvider::Github, Vec::new())
        .await?;
    println!("open this URL in your browser to authorize GitHub:\n");
    println!("  {}", resp.authorize_url);
    println!("\nthe token is stored server-side; this terminal does not need the callback.");
    Ok(())
}

async fn stream(client: &DoSessionsClient, args: StreamArgs) -> anyhow::Result<()> {
    let mut events = Box::pin(
        client
            .stream_session(&args.session_id, args.replay_from.as_deref(), args.replay_only)
            .await?,
    );
    use std::io::Write;
    let mut stdout = std::io::stdout();
    while let Some(item) = events.next().await {
        match item {
            Ok(event) => {
                // Flush per event: streaming output is often redirected to a
                // file/pipe where stdout is block-buffered, so without this the
                // events are not visible until the buffer fills or the process
                // exits cleanly.
                let _ = writeln!(stdout, "{}", render_event(&event));
                let _ = stdout.flush();
            }
            Err(e) => {
                eprintln!("stream error: {e}");
                break;
            }
        }
    }
    Ok(())
}

/// Attach the native Codex TUI to a DO-hosted session. Instead of the bespoke
/// REPL, this boots the real TUI against the `DoSession` app-server transport so
/// streaming, diffs, reasoning, slash commands, and approval modals all work.
async fn attach(
    args: AttachArgs,
    base_url: String,
    token: String,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<()> {
    // The TUI owns argv parsing; we want a default invocation (no prompt, no
    // flags) since the session/model/cwd come from the DO session itself.
    let tui_cli = codex_tui::Cli::parse_from(["codex"]);
    let launch =
        codex_tui::AppServerLaunch::DoSession(codex_app_server_client::DoSessionConnectArgs {
            session_id: args.session_id,
            base_url,
            token,
        });

    // `App::run`'s async future is large on the remote-workspace entry path and
    // overflows the default 8 MB main-thread stack when reached via the
    // `do-sessions attach` call chain. Run the TUI on a dedicated thread with a
    // generous stack (and its own runtime) so it has headroom. This is isolated
    // to the attach path and does not affect the normal `codex` launch.
    let exit_info = std::thread::Builder::new()
        .name("codex-do-session-tui".to_string())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || -> anyhow::Result<codex_tui::AppExitInfo> {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|err| anyhow!("failed to build TUI runtime: {err}"))?;
            runtime
                .block_on(codex_tui::run_main(
                    tui_cli,
                    arg0_paths,
                    codex_config::LoaderOverrides::default(),
                    launch,
                ))
                .map_err(anyhow::Error::from)
        })
        .map_err(|err| anyhow!("failed to spawn TUI thread: {err}"))?
        .join()
        .map_err(|_| anyhow!("TUI thread panicked"))??;

    match exit_info.exit_reason {
        codex_tui::ExitReason::Fatal(message) => Err(anyhow!(message)),
        codex_tui::ExitReason::UserRequested => Ok(()),
    }
}

fn render_event(ev: &codex_do_sessions_client::Event) -> String {
    match ev.kind() {
        EventKind::TokenChunk => ev.text().unwrap_or_default().to_string(),
        EventKind::RunStarted => format!("● run started · {}", ev.run_id),
        EventKind::ToolCallStarted => format!("  tool · {}", ev.tool_name().unwrap_or("?")),
        EventKind::ToolCallCompleted => format!("  tool done · {}", ev.tool_name().unwrap_or("?")),
        EventKind::HitlRequested => {
            let id = ev.hitl_request_id().unwrap_or_default();
            let action = ev.hitl_action().unwrap_or("approval");
            match ev.hitl_command() {
                Some(cmd) => format!(
                    "⏸  approval requested · {action}\n     $ {cmd}\n     request {id}"
                ),
                None => format!("⏸  approval requested · {action} · request {id}"),
            }
        }
        EventKind::HitlResolved => "  ✓ HITL resolved".to_string(),
        EventKind::RunCompleted => "● run completed".to_string(),
        EventKind::RunFailed => {
            format!("✗ run failed: {}", ev.data_str("message").unwrap_or(""))
        }
        EventKind::RunPaused => "⏸  run paused".to_string(),
        EventKind::RunResumed => "▶ run resumed".to_string(),
        EventKind::SessionUpdated => format!("· session.updated {}", ev.data_compact()),
        // Show the raw type + data so unmapped events are still observable.
        EventKind::Unknown => format!("[{}] {}", ev.event_type, ev.data_compact()),
    }
}

/// Expand `${VAR}` placeholders from the environment (envsubst-style). Unset
/// variables expand to an empty string. Only the `${NAME}` form is handled.
fn expand_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                out.push_str(&std::env::var(name).unwrap_or_default());
                rest = &after[end + 1..];
            }
            None => {
                // No closing brace; emit the remainder verbatim.
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn parse_agent(s: &str) -> anyhow::Result<AgentKind> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "codex" | "codex-cli" => AgentKind::CodexCli,
        "claude" | "claude-code" => AgentKind::ClaudeCode,
        "opencode" => AgentKind::OpenCode,
        "cursor" | "cursor-cli" => AgentKind::CursorCli,
        "none" => AgentKind::None,
        "custom" => AgentKind::Custom,
        other => return Err(anyhow!("unknown agent kind `{other}`")),
    })
}

fn parse_status(s: &str) -> anyhow::Result<SessionStatus> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "provisioning" => SessionStatus::Provisioning,
        "ready" => SessionStatus::Ready,
        "detached" => SessionStatus::Detached,
        "destroying" => SessionStatus::Destroying,
        "destroyed" => SessionStatus::Destroyed,
        "failed" => SessionStatus::Failed,
        other => return Err(anyhow!("unknown status `{other}`")),
    })
}

fn parse_outcome(s: &str) -> anyhow::Result<HitlOutcome> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "approve" | "a" => HitlOutcome::Approve,
        "reject" | "r" => HitlOutcome::Reject,
        "defer" | "d" => HitlOutcome::Defer,
        other => return Err(anyhow!("unknown outcome `{other}` (approve|reject|defer)")),
    })
}
