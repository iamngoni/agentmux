use crate::{
    paths,
    protocol::{Envelope, Request, Response},
    provider,
    session::SessionRegistry,
};
use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Notify, Semaphore},
    time::timeout,
};

const MAX_BLOCKING_REQUESTS: usize = 64;
const REQUEST_QUEUE_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(1_500);
const CONNECTION_IO_TIMEOUT: Duration = Duration::from_secs(2);

struct Runtime {
    sessions: Arc<SessionRegistry>,
    shutdown: Notify,
    request_slots: Arc<Semaphore>,
}

impl Runtime {
    fn new(state_dir: &Path) -> Result<Self> {
        Ok(Self {
            sessions: Arc::new(SessionRegistry::new(state_dir)?),
            shutdown: Notify::new(),
            request_slots: Arc::new(Semaphore::new(MAX_BLOCKING_REQUESTS)),
        })
    }

    async fn handle(&self, request: Request) -> Result<serde_json::Value> {
        match request {
            Request::Ping => {
                return Ok(serde_json::json!({
                    "status": "ok",
                    "pid": std::process::id(),
                    "version": env!("CARGO_PKG_VERSION")
                }));
            }
            Request::Shutdown => {
                return Ok(serde_json::json!({ "shutting_down": true }));
            }
            _ => {}
        }

        let operation_timeout = request_timeout(&request);
        let permit = timeout(
            REQUEST_QUEUE_TIMEOUT,
            Arc::clone(&self.request_slots).acquire_owned(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("daemon_busy: request queue exceeded 1000 ms"))?
        .context("daemon request semaphore closed")?;
        let sessions = Arc::clone(&self.sessions);
        let operation = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            handle_blocking(&sessions, request)
        });

        timeout(operation_timeout, operation)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "daemon_request_timeout: request exceeded {} ms",
                    operation_timeout.as_millis()
                )
            })?
            .context("join daemon request task")?
    }

    fn begin_shutdown(&self) {
        self.shutdown.notify_waiters();
    }
}

fn handle_blocking(sessions: &SessionRegistry, request: Request) -> Result<serde_json::Value> {
    match request {
        Request::Providers => Ok(serde_json::to_value(provider::list())?),
        Request::Spawn(request) => Ok(serde_json::to_value(sessions.spawn(request)?)?),
        Request::List => Ok(serde_json::to_value(sessions.list())?),
        Request::Status { session } => Ok(serde_json::to_value(sessions.get(&session)?.info())?),
        Request::Send { session, message } => {
            let session = sessions.get(&session)?;
            session.send(&message)?;
            Ok(serde_json::json!({ "sent": true, "submitted": true }))
        }
        Request::Key { session, key } => {
            sessions.get(&session)?.key(key)?;
            Ok(serde_json::json!({ "delivered": true }))
        }
        Request::Input {
            session,
            data_base64,
        } => {
            let bytes = STANDARD
                .decode(&data_base64)
                .context("decode base64 terminal input")?;
            sessions.get(&session)?.raw_input(&bytes)?;
            Ok(serde_json::json!({
                "delivered": true,
                "bytes": bytes.len()
            }))
        }
        Request::Output {
            session,
            after,
            limit,
            raw,
        } => Ok(serde_json::to_value(
            sessions.get(&session)?.output(after, limit, raw)?,
        )?),
        Request::Wait {
            session,
            after,
            limit,
            timeout_ms,
            raw,
        } => Ok(serde_json::to_value(
            sessions
                .get(&session)?
                .wait(after, limit, timeout_ms, raw)?,
        )?),
        Request::Interrupt { session } => {
            Ok(serde_json::to_value(sessions.get(&session)?.interrupt()?)?)
        }
        Request::Stop { session } => {
            let session = sessions.get(&session)?;
            session.stop()?;
            Ok(serde_json::to_value(session.info())?)
        }
        Request::Delete { session } => Ok(serde_json::json!({
            "deleted": sessions.delete(&session)?
        })),
        Request::Prune { older_than_ms } => {
            Ok(serde_json::to_value(sessions.prune(older_than_ms)?)?)
        }
        Request::Ping | Request::Shutdown => unreachable!("handled before blocking dispatch"),
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

    drop(listener);
    let _ = std::fs::remove_file(&socket_path);
    let sessions = Arc::clone(&runtime.sessions);
    tokio::task::spawn_blocking(move || sessions.stop_all())
        .await
        .context("join daemon session shutdown")?;
    let _ = std::fs::remove_file(paths::pid_path()?);
    Ok(())
}

async fn handle_connection(stream: UnixStream, runtime: Arc<Runtime>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    timeout(
        CONNECTION_IO_TIMEOUT,
        BufReader::new(reader).read_line(&mut line),
    )
    .await
    .map_err(|_| anyhow::anyhow!("daemon_read_timeout: request line exceeded 2000 ms"))??;

    let mut shutdown_after_response = false;
    let response = match serde_json::from_str::<Envelope>(&line) {
        Ok(envelope) => {
            shutdown_after_response = matches!(envelope.request, Request::Shutdown);
            match runtime.handle(envelope.request).await {
                Ok(result) => Response::success(envelope.id, result),
                Err(error) => Response::failure(envelope.id, format!("{error:#}")),
            }
        }
        Err(error) => Response::failure(0, format!("invalid request: {error}")),
    };
    let mut encoded = serde_json::to_vec(&response)?;
    encoded.push(b'\n');
    timeout(CONNECTION_IO_TIMEOUT, writer.write_all(&encoded))
        .await
        .map_err(|_| anyhow::anyhow!("daemon_write_timeout: response exceeded 2000 ms"))??;
    if shutdown_after_response && response.ok {
        runtime.begin_shutdown();
    }
    Ok(())
}

fn request_timeout(request: &Request) -> Duration {
    if let Some(wait_ms) = request.wait_timeout_ms() {
        return Duration::from_millis(wait_ms.saturating_add(500));
    }
    match request {
        Request::Providers => Duration::from_secs(5),
        Request::Stop { .. } => Duration::from_secs(4),
        _ => DEFAULT_REQUEST_TIMEOUT,
    }
}

async fn prepare_socket(socket_path: &Path) -> Result<()> {
    if !socket_path.exists() {
        return Ok(());
    }

    if let Ok(Ok(_)) = timeout(CONNECT_TIMEOUT, UnixStream::connect(socket_path)).await {
        bail!("an agentmux daemon is already running");
    }
    std::fs::remove_file(socket_path)
        .with_context(|| format!("remove stale socket {}", socket_path.display()))?;
    Ok(())
}

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
