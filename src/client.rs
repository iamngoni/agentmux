use crate::{
    paths,
    protocol::{Envelope, Request, Response},
};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    error::Error,
    fmt,
    fs::OpenOptions,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::Mutex,
    time::{sleep, timeout},
};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_millis(1_000);
const STOP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(4);
const SHUTDOWN_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const PROVIDERS_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const COLD_START_ATTEMPTS: usize = 160;
const RECOVERY_START_ATTEMPTS: usize = 40;
const START_RETRY_DELAY: Duration = Duration::from_millis(25);
const SHUTDOWN_ATTEMPTS: usize = 100;
const SHUTDOWN_RETRY_DELAY: Duration = Duration::from_millis(50);

static DAEMON_START_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonErrorCode {
    Unavailable,
    Timeout,
    ConnectionLost,
}

impl DaemonErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "daemon_unavailable",
            Self::Timeout => "daemon_timeout",
            Self::ConnectionLost => "connection_lost",
        }
    }
}

#[derive(Debug)]
pub struct DaemonClientError {
    code: DaemonErrorCode,
    message: String,
}

impl DaemonClientError {
    fn new(code: DaemonErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> DaemonErrorCode {
        self.code
    }
}

impl fmt::Display for DaemonClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code.as_str(), self.message)
    }
}

impl Error for DaemonClientError {}

pub async fn call(request: Request) -> Result<Value> {
    call_raw(request).await?.into_result()
}

pub async fn call_resilient(request: Request) -> Result<Value> {
    match call(request.clone()).await {
        Ok(value) => Ok(value),
        Err(error) if should_reconnect(&request, &error) => {
            ensure_daemon_with_attempts(RECOVERY_START_ATTEMPTS).await?;
            call(request).await
        }
        Err(error) => Err(error),
    }
}

pub async fn call_raw(request: Request) -> Result<Response> {
    let response_timeout = response_timeout(&request);
    let socket = paths::socket_path()?;
    let mut stream = match timeout(CONNECT_TIMEOUT, UnixStream::connect(&socket)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            return Err(DaemonClientError::new(
                DaemonErrorCode::Unavailable,
                format!("connect to daemon at {}: {error}", socket.display()),
            )
            .into());
        }
        Err(_) => {
            return Err(DaemonClientError::new(
                DaemonErrorCode::Timeout,
                format!(
                    "connect to daemon at {} exceeded {} ms",
                    socket.display(),
                    CONNECT_TIMEOUT.as_millis()
                ),
            )
            .into());
        }
    };

    let envelope = Envelope { id: 1, request };
    let mut encoded = serde_json::to_vec(&envelope)?;
    encoded.push(b'\n');
    match timeout(DEFAULT_RESPONSE_TIMEOUT, stream.write_all(&encoded)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            return Err(DaemonClientError::new(
                DaemonErrorCode::ConnectionLost,
                format!("write daemon request: {error}"),
            )
            .into());
        }
        Err(_) => {
            return Err(DaemonClientError::new(
                DaemonErrorCode::Timeout,
                "write daemon request exceeded 1000 ms",
            )
            .into());
        }
    }

    let mut line = String::new();
    match timeout(
        response_timeout,
        BufReader::new(stream).read_line(&mut line),
    )
    .await
    {
        Ok(Ok(0)) => {
            return Err(DaemonClientError::new(
                DaemonErrorCode::ConnectionLost,
                "daemon closed the connection without a response",
            )
            .into());
        }
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            return Err(DaemonClientError::new(
                DaemonErrorCode::ConnectionLost,
                format!("read daemon response: {error}"),
            )
            .into());
        }
        Err(_) => {
            return Err(DaemonClientError::new(
                DaemonErrorCode::Timeout,
                format!(
                    "daemon response exceeded {} ms",
                    response_timeout.as_millis()
                ),
            )
            .into());
        }
    }
    serde_json::from_str(&line).context("decode daemon response")
}

pub async fn ensure_daemon() -> Result<bool> {
    ensure_daemon_with_attempts(COLD_START_ATTEMPTS).await
}

async fn ensure_daemon_with_attempts(start_attempts: usize) -> Result<bool> {
    if call(Request::Ping).await.is_ok() {
        return Ok(false);
    }

    let _guard = DAEMON_START_LOCK.lock().await;
    match call(Request::Ping).await {
        Ok(_) => return Ok(false),
        Err(error) if error_code(&error) == Some(DaemonErrorCode::Timeout) => {
            return Err(DaemonClientError::new(
                DaemonErrorCode::Timeout,
                "existing daemon did not answer its health probe; refusing to start a competing daemon",
            )
            .into());
        }
        Err(_) => {}
    }

    start_daemon_process()?;
    for _ in 0..start_attempts {
        sleep(START_RETRY_DELAY).await;
        if call(Request::Ping).await.is_ok() {
            return Ok(true);
        }
    }

    let log_path = paths::daemon_log_path()?;
    Err(DaemonClientError::new(
        DaemonErrorCode::Timeout,
        format!(
            "daemon did not become ready within {} ms; inspect {}",
            (start_attempts as u128) * START_RETRY_DELAY.as_millis(),
            log_path.display()
        ),
    )
    .into())
}

fn start_daemon_process() -> Result<()> {
    let executable = std::env::current_exe().context("locate agentmux executable")?;
    let log_path = paths::daemon_log_path()?;
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let stderr = stdout.try_clone()?;

    let mut command = Command::new(executable);
    command
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().context("start agentmux daemon")?;
    Ok(())
}

pub async fn stop_daemon() -> Result<()> {
    call(Request::Shutdown).await?;
    let pid_path = paths::pid_path()?;
    for _ in 0..SHUTDOWN_ATTEMPTS {
        if !pid_path.exists() {
            return Ok(());
        }
        sleep(SHUTDOWN_RETRY_DELAY).await;
    }
    bail!(
        "daemon_shutdown_timeout: daemon did not exit within {} ms; read the exact PID from {} and terminate only that process",
        (SHUTDOWN_ATTEMPTS as u128) * SHUTDOWN_RETRY_DELAY.as_millis(),
        pid_path.display()
    )
}

fn response_timeout(request: &Request) -> Duration {
    if let Some(wait_ms) = request.wait_timeout_ms() {
        return Duration::from_millis(wait_ms.saturating_add(1_000));
    }
    match request {
        Request::Providers => PROVIDERS_RESPONSE_TIMEOUT,
        Request::Stop { .. } => STOP_RESPONSE_TIMEOUT,
        Request::Shutdown => SHUTDOWN_RESPONSE_TIMEOUT,
        _ => DEFAULT_RESPONSE_TIMEOUT,
    }
}

fn should_reconnect(request: &Request, error: &anyhow::Error) -> bool {
    match error_code(error) {
        None => false,
        Some(DaemonErrorCode::Unavailable) => true,
        Some(DaemonErrorCode::ConnectionLost) => request.is_read_only(),
        Some(DaemonErrorCode::Timeout) => false,
    }
}

fn error_code(error: &anyhow::Error) -> Option<DaemonErrorCode> {
    error
        .downcast_ref::<DaemonClientError>()
        .map(DaemonClientError::code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_errors_have_stable_machine_codes() {
        let error = DaemonClientError::new(DaemonErrorCode::Timeout, "status read timed out");
        assert_eq!(error.code(), DaemonErrorCode::Timeout);
        assert!(error.to_string().starts_with("daemon_timeout:"));
    }

    #[test]
    fn waits_receive_their_requested_window_plus_transport_margin() {
        let request = Request::Wait {
            session: "test".to_string(),
            after: 0,
            limit: 1,
            timeout_ms: 30_000,
            raw: false,
        };
        assert_eq!(response_timeout(&request), Duration::from_secs(31));
    }

    #[test]
    fn ambiguous_mutations_are_not_replayed_after_connection_loss() {
        let error: anyhow::Error = DaemonClientError::new(
            DaemonErrorCode::ConnectionLost,
            "daemon exited after request delivery",
        )
        .into();
        assert!(should_reconnect(
            &Request::Status {
                session: "test".to_string()
            },
            &error
        ));
        assert!(!should_reconnect(
            &Request::Delete {
                session: "test".to_string()
            },
            &error
        ));
    }
}
