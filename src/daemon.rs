use crate::{
    paths,
    protocol::{Envelope, Request, Response},
    provider,
    session::SessionRegistry,
};
use anyhow::{Context, Result, bail};
use std::{path::Path, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::Notify,
};

struct Runtime {
    sessions: SessionRegistry,
    shutdown: Notify,
}

impl Runtime {
    fn new(state_dir: &Path) -> Result<Self> {
        Ok(Self {
            sessions: SessionRegistry::new(state_dir)?,
            shutdown: Notify::new(),
        })
    }

    fn handle(&self, request: Request) -> Result<serde_json::Value> {
        match request {
            Request::Ping => Ok(serde_json::json!({
                "status": "ok",
                "pid": std::process::id(),
                "version": env!("CARGO_PKG_VERSION")
            })),
            Request::Providers => Ok(serde_json::to_value(provider::list())?),
            Request::Spawn(request) => Ok(serde_json::to_value(self.sessions.spawn(request)?)?),
            Request::List => Ok(serde_json::to_value(self.sessions.list())?),
            Request::Status { session } => {
                Ok(serde_json::to_value(self.sessions.get(&session)?.info())?)
            }
            Request::Send { session, message } => {
                let session = self.sessions.get(&session)?;
                session.send(&message)?;
                Ok(serde_json::json!({ "sent": true }))
            }
            Request::Output {
                session,
                after,
                limit,
            } => Ok(serde_json::to_value(
                self.sessions.get(&session)?.output(after, limit)?,
            )?),
            Request::Interrupt { session } => {
                self.sessions.get(&session)?.interrupt()?;
                Ok(serde_json::json!({ "interrupted": true }))
            }
            Request::Stop { session } => {
                let session = self.sessions.get(&session)?;
                session.stop()?;
                Ok(serde_json::to_value(session.info())?)
            }
            Request::Shutdown => {
                self.sessions.stop_all();
                self.shutdown.notify_waiters();
                Ok(serde_json::json!({ "shutting_down": true }))
            }
        }
    }
}

pub async fn run() -> Result<()> {
    let state_dir = paths::state_dir()?;
    let socket_path = paths::socket_path()?;
    prepare_socket(&socket_path).await?;

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind daemon socket {}", socket_path.display()))?;
    std::fs::write(paths::pid_path()?, std::process::id().to_string())?;

    let runtime = Arc::new(Runtime::new(&state_dir)?);
    loop {
        tokio::select! {
            _ = runtime.shutdown.notified() => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let runtime = Arc::clone(&runtime);
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(stream, runtime).await {
                        eprintln!("agentmux daemon request failed: {error:#}");
                    }
                });
            }
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(paths::pid_path()?);
    Ok(())
}

async fn handle_connection(stream: UnixStream, runtime: Arc<Runtime>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    BufReader::new(reader).read_line(&mut line).await?;

    let response = match serde_json::from_str::<Envelope>(&line) {
        Ok(envelope) => match runtime.handle(envelope.request) {
            Ok(result) => Response::success(envelope.id, result),
            Err(error) => Response::failure(envelope.id, format!("{error:#}")),
        },
        Err(error) => Response::failure(0, format!("invalid request: {error}")),
    };
    let mut encoded = serde_json::to_vec(&response)?;
    encoded.push(b'\n');
    writer.write_all(&encoded).await?;
    Ok(())
}

async fn prepare_socket(socket_path: &Path) -> Result<()> {
    if !socket_path.exists() {
        return Ok(());
    }

    if UnixStream::connect(socket_path).await.is_ok() {
        bail!("an agentmux daemon is already running");
    }
    std::fs::remove_file(socket_path)
        .with_context(|| format!("remove stale socket {}", socket_path.display()))?;
    Ok(())
}
