use crate::{
    client,
    protocol::{
        DEFAULT_OUTPUT_LIMIT, Request, SafetyProfile, SpawnMode, SpawnRequest, TerminalKey,
    },
};
use anyhow::Result;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SessionParam {
    #[schemars(description = "Session name or ID")]
    session: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SpawnParams {
    #[schemars(description = "Unique session name")]
    name: String,
    #[serde(default = "default_provider")]
    #[schemars(
        description = "Provider preset: shell, claude, codex, grok, kimi, antigravity, gemini, or a custom label"
    )]
    provider: String,
    #[schemars(
        description = "Absolute working directory; defaults to the MCP server working directory"
    )]
    cwd: Option<String>,
    #[schemars(
        description = "Initial instruction. Built-in providers run it headlessly by default; use mode=interactive for a steerable TUI session"
    )]
    prompt: Option<String>,
    #[serde(default)]
    #[schemars(description = "Launch mode: auto, interactive, or headless")]
    mode: SpawnMode,
    #[serde(default)]
    #[schemars(
        description = "Safety profile: read_only, workspace_write, or provider_default. Kimi headless prompt mode requires provider_default; use interactive read_only for Kimi plan mode"
    )]
    safety: SafetyProfile,
    #[serde(default)]
    #[schemars(description = "Explicit command and arguments; required for unknown providers")]
    command: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SendParams {
    #[schemars(description = "Session name or ID")]
    session: String,
    #[schemars(description = "Follow-up instruction to send to the running session")]
    message: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct KeyParams {
    #[schemars(description = "Session name or ID")]
    session: String,
    #[schemars(description = "Terminal key: enter, escape, ctrl_c, up, or down")]
    key: TerminalKey,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct InputParams {
    #[schemars(description = "Session name or ID")]
    session: String,
    #[schemars(description = "Raw PTY bytes encoded as standard base64")]
    data_base64: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OutputParams {
    #[schemars(description = "Session name or ID")]
    session: String,
    #[serde(default)]
    #[schemars(description = "Read after this byte cursor")]
    after: u64,
    #[serde(default = "default_output_limit")]
    #[schemars(description = "Maximum bytes to return, capped at 1 MiB")]
    limit: usize,
}

#[derive(Debug, Clone)]
struct AgentmuxMcp {
    tool_router: ToolRouter<Self>,
}

impl AgentmuxMcp {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl AgentmuxMcp {
    #[tool(description = "List provider presets and detect which agent commands are installed")]
    async fn agents_providers(&self) -> Result<String, String> {
        call(Request::Providers).await
    }

    #[tool(description = "Spawn a persistent PTY-backed agent session")]
    async fn agents_spawn(
        &self,
        Parameters(params): Parameters<SpawnParams>,
    ) -> Result<String, String> {
        let cwd = match params.cwd {
            Some(cwd) => PathBuf::from(cwd),
            None => std::env::current_dir().map_err(|error| error.to_string())?,
        };
        let cwd = cwd
            .canonicalize()
            .map_err(|error| format!("resolve working directory: {error}"))?;
        call(Request::Spawn(SpawnRequest {
            name: params.name,
            provider: params.provider,
            cwd: cwd.to_string_lossy().into_owned(),
            prompt: params.prompt,
            command: params.command,
            mode: params.mode,
            safety: params.safety,
        }))
        .await
    }

    #[tool(description = "List all sessions known to the running daemon")]
    async fn agents_list(&self) -> Result<String, String> {
        call(Request::List).await
    }

    #[tool(description = "Read status and metadata for one session")]
    async fn agents_status(
        &self,
        Parameters(SessionParam { session }): Parameters<SessionParam>,
    ) -> Result<String, String> {
        call(Request::Status { session }).await
    }

    #[tool(description = "Send a corrective or follow-up message to a running session")]
    async fn agents_send(
        &self,
        Parameters(SendParams { session, message }): Parameters<SendParams>,
    ) -> Result<String, String> {
        call(Request::Send { session, message }).await
    }

    #[tool(description = "Send one terminal control key to an interactive session")]
    async fn agents_key(
        &self,
        Parameters(KeyParams { session, key }): Parameters<KeyParams>,
    ) -> Result<String, String> {
        call(Request::Key { session, key }).await
    }

    #[tool(description = "Write standard-base64-encoded raw bytes to an interactive session PTY")]
    async fn agents_input(
        &self,
        Parameters(InputParams {
            session,
            data_base64,
        }): Parameters<InputParams>,
    ) -> Result<String, String> {
        call(Request::Input {
            session,
            data_base64,
        })
        .await
    }

    #[tool(description = "Read incremental terminal output using a byte cursor")]
    async fn agents_output(
        &self,
        Parameters(OutputParams {
            session,
            after,
            limit,
        }): Parameters<OutputParams>,
    ) -> Result<String, String> {
        call(Request::Output {
            session,
            after,
            limit,
        })
        .await
    }

    #[tool(
        description = "Deliver Ctrl-C to a running session; delivery does not imply that the turn was cancelled"
    )]
    async fn agents_interrupt(
        &self,
        Parameters(SessionParam { session }): Parameters<SessionParam>,
    ) -> Result<String, String> {
        call(Request::Interrupt { session }).await
    }

    #[tool(description = "Terminate a session process")]
    async fn agents_stop(
        &self,
        Parameters(SessionParam { session }): Parameters<SessionParam>,
    ) -> Result<String, String> {
        call(Request::Stop { session }).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AgentmuxMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "Supervise persistent terminal-native agent sessions. Read output incrementally, verify real workspace state, and steer the same session when it deviates.",
            )
            .with_server_info(Implementation::new("agentmux", env!("CARGO_PKG_VERSION")))
    }
}

pub async fn serve() -> Result<()> {
    client::ensure_daemon().await?;
    AgentmuxMcp::new()
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

async fn call(request: Request) -> Result<String, String> {
    client::ensure_daemon()
        .await
        .map_err(|error| format!("start daemon: {error:#}"))?;
    let value = client::call(request)
        .await
        .map_err(|error| format!("agentmux request failed: {error:#}"))?;
    serde_json::to_string_pretty(&value).map_err(|error| error.to_string())
}

fn default_provider() -> String {
    "shell".to_string()
}

fn default_output_limit() -> usize {
    DEFAULT_OUTPUT_LIMIT
}
