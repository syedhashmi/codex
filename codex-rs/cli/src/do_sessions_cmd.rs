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
use owo_colors::OwoColorize;

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
    /// Backfill events strictly after this event id before going live.
    #[arg(long = "replay-from", value_name = "EVENT_ID")]
    pub replay_from: Option<String>,
}

pub async fn run(cli: DoSessionsCli) -> anyhow::Result<()> {
    let base_url = resolve_base_url(cli.base_url.as_deref())
        .context("resolving harness-api base URL")?;
    let token = resolve_token().context("resolving DO IAM token")?;
    let client = DoSessionsClient::new(base_url, token)?;

    match cli.command {
        DoSessionsCommand::List(args) => list(&client, args).await,
        DoSessionsCommand::Create(args) => create(&client, args).await,
        DoSessionsCommand::Show(args) => show(&client, args).await,
        DoSessionsCommand::Destroy(args) => destroy(&client, args).await,
        DoSessionsCommand::Send(args) => send(&client, args).await,
        DoSessionsCommand::Resolve(args) => resolve(&client, args).await,
        DoSessionsCommand::AuthGithub(args) => auth_github(&client, args).await,
        DoSessionsCommand::Stream(args) => stream(&client, args).await,
        DoSessionsCommand::Attach(args) => attach(&client, args).await,
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

async fn attach(client: &DoSessionsClient, args: AttachArgs) -> anyhow::Result<()> {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tokio::io::AsyncBufReadExt;
    use tokio::io::BufReader;

    let sid = args.session_id.clone();
    let session = client.get_session(&sid).await.ok();
    print_banner(&sid, session.as_ref(), client.base_url());

    // request_id of any pending HITL the user must answer.
    let pending: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Event stream task: owns a cloned client so the stream is 'static.
    let client_stream = client.clone();
    let sid_stream = sid.clone();
    let replay_from = args.replay_from.clone();
    let pending_stream = pending.clone();
    let stream_task = tokio::spawn(async move {
        let stream = match client_stream
            .stream_session(&sid_stream, replay_from.as_deref(), false)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("stream connect failed: {e}");
                return;
            }
        };
        let mut stream = Box::pin(stream);
        let mut out = std::io::stdout();
        // Whether we've printed the "codex ▸" label for the active turn so
        // streamed tokens flow under one labeled block.
        let mut agent_labeled = false;
        // `dirty` = agent produced output since the last prompt. An inactivity
        // timer reprints the prompt once the stream goes quiet, so we recover
        // the prompt even when a turn ends without a `run.completed` we can see
        // (e.g. multi-turn idle markers arrive as suppressed `run.log`s).
        let mut dirty = false;
        let quiet = std::time::Duration::from_millis(700);
        let idle = tokio::time::sleep(quiet);
        tokio::pin!(idle);
        loop {
            tokio::select! {
                maybe = stream.next() => {
                    let Some(item) = maybe else { break; };
                    let ev = match item {
                        Ok(ev) => ev,
                        Err(e) => {
                            let _ = writeln!(out, "\n{} {e}", "stream error:".red());
                            break;
                        }
                    };
                    idle.as_mut().reset(tokio::time::Instant::now() + quiet);
                    match ev.kind() {
                        EventKind::RunStarted => {
                            agent_labeled = false;
                        }
                        EventKind::TokenChunk => {
                            if let Some(t) = ev.text() {
                                if !agent_labeled {
                                    let _ = write!(out, "\n{} ", "codex ▸".green().bold());
                                    agent_labeled = true;
                                }
                                let _ = write!(out, "{t}");
                                let _ = out.flush();
                                dirty = true;
                            }
                        }
                        EventKind::ToolCallStarted => {
                            let _ = write!(
                                out,
                                "\n  {} {}",
                                "⚙".dimmed(),
                                ev.tool_name().unwrap_or("tool").dimmed()
                            );
                            agent_labeled = false;
                            let _ = out.flush();
                            dirty = true;
                        }
                        EventKind::ToolCallCompleted => {
                            let _ = writeln!(out, " {}", "done".dimmed());
                            agent_labeled = false;
                            let _ = out.flush();
                            dirty = true;
                        }
                        EventKind::HitlRequested => {
                            if let Some(req_id) = ev.hitl_request_id() {
                                *pending_stream.lock().unwrap() = Some(req_id);
                            }
                            // Bare notice first, detailed payload second; prompt
                            // only once we have the command details.
                            if ev.hitl_payload().is_some() {
                                let action = ev.hitl_action().unwrap_or("approval");
                                let _ = writeln!(
                                    out,
                                    "\n{}  {}",
                                    "⏸ approval needed".yellow().bold(),
                                    action.dimmed()
                                );
                                if let Some(cmd) = ev.hitl_command() {
                                    let _ = writeln!(out, "    {} {}", "$".dimmed(), cmd);
                                }
                                let _ = writeln!(
                                    out,
                                    "    {}    {}    {}",
                                    " a ⏵ approve ".black().on_green().bold(),
                                    " r ⏵ reject ".white().on_red().bold(),
                                    " d ⏵ defer ".black().on_yellow().bold(),
                                );
                                agent_labeled = false;
                                print_prompt(&mut out);
                                dirty = false;
                            }
                        }
                        EventKind::HitlResolved => {
                            *pending_stream.lock().unwrap() = None;
                            agent_labeled = false;
                        }
                        EventKind::RunCompleted => {
                            *pending_stream.lock().unwrap() = None;
                            agent_labeled = false;
                            let _ = writeln!(out);
                            print_prompt(&mut out);
                            dirty = false;
                        }
                        EventKind::RunFailed => {
                            *pending_stream.lock().unwrap() = None;
                            agent_labeled = false;
                            let msg = ev.data_str("message").unwrap_or("run failed");
                            let _ = writeln!(out, "\n{} {}", "✗".red().bold(), msg.red());
                            print_prompt(&mut out);
                            dirty = false;
                        }
                        // session.updated / logs / unknown: suppressed in the
                        // chat view; the inactivity timer handles the prompt.
                        _ => {}
                    }
                }
                _ = &mut idle, if dirty => {
                    // Output settled and no terminal event arrived; reprint the
                    // prompt so the user is never left without one. Skip while a
                    // HITL approval is pending (its own prompt is already shown).
                    if pending_stream.lock().unwrap().is_none() {
                        let _ = writeln!(out);
                        print_prompt(&mut out);
                    }
                    dirty = false;
                }
            }
        }
    });

    // Initial prompt; subsequent prompts are reprinted by the stream task after
    // each turn completes / approval is requested.
    print_prompt(&mut std::io::stdout());

    // Input loop on stdin.
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let Some(line) = lines.next_line().await? else {
            break; // EOF / Ctrl-D
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "/exit" || trimmed == "/quit" {
            break;
        }

        // If a HITL is pending and the line is a decision key, resolve it.
        let pend = pending.lock().unwrap().clone();
        if let Some(req_id) = pend {
            if let Some(outcome) = hitl_key(trimmed) {
                client
                    .resolve_hitl(&sid, &req_id, outcome, None, ResolutionSource::InlineKeystroke)
                    .await?;
                *pending.lock().unwrap() = None;
                println!("  {}", format!("→ {}", outcome_label(outcome)).dimmed());
                continue;
            }
        }

        // Otherwise send it as a chat message.
        if let Err(e) = client.send_input(&sid, trimmed).await {
            eprintln!("  {} {e}", "send failed:".red());
        }
    }

    stream_task.abort();
    println!("\n{}", format!("detached from {sid} (still running)").dimmed());
    Ok(())
}

fn outcome_label(o: HitlOutcome) -> &'static str {
    match o {
        HitlOutcome::Approve => "approved",
        HitlOutcome::Reject => "rejected",
        HitlOutcome::Defer => "deferred",
        HitlOutcome::Unspecified => "unspecified",
    }
}

/// Print the `you ▸` input prompt (no trailing newline).
fn print_prompt<W: std::io::Write>(out: &mut W) {
    let _ = write!(out, "\n{} ", "you ▸".cyan().bold());
    let _ = out.flush();
}

/// ASCII-art wordmark shown at the top of the attach banner.
const CODEX_ART: [&str; 5] = [
    r"  ____  ___  ____  _____ __  __",
    r" / ___|/ _ \|  _ \| ____|\ \/ /",
    r"| |   | | | | | | |  _|   \  / ",
    r"| |___| |_| | |_| | |___  /  \ ",
    r" \____|\___/|____/|_____|/_/\_\",
];

/// Print a banner with an ASCII-art wordmark plus session + endpoint details.
fn print_banner(
    sid: &str,
    session: Option<&codex_do_sessions_client::Session>,
    base_url: &str,
) {
    let agent = session
        .and_then(|s| s.agent_kind)
        .map(|a| a.label())
        .unwrap_or("agent");
    let status = session
        .and_then(|s| s.status)
        .map(|s| s.label())
        .unwrap_or("?");
    let host = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(base_url);

    println!();
    for line in CODEX_ART {
        println!("{}", line.cyan().bold());
    }
    println!("{}", "  on DigitalOcean · hosted agent".dimmed());
    println!();
    println!("  {}  {}", "session ".dimmed(), sid);
    println!(
        "  {}  {} {} {}",
        "agent   ".dimmed(),
        agent,
        "·".dimmed(),
        status
    );
    println!("  {}  {}", "endpoint".dimmed(), host.dimmed());
    println!();
    println!(
        "{}",
        "  type to chat · a/r/d resolves approvals · /exit detaches".dimmed()
    );
}

fn hitl_key(s: &str) -> Option<HitlOutcome> {
    match s {
        "a" | "approve" => Some(HitlOutcome::Approve),
        "r" | "reject" => Some(HitlOutcome::Reject),
        "d" | "defer" => Some(HitlOutcome::Defer),
        _ => None,
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
