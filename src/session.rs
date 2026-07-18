use crate::{
    protocol::{MAX_OUTPUT_LIMIT, SpawnRequest, TerminalKey},
    provider::{self, LaunchMode},
};
use anyhow::{Context, Result, bail};
use portable_pty::{ChildKiller, CommandBuilder, PtySize, native_pty_system};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, LazyLock, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const PERSISTENCE_VERSION: u32 = 1;
const DEFAULT_LOG_SEGMENT_BYTES: u64 = 4 * 1024 * 1024;
const FINAL_OUTPUT_LIMIT: usize = 1024 * 1024;
const MAX_WAIT_MS: u64 = 30_000;
const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

static TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:sk-[a-z0-9_-]{12,}|gh[pousr]_[a-z0-9_]{12,}|xox[baprs]-[a-z0-9-]{10,}|AIza[a-z0-9_-]{20,})\b",
    )
    .expect("valid token regex")
});
static BEARER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(authorization\s*:\s*bearer\s+)[^\s]+").expect("valid bearer regex")
});
static SECRET_VALUE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)\b([A-Z][A-Z0-9_]*(?:TOKEN|API_KEY|SECRET|PASSWORD|DEVICE_CODE))\s*([:=])\s*([^\s,;]+)",
    )
    .expect("valid secret value regex")
});
static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[a-z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)+\b")
        .expect("valid email regex")
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Completed,
    Failed,
    Stopped,
    Orphaned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    Running,
    Exited,
    Orphaned,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Pending,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessState {
    Starting,
    Ready,
    RunningTurn,
    NeedsAuth,
    NeedsTrust,
    NeedsInput,
    Finished,
    Orphaned,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageInfo {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub cwd: String,
    pub command: Vec<String>,
    pub launch_mode: LaunchMode,
    pub safety_profile: String,
    pub status: SessionStatus,
    pub process_status: ProcessStatus,
    pub outcome: TaskOutcome,
    pub readiness: ReadinessState,
    pub created_at_ms: u128,
    pub finished_at_ms: Option<u128>,
    pub duration_ms: Option<u128>,
    pub process_id: Option<u32>,
    pub exit_code: Option<u32>,
    pub exit_signal: Option<String>,
    pub output_cursor: u64,
    pub blocked_reason: Option<String>,
    pub final_text: Option<String>,
    pub provider_error: Option<String>,
    #[serde(default)]
    pub usage: Option<UsageInfo>,
}

#[derive(Debug, Serialize)]
pub struct OutputChunk {
    pub session: String,
    pub after: u64,
    pub cursor: u64,
    pub dropped_before: bool,
    pub truncated: bool,
    pub raw: bool,
    pub redactions_applied: bool,
    pub text: String,
    pub normalized_text: String,
    pub screen_text: Option<String>,
    pub status: SessionStatus,
    pub process_status: ProcessStatus,
    pub outcome: TaskOutcome,
}

#[derive(Debug, Serialize)]
pub struct WaitResult {
    pub changed: bool,
    pub timed_out: bool,
    pub output: OutputChunk,
}

#[derive(Debug, Serialize)]
pub struct InterruptDelivery {
    pub signal_delivered: bool,
    pub turn_cancelled: bool,
    pub process_exited: bool,
}

#[derive(Debug, Serialize)]
pub struct PruneResult {
    pub deleted: Vec<String>,
    pub count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedSession {
    version: u32,
    info: SessionInfo,
}

#[derive(Debug, Clone)]
struct SessionPaths {
    output: PathBuf,
    rotated_output: PathBuf,
    screen: PathBuf,
    metadata: PathBuf,
}

impl SessionPaths {
    fn new(logs_dir: &Path, id: &str) -> Self {
        Self {
            output: logs_dir.join(format!("{id}.log")),
            rotated_output: logs_dir.join(format!("{id}.log.1")),
            screen: logs_dir.join(format!("{id}.screen")),
            metadata: logs_dir.join(format!("{id}.json")),
        }
    }
}

type ChangeSignal = Arc<(Mutex<u64>, Condvar)>;

pub struct Session {
    info: Arc<RwLock<SessionInfo>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    killer: Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>,
    paths: SessionPaths,
    metadata_lock: Arc<Mutex<()>>,
    log_lock: Arc<Mutex<()>>,
    changes: ChangeSignal,
    stop_requested: Arc<AtomicBool>,
    interrupt_requested: Arc<AtomicBool>,
}

impl Session {
    fn recovered(mut info: SessionInfo, paths: SessionPaths) -> Result<Self> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.output)?;
        secure_permissions(&paths.output)?;
        let retained = file_len(&paths.output) + file_len(&paths.rotated_output);
        info.output_cursor = info.output_cursor.max(retained);
        if info.process_status == ProcessStatus::Running {
            info.status = SessionStatus::Orphaned;
            info.process_status = ProcessStatus::Orphaned;
            info.outcome = TaskOutcome::Unknown;
            info.readiness = ReadinessState::Orphaned;
            info.blocked_reason = Some(
                "daemon restarted while the provider was running; process ownership was lost"
                    .to_string(),
            );
        }
        Ok(Self {
            info: Arc::new(RwLock::new(info)),
            writer: Mutex::new(None),
            killer: Mutex::new(None),
            paths,
            metadata_lock: Arc::new(Mutex::new(())),
            log_lock: Arc::new(Mutex::new(())),
            changes: Arc::new((Mutex::new(0), Condvar::new())),
            stop_requested: Arc::new(AtomicBool::new(false)),
            interrupt_requested: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn info(&self) -> SessionInfo {
        self.info
            .read()
            .expect("session info lock poisoned")
            .clone()
    }

    pub fn send(&self, message: &str) -> Result<()> {
        let info = self.info();
        self.ensure_live(&info)?;
        if info.launch_mode == LaunchMode::Headless {
            bail!(
                "session '{}' is headless; spawn an interactive session for follow-up messages",
                info.name
            );
        }
        let message = message.trim_end_matches(['\r', '\n']);
        if message.is_empty() {
            bail!("semantic messages cannot be empty; use key or input for terminal control");
        }
        {
            let mut writer = self.writer.lock().expect("PTY writer lock poisoned");
            let writer = writer
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("session '{}' has no live PTY", info.name))?;
            writer.write_all(message.as_bytes())?;
            writer.write_all(b"\r")?;
            writer.flush()?;
        }
        self.set_running_turn()?;
        Ok(())
    }

    pub fn key(&self, key: TerminalKey) -> Result<()> {
        let bytes: &[u8] = match key {
            TerminalKey::Enter => b"\r",
            TerminalKey::Escape => &[27],
            TerminalKey::CtrlC => &[3],
            TerminalKey::Up => b"\x1b[A",
            TerminalKey::Down => b"\x1b[B",
        };
        self.raw_input(bytes)
    }

    pub fn raw_input(&self, bytes: &[u8]) -> Result<()> {
        let info = self.info();
        self.ensure_live(&info)?;
        if bytes.is_empty() {
            bail!("raw input cannot be empty");
        }
        let mut writer = self.writer.lock().expect("PTY writer lock poisoned");
        let writer = writer
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("session '{}' has no live PTY", info.name))?;
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    pub fn interrupt(&self) -> Result<InterruptDelivery> {
        let info = self.info();
        self.ensure_live(&info)?;
        self.interrupt_requested.store(true, Ordering::SeqCst);
        self.key(TerminalKey::CtrlC)?;
        Ok(InterruptDelivery {
            signal_delivered: true,
            turn_cancelled: false,
            process_exited: false,
        })
    }

    pub fn stop(&self) -> Result<()> {
        let current = self.info();
        if current.process_status == ProcessStatus::Exited {
            return Ok(());
        }
        self.ensure_live(&current)?;
        self.stop_requested.store(true, Ordering::SeqCst);
        let mut killer = self.killer.lock().expect("PTY killer lock poisoned");
        killer
            .as_mut()
            .ok_or_else(|| {
                anyhow::anyhow!("session '{}' has no live process handle", current.name)
            })?
            .kill()?;
        drop(killer);

        let deadline = Instant::now() + PROCESS_EXIT_TIMEOUT;
        while Instant::now() < deadline {
            if self.info().process_status == ProcessStatus::Exited {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        bail!(
            "process for session '{}' did not exit after stop request",
            current.name
        )
    }

    pub fn output(&self, after: u64, requested_limit: usize, raw: bool) -> Result<OutputChunk> {
        let limit = requested_limit.clamp(1, MAX_OUTPUT_LIMIT);
        let _log_guard = self.log_lock.lock().expect("session log lock poisoned");
        let info = self.info();
        let retained = read_retained_output(&self.paths, info.output_cursor, after, limit)?;
        let raw_text = String::from_utf8_lossy(&retained.bytes).into_owned();
        let normalized = normalize_output(&raw_text);
        let (normalized_text, normalized_redacted) = redact_text(&normalized);
        let (raw_text, raw_redacted) = redact_text(&raw_text);
        let screen = std::fs::read_to_string(&self.paths.screen)
            .ok()
            .map(|screen| redact_text(&screen))
            .filter(|(screen, _)| !screen.trim().is_empty());
        let screen_redacted = screen.as_ref().is_some_and(|(_, redacted)| *redacted);
        Ok(OutputChunk {
            session: info.name,
            after: retained.start,
            cursor: retained.cursor,
            dropped_before: retained.dropped_before,
            truncated: retained.cursor < info.output_cursor,
            raw,
            redactions_applied: normalized_redacted || raw_redacted || screen_redacted,
            text: if raw {
                raw_text
            } else {
                normalized_text.clone()
            },
            normalized_text,
            screen_text: screen.map(|(screen, _)| screen),
            status: info.status,
            process_status: info.process_status,
            outcome: info.outcome,
        })
    }

    pub fn wait(
        &self,
        after: u64,
        requested_limit: usize,
        timeout_ms: u64,
        raw: bool,
    ) -> Result<WaitResult> {
        let timeout = Duration::from_millis(timeout_ms.min(MAX_WAIT_MS));
        let deadline = Instant::now() + timeout;
        let (revision_lock, condition) = &*self.changes;
        let mut revision = revision_lock.lock().expect("change lock poisoned");
        let initial_revision = *revision;
        let mut changed = false;
        let mut timed_out = false;

        loop {
            let info = self.info();
            if info.output_cursor > after
                || info.process_status != ProcessStatus::Running
                || *revision != initial_revision
            {
                changed = true;
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                timed_out = true;
                break;
            }
            let waited = condition
                .wait_timeout(revision, remaining)
                .expect("change lock poisoned while waiting");
            revision = waited.0;
            if waited.1.timed_out() {
                timed_out = true;
                break;
            }
        }
        drop(revision);

        Ok(WaitResult {
            changed,
            timed_out,
            output: self.output(after, requested_limit, raw)?,
        })
    }

    fn ensure_live(&self, info: &SessionInfo) -> Result<()> {
        if info.status != SessionStatus::Running || info.process_status != ProcessStatus::Running {
            bail!("session '{}' is not running", info.name);
        }
        Ok(())
    }

    fn set_running_turn(&self) -> Result<()> {
        {
            let mut info = self.info.write().expect("session info lock poisoned");
            if info.status == SessionStatus::Running {
                info.readiness = ReadinessState::RunningTurn;
                info.blocked_reason = None;
            }
        }
        self.persist()?;
        notify_change(&self.changes);
        Ok(())
    }

    fn persist(&self) -> Result<()> {
        persist_info(&self.paths.metadata, &self.metadata_lock, &self.info())
    }
}

pub struct SessionRegistry {
    sessions: RwLock<HashMap<String, Arc<Session>>>,
    logs_dir: PathBuf,
    log_segment_bytes: u64,
}

impl SessionRegistry {
    pub fn new(state_dir: &Path) -> Result<Self> {
        let logs_dir = state_dir.join("sessions");
        std::fs::create_dir_all(&logs_dir)?;
        let registry = Self {
            sessions: RwLock::new(HashMap::new()),
            logs_dir,
            log_segment_bytes: configured_log_segment_bytes(),
        };
        registry.load_persisted()?;
        Ok(registry)
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
        let launch = provider::resolve(
            &request.provider,
            &request.command,
            request.prompt.as_deref(),
            request.mode,
            request.safety,
        )?;
        let id = Uuid::new_v4().to_string();
        let paths = SessionPaths::new(&self.logs_dir, &id);
        File::create(&paths.output)?;
        secure_permissions(&paths.output)?;

        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut builder = CommandBuilder::new(&launch.command[0]);
        for arg in &launch.command[1..] {
            builder.arg(arg);
        }
        builder.cwd(&cwd);
        builder.env("TERM", "xterm-256color");
        builder.env("NO_COLOR", "1");
        builder.env("AGENTMUX_SESSION", &request.name);

        let child = pair
            .slave
            .spawn_command(builder)
            .with_context(|| format!("spawn command '{}'", launch.command.join(" ")))?;
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
            command: launch.command,
            launch_mode: launch.mode,
            safety_profile: launch.safety_profile.to_string(),
            status: SessionStatus::Running,
            process_status: ProcessStatus::Running,
            outcome: TaskOutcome::Pending,
            readiness: if launch.mode == LaunchMode::Headless {
                ReadinessState::RunningTurn
            } else {
                ReadinessState::Starting
            },
            created_at_ms: now_ms(),
            finished_at_ms: None,
            duration_ms: None,
            process_id,
            exit_code: None,
            exit_signal: None,
            output_cursor: 0,
            blocked_reason: None,
            final_text: None,
            provider_error: None,
            usage: None,
        }));
        let metadata_lock = Arc::new(Mutex::new(()));
        let log_lock = Arc::new(Mutex::new(()));
        let changes = Arc::new((Mutex::new(0), Condvar::new()));
        let stop_requested = Arc::new(AtomicBool::new(false));
        let interrupt_requested = Arc::new(AtomicBool::new(false));
        let session = Arc::new(Session {
            info: Arc::clone(&info),
            writer: Mutex::new(Some(writer)),
            killer: Mutex::new(Some(killer)),
            paths: paths.clone(),
            metadata_lock: Arc::clone(&metadata_lock),
            log_lock: Arc::clone(&log_lock),
            changes: Arc::clone(&changes),
            stop_requested: Arc::clone(&stop_requested),
            interrupt_requested: Arc::clone(&interrupt_requested),
        });
        session.persist()?;

        self.sessions
            .write()
            .expect("session registry lock poisoned")
            .insert(request.name, Arc::clone(&session));

        let (reader_done_tx, reader_done_rx) = mpsc::channel();
        spawn_reader(
            reader,
            paths.clone(),
            Arc::clone(&info),
            Arc::clone(&metadata_lock),
            Arc::clone(&log_lock),
            Arc::clone(&changes),
            self.log_segment_bytes,
            reader_done_tx,
        );
        spawn_waiter(
            child,
            Arc::clone(&info),
            paths,
            metadata_lock,
            log_lock,
            changes,
            launch.mode,
            stop_requested,
            interrupt_requested,
            reader_done_rx,
        );

        if let Some(prompt) = launch.initial_input {
            spawn_initial_input(Arc::clone(&session), prompt);
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

    pub fn delete(&self, name_or_id: &str) -> Result<SessionInfo> {
        let session = self.get(name_or_id)?;
        let info = session.info();
        if info.process_status == ProcessStatus::Running {
            bail!(
                "session '{}' is still running; stop it before deletion",
                info.name
            );
        }
        self.sessions
            .write()
            .expect("session registry lock poisoned")
            .remove(&info.name);
        for path in [
            &session.paths.output,
            &session.paths.rotated_output,
            &session.paths.screen,
            &session.paths.metadata,
        ] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(info)
    }

    pub fn prune(&self, older_than_ms: u64) -> Result<PruneResult> {
        if older_than_ms == 0 {
            bail!("older_than_ms must be greater than zero");
        }
        let cutoff = now_ms().saturating_sub(older_than_ms as u128);
        let candidates: Vec<String> = self
            .list()
            .into_iter()
            .filter(|info| info.process_status != ProcessStatus::Running)
            .filter(|info| info.finished_at_ms.unwrap_or(info.created_at_ms) <= cutoff)
            .map(|info| info.name)
            .collect();
        let mut deleted = Vec::new();
        for name in candidates {
            self.delete(&name)?;
            deleted.push(name);
        }
        let count = deleted.len();
        Ok(PruneResult { deleted, count })
    }

    pub fn stop_all(&self) {
        let sessions: Vec<_> = self
            .sessions
            .read()
            .expect("session registry lock poisoned")
            .values()
            .filter(|session| session.info().process_status == ProcessStatus::Running)
            .cloned()
            .collect();
        for session in sessions {
            let _ = session.stop();
        }
    }

    fn load_persisted(&self) -> Result<()> {
        let entries = std::fs::read_dir(&self.logs_dir)?;
        let mut recovered = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let result: Result<PersistedSession> = (|| {
                let bytes = std::fs::read(&path).context("read session metadata")?;
                Ok(serde_json::from_slice(&bytes)?)
            })();
            let persisted = match result {
                Ok(persisted) if persisted.version == PERSISTENCE_VERSION => persisted,
                Ok(persisted) => {
                    eprintln!(
                        "ignoring unsupported session metadata version {} in {}",
                        persisted.version,
                        path.display()
                    );
                    continue;
                }
                Err(error) => {
                    eprintln!(
                        "ignoring invalid session metadata {}: {error:#}",
                        path.display()
                    );
                    continue;
                }
            };
            let paths = SessionPaths::new(&self.logs_dir, &persisted.info.id);
            let session = Arc::new(Session::recovered(persisted.info, paths)?);
            session.persist()?;
            recovered.push(session);
        }
        recovered.sort_by_key(|session| session.info().created_at_ms);
        let mut sessions = self
            .sessions
            .write()
            .expect("session registry lock poisoned");
        for session in recovered {
            sessions.insert(session.info().name, session);
        }
        Ok(())
    }
}

fn spawn_initial_input(session: Arc<Session>, prompt: String) {
    thread::spawn(move || {
        let started = Instant::now();
        let fallback_delay = if session.info().provider == "shell" {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(2)
        };
        loop {
            let info = session.info();
            if info.status != SessionStatus::Running {
                return;
            }
            match info.readiness {
                ReadinessState::Ready => {
                    let _ = session.send(&prompt);
                    return;
                }
                ReadinessState::NeedsAuth
                | ReadinessState::NeedsTrust
                | ReadinessState::NeedsInput => {}
                ReadinessState::Starting if started.elapsed() >= fallback_delay => {
                    let _ = session.send(&prompt);
                    return;
                }
                _ => {}
            }
            thread::sleep(Duration::from_millis(25));
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    paths: SessionPaths,
    info: Arc<RwLock<SessionInfo>>,
    metadata_lock: Arc<Mutex<()>>,
    log_lock: Arc<Mutex<()>>,
    changes: ChangeSignal,
    log_segment_bytes: u64,
    done: mpsc::Sender<()>,
) {
    thread::spawn(move || {
        let mut output = match OpenOptions::new().append(true).open(&paths.output) {
            Ok(output) => output,
            Err(error) => {
                fail_reader(&info, &paths, &metadata_lock, &changes, error.to_string());
                let _ = done.send(());
                return;
            }
        };
        let mut parser = vt100::Parser::new(40, 120, 1000);
        let mut last_screen = String::new();
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let latest_output = String::from_utf8_lossy(&buffer[..read]);
                    let write_result = {
                        let _guard = log_lock.lock().expect("session log lock poisoned");
                        append_with_rotation(
                            &mut output,
                            &paths,
                            &buffer[..read],
                            log_segment_bytes,
                        )
                        .map(|rotated| {
                            let readiness_changed = {
                                let mut info = info.write().expect("session info lock poisoned");
                                info.output_cursor += read as u64;
                                update_readiness(&mut info, &latest_output)
                            };
                            (rotated, readiness_changed)
                        })
                    };
                    let (rotated, readiness_changed) = match write_result {
                        Ok(result) => result,
                        Err(error) => {
                            fail_reader(&info, &paths, &metadata_lock, &changes, error.to_string());
                            break;
                        }
                    };

                    parser.process(&buffer[..read]);
                    let screen = parser.screen().contents();
                    if screen != last_screen {
                        if let Err(error) = persist_text(&paths.screen, &screen) {
                            fail_reader(
                                &info,
                                &paths,
                                &metadata_lock,
                                &changes,
                                format!("persist terminal screen: {error}"),
                            );
                            break;
                        }
                        last_screen = screen;
                    }

                    if readiness_changed || rotated {
                        let snapshot = info.read().expect("session info lock poisoned").clone();
                        let _ = persist_info(&paths.metadata, &metadata_lock, &snapshot);
                    }
                    notify_change(&changes);
                }
                Err(error) => {
                    let running = info
                        .read()
                        .expect("session info lock poisoned")
                        .process_status
                        == ProcessStatus::Running;
                    if running {
                        fail_reader(
                            &info,
                            &paths,
                            &metadata_lock,
                            &changes,
                            format!("read PTY output: {error}"),
                        );
                    }
                    break;
                }
            }
        }
        let snapshot = info.read().expect("session info lock poisoned").clone();
        let _ = persist_info(&paths.metadata, &metadata_lock, &snapshot);
        let _ = done.send(());
    });
}

fn append_with_rotation(
    output: &mut File,
    paths: &SessionPaths,
    mut bytes: &[u8],
    segment_bytes: u64,
) -> Result<bool> {
    let mut rotated = false;
    while !bytes.is_empty() {
        let current_len = file_len(&paths.output);
        if current_len >= segment_bytes {
            output.flush()?;
            if paths.rotated_output.exists() {
                std::fs::remove_file(&paths.rotated_output)?;
            }
            std::fs::rename(&paths.output, &paths.rotated_output)?;
            *output = File::create(&paths.output)?;
            secure_permissions(&paths.output)?;
            rotated = true;
            continue;
        }
        let available = (segment_bytes - current_len) as usize;
        let write = available.min(bytes.len());
        output.write_all(&bytes[..write])?;
        bytes = &bytes[write..];
    }
    output.flush()?;
    Ok(rotated)
}

fn fail_reader(
    info: &Arc<RwLock<SessionInfo>>,
    paths: &SessionPaths,
    metadata_lock: &Arc<Mutex<()>>,
    changes: &ChangeSignal,
    error: String,
) {
    let snapshot = {
        let mut info = info.write().expect("session info lock poisoned");
        info.status = SessionStatus::Failed;
        info.outcome = TaskOutcome::Failed;
        info.provider_error = Some(error);
        info.clone()
    };
    let _ = persist_info(&paths.metadata, metadata_lock, &snapshot);
    notify_change(changes);
}

fn update_readiness(info: &mut SessionInfo, output: &str) -> bool {
    if info.launch_mode == LaunchMode::Headless || info.status != SessionStatus::Running {
        return false;
    }
    let output = normalize_output(output).to_lowercase();
    let (readiness, blocked_reason) = if contains_any(
        &output,
        &["trust this folder", "trust this directory", "do you trust"],
    ) {
        (
            ReadinessState::NeedsTrust,
            Some("provider is waiting for workspace trust".to_string()),
        )
    } else if contains_any(
        &output,
        &[
            "login required",
            "log in to continue",
            "sign in to continue",
            "authentication required",
        ],
    ) {
        (
            ReadinessState::NeedsAuth,
            Some("provider authentication is required".to_string()),
        )
    } else if contains_any(
        &output,
        &[
            "permission required",
            "approve this command",
            "allow this command",
            "waiting for approval",
        ],
    ) {
        (
            ReadinessState::NeedsInput,
            Some("provider is waiting for approval or input".to_string()),
        )
    } else if matches!(
        info.readiness,
        ReadinessState::Starting
            | ReadinessState::NeedsAuth
            | ReadinessState::NeedsTrust
            | ReadinessState::NeedsInput
    ) {
        (ReadinessState::Ready, None)
    } else {
        return false;
    };
    let changed = info.readiness != readiness || info.blocked_reason != blocked_reason;
    info.readiness = readiness;
    info.blocked_reason = blocked_reason;
    changed
}

#[allow(clippy::too_many_arguments)]
fn spawn_waiter(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    info: Arc<RwLock<SessionInfo>>,
    paths: SessionPaths,
    metadata_lock: Arc<Mutex<()>>,
    log_lock: Arc<Mutex<()>>,
    changes: ChangeSignal,
    launch_mode: LaunchMode,
    stop_requested: Arc<AtomicBool>,
    interrupt_requested: Arc<AtomicBool>,
    reader_done: mpsc::Receiver<()>,
) {
    thread::spawn(move || {
        let result = child.wait();
        let _ = reader_done.recv_timeout(Duration::from_secs(2));
        let normalized = {
            let _guard = log_lock.lock().expect("session log lock poisoned");
            read_final_output(&paths)
                .map(|output| redact_text(&normalize_output(&output)).0)
                .unwrap_or_default()
        };
        let finished_at_ms = now_ms();
        let snapshot = {
            let mut info = info.write().expect("session info lock poisoned");
            info.process_status = ProcessStatus::Exited;
            info.process_id = None;
            info.finished_at_ms = Some(finished_at_ms);
            info.duration_ms = Some(finished_at_ms.saturating_sub(info.created_at_ms));
            info.readiness = ReadinessState::Finished;

            match result {
                Ok(exit) => {
                    info.exit_code = Some(exit.exit_code());
                    info.exit_signal = exit.signal().map(ToOwned::to_owned);
                    if stop_requested.load(Ordering::SeqCst) {
                        info.status = SessionStatus::Stopped;
                        info.outcome = TaskOutcome::Cancelled;
                    } else if interrupt_requested.load(Ordering::SeqCst) && !exit.success() {
                        info.status = SessionStatus::Stopped;
                        info.outcome = TaskOutcome::Cancelled;
                        info.blocked_reason =
                            Some("process exited after interrupt delivery".to_string());
                    } else if launch_mode == LaunchMode::Headless {
                        classify_headless_result(&mut info, exit.success(), normalized);
                    } else if exit.success() {
                        info.status = SessionStatus::Completed;
                        info.outcome = TaskOutcome::Succeeded;
                        info.final_text = (!normalized.is_empty()).then_some(normalized);
                    } else {
                        info.status = SessionStatus::Failed;
                        info.outcome = TaskOutcome::Failed;
                        info.provider_error =
                            (!normalized.is_empty()).then_some(normalized).or_else(|| {
                                Some("interactive provider exited unsuccessfully".to_string())
                            });
                    }
                }
                Err(error) => {
                    info.status = SessionStatus::Failed;
                    info.outcome = TaskOutcome::Failed;
                    info.provider_error = Some(format!("wait for provider process: {error}"));
                }
            }
            info.clone()
        };
        let _ = persist_info(&paths.metadata, &metadata_lock, &snapshot);
        notify_change(&changes);
    });
}

fn classify_headless_result(info: &mut SessionInfo, exit_success: bool, normalized: String) {
    if !exit_success {
        info.status = SessionStatus::Failed;
        info.outcome = TaskOutcome::Failed;
        info.provider_error = Some(if normalized.is_empty() {
            "headless provider exited unsuccessfully without output".to_string()
        } else {
            normalized
        });
        return;
    }

    let lower = normalized.to_lowercase();
    if normalized.is_empty() || lower.contains("no output produced") {
        info.status = SessionStatus::Failed;
        info.outcome = TaskOutcome::Failed;
        info.provider_error = Some(if normalized.is_empty() {
            "headless provider exited successfully but produced no result".to_string()
        } else {
            normalized
        });
        return;
    }

    info.status = SessionStatus::Completed;
    info.outcome = TaskOutcome::Succeeded;
    info.final_text = Some(normalized);
}

struct RetainedOutput {
    start: u64,
    cursor: u64,
    dropped_before: bool,
    bytes: Vec<u8>,
}

fn read_retained_output(
    paths: &SessionPaths,
    output_cursor: u64,
    after: u64,
    limit: usize,
) -> Result<RetainedOutput> {
    let rotated_len = file_len(&paths.rotated_output);
    let active_len = file_len(&paths.output);
    let retained_len = rotated_len + active_len;
    let retained_start = output_cursor.saturating_sub(retained_len);
    let start = after.max(retained_start).min(output_cursor);
    let dropped_before = after < retained_start;
    let mut remaining = limit;
    let mut bytes = Vec::with_capacity(limit);
    let offset = start.saturating_sub(retained_start);

    if offset < rotated_len && remaining > 0 {
        read_file_slice(&paths.rotated_output, offset, &mut remaining, &mut bytes)?;
    }
    if remaining > 0 {
        let active_offset = offset.saturating_sub(rotated_len);
        read_file_slice(&paths.output, active_offset, &mut remaining, &mut bytes)?;
    }

    Ok(RetainedOutput {
        start,
        cursor: start + bytes.len() as u64,
        dropped_before,
        bytes,
    })
}

fn read_file_slice(
    path: &Path,
    offset: u64,
    remaining: &mut usize,
    output: &mut Vec<u8>,
) -> Result<()> {
    if *remaining == 0 || !path.exists() {
        return Ok(());
    }
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if offset >= len {
        return Ok(());
    }
    file.seek(SeekFrom::Start(offset))?;
    let before = output.len();
    file.take(*remaining as u64).read_to_end(output)?;
    *remaining -= output.len() - before;
    Ok(())
}

fn read_final_output(paths: &SessionPaths) -> Result<String> {
    let output_cursor = file_len(&paths.rotated_output) + file_len(&paths.output);
    let start = output_cursor.saturating_sub(FINAL_OUTPUT_LIMIT as u64);
    let retained = read_retained_output(paths, output_cursor, start, FINAL_OUTPUT_LIMIT)?;
    Ok(String::from_utf8_lossy(&retained.bytes).into_owned())
}

fn persist_info(path: &Path, lock: &Arc<Mutex<()>>, info: &SessionInfo) -> Result<()> {
    let _guard = lock.lock().expect("session metadata lock poisoned");
    let persisted = PersistedSession {
        version: PERSISTENCE_VERSION,
        info: info.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&persisted)?;
    persist_bytes(path, &bytes)
}

fn persist_text(path: &Path, text: &str) -> Result<()> {
    persist_bytes(path, text.as_bytes())
}

fn persist_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("data")
    ));
    std::fs::write(&temporary, bytes)?;
    secure_permissions(&temporary)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn notify_change(changes: &ChangeSignal) {
    let (revision, condition) = &**changes;
    let mut revision = revision.lock().expect("change lock poisoned");
    *revision = revision.wrapping_add(1);
    condition.notify_all();
}

fn normalize_output(output: &str) -> String {
    let bytes = output.as_bytes();
    let mut clean = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            index += 1;
            if index >= bytes.len() {
                break;
            }
            match bytes[index] {
                b'[' => {
                    index += 1;
                    while index < bytes.len() {
                        let byte = bytes[index];
                        index += 1;
                        if (0x40..=0x7e).contains(&byte) {
                            break;
                        }
                    }
                }
                b']' => {
                    index += 1;
                    while index < bytes.len() {
                        if bytes[index] == 0x07 {
                            index += 1;
                            break;
                        }
                        if bytes[index] == 0x1b
                            && index + 1 < bytes.len()
                            && bytes[index + 1] == b'\\'
                        {
                            index += 2;
                            break;
                        }
                        index += 1;
                    }
                }
                _ => index += 1,
            }
            continue;
        }
        let byte = bytes[index];
        index += 1;
        if byte == b'\r' {
            if index < bytes.len() && bytes[index] == b'\n' {
                continue;
            }
            clean.push(b'\n');
        } else if byte == b'\n' || byte == b'\t' || !byte.is_ascii_control() {
            clean.push(byte);
        }
    }

    let decoded = String::from_utf8_lossy(&clean);
    let mut lines = Vec::new();
    for line in decoded.lines().map(str::trim_end) {
        if line.is_empty()
            && lines
                .last()
                .is_some_and(|previous: &&str| previous.is_empty())
        {
            continue;
        }
        if lines.last().is_some_and(|previous| *previous == line) {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

fn redact_text(text: &str) -> (String, bool) {
    let mut redacted = text.to_string();
    redacted = BEARER_RE
        .replace_all(&redacted, "${1}[REDACTED]")
        .into_owned();
    redacted = SECRET_VALUE_RE
        .replace_all(&redacted, "${1}${2}[REDACTED]")
        .into_owned();
    redacted = TOKEN_RE
        .replace_all(&redacted, "[REDACTED_TOKEN]")
        .into_owned();
    redacted = EMAIL_RE
        .replace_all(&redacted, "[REDACTED_EMAIL]")
        .into_owned();
    let changed = redacted != text;
    (redacted, changed)
}

fn configured_log_segment_bytes() -> u64 {
    std::env::var("AGENTMUX_LOG_SEGMENT_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value >= 1024)
        .unwrap_or(DEFAULT_LOG_SEGMENT_BYTES)
}

fn file_len(path: &Path) -> u64 {
    path.metadata().map(|metadata| metadata.len()).unwrap_or(0)
}

#[cfg(unix)]
fn secure_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
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

    #[test]
    fn normalizes_ansi_carriage_returns_and_duplicate_redraws() {
        let output = "\x1b[31mhello\x1b[0m\r\nhello\rworld";
        assert_eq!(normalize_output(output), "hello\nworld");
    }

    #[test]
    fn redacts_tokens_secrets_and_account_identifiers() {
        let input = "Authorization: Bearer abc123\nAPI_TOKEN=secret-value\nsk-abcdefghijklmnop\nme@example.com";
        let (output, changed) = redact_text(input);
        assert!(changed);
        assert!(!output.contains("abc123"));
        assert!(!output.contains("secret-value"));
        assert!(!output.contains("sk-abcdefghijklmnop"));
        assert!(!output.contains("me@example.com"));
    }

    #[test]
    fn empty_successful_headless_result_is_a_failure() {
        let mut info = test_info();
        classify_headless_result(&mut info, true, String::new());
        assert_eq!(info.status, SessionStatus::Failed);
        assert_eq!(info.outcome, TaskOutcome::Failed);
        assert!(info.final_text.is_none());
    }

    #[test]
    fn no_output_message_is_not_success() {
        let mut info = test_info();
        classify_headless_result(&mut info, true, "No output produced".to_string());
        assert_eq!(info.status, SessionStatus::Failed);
        assert_eq!(info.outcome, TaskOutcome::Failed);
    }

    #[test]
    fn startup_dialogs_are_reported_and_can_return_to_ready() {
        let mut info = test_info();
        info.launch_mode = LaunchMode::Interactive;
        info.readiness = ReadinessState::Starting;
        assert!(update_readiness(&mut info, "Do you trust this folder?"));
        assert_eq!(info.readiness, ReadinessState::NeedsTrust);
        assert!(info.blocked_reason.is_some());

        assert!(update_readiness(&mut info, "Welcome. Type a message."));
        assert_eq!(info.readiness, ReadinessState::Ready);
        assert!(info.blocked_reason.is_none());
    }

    fn test_info() -> SessionInfo {
        SessionInfo {
            id: "id".to_string(),
            name: "test".to_string(),
            provider: "test".to_string(),
            cwd: "/tmp".to_string(),
            command: vec!["test".to_string()],
            launch_mode: LaunchMode::Headless,
            safety_profile: "read_only".to_string(),
            status: SessionStatus::Running,
            process_status: ProcessStatus::Running,
            outcome: TaskOutcome::Pending,
            readiness: ReadinessState::RunningTurn,
            created_at_ms: 0,
            finished_at_ms: None,
            duration_ms: None,
            process_id: None,
            exit_code: None,
            exit_signal: None,
            output_cursor: 0,
            blocked_reason: None,
            final_text: None,
            provider_error: None,
            usage: None,
        }
    }
}
