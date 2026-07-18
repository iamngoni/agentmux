use crate::{
    paths,
    protocol::{Envelope, Request, Response},
};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    fs::OpenOptions,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::sleep,
};

pub async fn call(request: Request) -> Result<Value> {
    call_raw(request).await?.into_result()
}

pub async fn call_raw(request: Request) -> Result<Response> {
    let socket = paths::socket_path()?;
    let mut stream = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("connect to daemon at {}", socket.display()))?;
    let envelope = Envelope { id: 1, request };
    let mut encoded = serde_json::to_vec(&envelope)?;
    encoded.push(b'\n');
    stream.write_all(&encoded).await?;

    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    if line.is_empty() {
        bail!("daemon closed the connection without a response");
    }
    serde_json::from_str(&line).context("decode daemon response")
}

pub async fn ensure_daemon() -> Result<bool> {
    if call(Request::Ping).await.is_ok() {
        return Ok(false);
    }

    start_daemon_process()?;
    for _ in 0..80 {
        sleep(Duration::from_millis(50)).await;
        if call(Request::Ping).await.is_ok() {
            return Ok(true);
        }
    }

    let log_path = paths::daemon_log_path()?;
    bail!(
        "daemon did not become ready; inspect {}",
        log_path.display()
    )
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
    for _ in 0..40 {
        if call(Request::Ping).await.is_err() {
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
    bail!("daemon did not stop")
}
