use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DEFAULT_OUTPUT_LIMIT: usize = 64 * 1024;
pub const MAX_OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Providers,
    Spawn(SpawnRequest),
    List,
    Status {
        session: String,
    },
    Send {
        session: String,
        message: String,
    },
    Key {
        session: String,
        key: TerminalKey,
    },
    Input {
        session: String,
        data_base64: String,
    },
    Output {
        session: String,
        #[serde(default)]
        after: u64,
        #[serde(default = "default_output_limit")]
        limit: usize,
        #[serde(default)]
        raw: bool,
    },
    Wait {
        session: String,
        #[serde(default)]
        after: u64,
        #[serde(default = "default_output_limit")]
        limit: usize,
        #[serde(default = "default_wait_timeout_ms")]
        timeout_ms: u64,
        #[serde(default)]
        raw: bool,
    },
    Interrupt {
        session: String,
    },
    Stop {
        session: String,
    },
    Delete {
        session: String,
    },
    Prune {
        older_than_ms: u64,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnRequest {
    pub name: String,
    pub provider: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub mode: SpawnMode,
    #[serde(default)]
    pub safety: SafetyProfile,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    clap::ValueEnum,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum SpawnMode {
    #[default]
    Auto,
    Interactive,
    Headless,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    clap::ValueEnum,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum SafetyProfile {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    ProviderDefault,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum TerminalKey {
    Enter,
    Escape,
    CtrlC,
    Up,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn success(id: u64, result: impl Serialize) -> Self {
        match serde_json::to_value(result) {
            Ok(result) => Self {
                id,
                ok: true,
                result: Some(result),
                error: None,
            },
            Err(error) => Self::failure(id, error),
        }
    }

    pub fn failure(id: u64, error: impl std::fmt::Display) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error.to_string()),
        }
    }

    pub fn into_result(self) -> anyhow::Result<Value> {
        if self.ok {
            Ok(self.result.unwrap_or(Value::Null))
        } else {
            anyhow::bail!(self.error.unwrap_or_else(|| "unknown daemon error".into()))
        }
    }
}

fn default_output_limit() -> usize {
    DEFAULT_OUTPUT_LIMIT
}

fn default_wait_timeout_ms() -> u64 {
    10_000
}
