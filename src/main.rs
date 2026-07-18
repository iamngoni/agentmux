mod client;
mod daemon;
mod mcp;
mod paths;
mod protocol;
mod provider;
mod session;

use crate::protocol::{
    DEFAULT_OUTPUT_LIMIT, Request, SafetyProfile, SpawnMode, SpawnRequest, TerminalKey,
};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "agentmux",
    version,
    about = "Persistent, observable, steerable sessions for AI agents"
)]
struct Cli {
    #[arg(long, global = true, help = "Print machine-readable JSON")]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage the local session daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Show built-in provider presets and whether their commands are installed.
    Providers,
    /// Start an agent session (prompted built-ins default to safe headless mode).
    Spawn {
        name: String,
        #[arg(long, default_value = "shell")]
        provider: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long, value_enum, default_value_t = SpawnMode::Auto)]
        mode: SpawnMode,
        #[arg(
            long,
            value_enum,
            default_value_t = SafetyProfile::ReadOnly,
            help = "Headless safety profile; Kimi prompt mode requires provider_default"
        )]
        safety: SafetyProfile,
        #[arg(last = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// List known sessions.
    List,
    /// Show one session's state.
    Status { session: String },
    /// Send a follow-up message to a running session.
    Send { session: String, message: String },
    /// Send a terminal key without semantic message handling.
    Key {
        session: String,
        #[arg(value_enum)]
        key: TerminalKey,
    },
    /// Write base64-encoded bytes directly to the session PTY.
    Input {
        session: String,
        data_base64: String,
    },
    /// Read terminal output after a byte cursor.
    Output {
        session: String,
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, default_value_t = DEFAULT_OUTPUT_LIMIT)]
        limit: usize,
        #[arg(
            long,
            help = "Return redacted raw PTY bytes instead of normalized text"
        )]
        raw: bool,
    },
    /// Wait for new output or a terminal lifecycle state.
    Wait {
        session: String,
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, default_value_t = DEFAULT_OUTPUT_LIMIT)]
        limit: usize,
        #[arg(long, default_value_t = 10_000)]
        timeout_ms: u64,
        #[arg(
            long,
            help = "Return redacted raw PTY bytes instead of normalized text"
        )]
        raw: bool,
    },
    /// Send Ctrl-C to a running session.
    Interrupt { session: String },
    /// Terminate a session process.
    Stop { session: String },
    /// Delete a finished or orphaned session and its retained artifacts.
    Delete { session: String },
    /// Delete terminal sessions older than the given duration.
    Prune {
        #[arg(long)]
        older_than_ms: u64,
    },
    /// Serve Agentmux tools over MCP stdio.
    Mcp,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Start the daemon in the background if it is not running.
    Start,
    /// Run the daemon in the foreground.
    Run,
    /// Show daemon health.
    Status,
    /// Stop the daemon and all child sessions.
    Stop,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon { command } => handle_daemon(command, cli.json).await,
        Command::Mcp => mcp::serve().await,
        command => {
            client::ensure_daemon().await?;
            let value = handle_command(command).await?;
            print_value(value, cli.json)
        }
    }
}

async fn handle_daemon(command: DaemonCommand, json: bool) -> Result<()> {
    match command {
        DaemonCommand::Start => {
            let started = client::ensure_daemon().await?;
            print_value(
                serde_json::json!({
                    "status": "running",
                    "started": started,
                    "socket": paths::socket_path()?
                }),
                json,
            )
        }
        DaemonCommand::Run => daemon::run().await,
        DaemonCommand::Status => {
            let value = client::call(Request::Ping)
                .await
                .context("agentmux daemon is not running")?;
            print_value(value, json)
        }
        DaemonCommand::Stop => {
            client::stop_daemon().await?;
            print_value(serde_json::json!({ "status": "stopped" }), json)
        }
    }
}

async fn handle_command(command: Command) -> Result<Value> {
    let request = match command {
        Command::Providers => Request::Providers,
        Command::Spawn {
            name,
            provider,
            cwd,
            prompt,
            mode,
            safety,
            command,
        } => Request::Spawn(SpawnRequest {
            name,
            provider,
            cwd: cwd
                .unwrap_or(std::env::current_dir()?)
                .canonicalize()?
                .to_string_lossy()
                .into_owned(),
            prompt,
            command,
            mode,
            safety,
        }),
        Command::List => Request::List,
        Command::Status { session } => Request::Status { session },
        Command::Send { session, message } => Request::Send { session, message },
        Command::Key { session, key } => Request::Key { session, key },
        Command::Input {
            session,
            data_base64,
        } => Request::Input {
            session,
            data_base64,
        },
        Command::Output {
            session,
            after,
            limit,
            raw,
        } => Request::Output {
            session,
            after,
            limit,
            raw,
        },
        Command::Wait {
            session,
            after,
            limit,
            timeout_ms,
            raw,
        } => Request::Wait {
            session,
            after,
            limit,
            timeout_ms,
            raw,
        },
        Command::Interrupt { session } => Request::Interrupt { session },
        Command::Stop { session } => Request::Stop { session },
        Command::Delete { session } => Request::Delete { session },
        Command::Prune { older_than_ms } => Request::Prune { older_than_ms },
        Command::Daemon { .. } | Command::Mcp => unreachable!(),
    };
    client::call(request).await
}

fn print_value(value: Value, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if let Some(text) = value.as_str() {
        println!("{text}");
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}
