use crate::{
    protocol::{MAX_OUTPUT_LIMIT, SpawnRequest},
    provider,
};
use anyhow::{Context, Result, bail};
use portable_pty::{ChildKiller, CommandBuilder, PtySize, native_pty_system};
use serde::Serialize;
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Completed,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub cwd: String,
    pub command: Vec<String>,
    pub status: SessionStatus,
    pub created_at_ms: u128,
    pub process_id: Option<u32>,
    pub exit_code: Option<u32>,
    pub exit_signal: Option<String>,
    pub output_cursor: u64,
}

#[derive(Debug, Serialize)]
pub struct OutputChunk {
    pub session: String,
    pub after: u64,
    pub cursor: u64,
    pub truncated: bool,
    pub text: String,
    pub status: SessionStatus,
}

pub struct Session {
    info: Arc<RwLock<SessionInfo>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    output_path: PathBuf,
}

impl Session {
    pub fn info(&self) -> SessionInfo {
        self.info
            .read()
            .expect("session info lock poisoned")
            .clone()
    }

    pub fn send(&self, message: &str) -> Result<()> {
        if self.info().status != SessionStatus::Running {
            bail!("session '{}' is not running", self.info().name);
        }
        let mut writer = self.writer.lock().expect("PTY writer lock poisoned");
        writer.write_all(message.as_bytes())?;
        if !message.ends_with('\n') {
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        Ok(())
    }

    pub fn interrupt(&self) -> Result<()> {
        if self.info().status != SessionStatus::Running {
            bail!("session '{}' is not running", self.info().name);
        }
        let mut writer = self.writer.lock().expect("PTY writer lock poisoned");
        writer.write_all(&[3])?;
        writer.flush()?;
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        let current = self.info().status;
        if current != SessionStatus::Running {
            return Ok(());
        }
        self.killer
            .lock()
            .expect("PTY killer lock poisoned")
            .kill()?;
        self.info
            .write()
            .expect("session info lock poisoned")
            .status = SessionStatus::Stopped;
        Ok(())
    }

    pub fn output(&self, after: u64, requested_limit: usize) -> Result<OutputChunk> {
        let limit = requested_limit.clamp(1, MAX_OUTPUT_LIMIT);
        let mut file = File::open(&self.output_path)
            .with_context(|| format!("open output log {}", self.output_path.display()))?;
        let len = file.metadata()?.len();
        let start = after.min(len);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::with_capacity(limit);
        file.take(limit as u64).read_to_end(&mut bytes)?;
        let cursor = start + bytes.len() as u64;
        let info = self.info();
        Ok(OutputChunk {
            session: info.name,
            after: start,
            cursor,
            truncated: cursor < len,
            text: String::from_utf8_lossy(&bytes).into_owned(),
            status: info.status,
        })
    }
}

pub struct SessionRegistry {
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    logs_dir: PathBuf,
}

impl SessionRegistry {
    pub fn new(state_dir: &Path) -> Result<Self> {
        let logs_dir = state_dir.join("sessions");
        std::fs::create_dir_all(&logs_dir)?;
        Ok(Self {
            sessions: RwLock::new(HashMap::new()),
            logs_dir,
        })
    }

    pub fn spawn(&self, request: SpawnRequest) -> Result<SessionInfo> {
        validate_name(&request.name)?;
        if self
            .sessions
            .read()
            .expect("session registry lock poisoned")
            .contains_key(&request.name)
        {
            bail!("session '{}' already exists", request.name);
        }

        let cwd = PathBuf::from(&request.cwd);
        if !cwd.is_dir() {
            bail!("working directory does not exist: {}", cwd.display());
        }
        let command = provider::resolve(&request.provider, &request.command)?;
        let id = Uuid::new_v4().to_string();
        let output_path = self.logs_dir.join(format!("{id}.log"));
        File::create(&output_path)?;

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut builder = CommandBuilder::new(&command[0]);
        for arg in &command[1..] {
            builder.arg(arg);
        }
        builder.cwd(&cwd);
        builder.env("TERM", "xterm-256color");
        builder.env("AGENTMUX_SESSION", &request.name);

        let child = pair
            .slave
            .spawn_command(builder)
            .with_context(|| format!("spawn command '{}'", command.join(" ")))?;
        drop(pair.slave);

        let process_id = child.process_id();
        let killer = child.clone_killer();
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let info = Arc::new(RwLock::new(SessionInfo {
            id,
            name: request.name.clone(),
            provider: request.provider,
            cwd: cwd.to_string_lossy().into_owned(),
            command,
            status: SessionStatus::Running,
            created_at_ms: now_ms(),
            process_id,
            exit_code: None,
            exit_signal: None,
            output_cursor: 0,
        }));
        let session = Arc::new(Session {
            info: Arc::clone(&info),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            output_path: output_path.clone(),
        });

        self.sessions
            .write()
            .expect("session registry lock poisoned")
            .insert(request.name, Arc::clone(&session));

        spawn_reader(reader, output_path, Arc::clone(&info));
        spawn_waiter(child, Arc::clone(&info));

        if let Some(prompt) = request.prompt {
            session.send(&prompt)?;
        }

        Ok(session.info())
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let mut sessions: Vec<_> = self
            .sessions
            .read()
            .expect("session registry lock poisoned")
            .values()
            .map(|session| session.info())
            .collect();
        sessions.sort_by(|left, right| left.created_at_ms.cmp(&right.created_at_ms));
        sessions
    }

    pub fn get(&self, name_or_id: &str) -> Result<Arc<Session>> {
        let sessions = self
            .sessions
            .read()
            .expect("session registry lock poisoned");
        sessions
            .get(name_or_id)
            .cloned()
            .or_else(|| {
                sessions
                    .values()
                    .find(|session| session.info().id == name_or_id)
                    .cloned()
            })
            .ok_or_else(|| anyhow::anyhow!("session '{name_or_id}' not found"))
    }

    pub fn stop_all(&self) {
        let sessions: Vec<_> = self
            .sessions
            .read()
            .expect("session registry lock poisoned")
            .values()
            .cloned()
            .collect();
        for session in sessions {
            let _ = session.stop();
        }
    }
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    output_path: PathBuf,
    info: Arc<RwLock<SessionInfo>>,
) {
    thread::spawn(move || {
        let Ok(mut output) = OpenOptions::new().append(true).open(&output_path) else {
            info.write().expect("session info lock poisoned").status = SessionStatus::Failed;
            return;
        };
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if output.write_all(&buffer[..read]).is_err() || output.flush().is_err() {
                        info.write().expect("session info lock poisoned").status =
                            SessionStatus::Failed;
                        break;
                    }
                    info.write()
                        .expect("session info lock poisoned")
                        .output_cursor += read as u64;
                }
                Err(_) => break,
            }
        }
    });
}

fn spawn_waiter(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    info: Arc<RwLock<SessionInfo>>,
) {
    thread::spawn(move || match child.wait() {
        Ok(exit) => {
            let mut info = info.write().expect("session info lock poisoned");
            info.exit_code = Some(exit.exit_code());
            info.exit_signal = exit.signal().map(ToOwned::to_owned);
            if info.status == SessionStatus::Running {
                info.status = if exit.success() {
                    SessionStatus::Completed
                } else {
                    SessionStatus::Failed
                };
            }
        }
        Err(_) => {
            let mut info = info.write().expect("session info lock poisoned");
            if info.status == SessionStatus::Running {
                info.status = SessionStatus::Failed;
            }
        }
    });
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
    {
        bail!("session names must be 1-64 ASCII letters, numbers, '-' or '_'");
    }
    Ok(())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_safe_session_names() {
        assert!(validate_name("backend-01").is_ok());
        assert!(validate_name("../escape").is_err());
        assert!(validate_name("").is_err());
    }
}
