use crate::{
    protocol::{MAX_OUTPUT_LIMIT, SpawnRequest, TerminalKey},
    provider::{self, LaunchMode},
};
use anyhow::{Context, Result, bail};
use portable_pty::{ChildKiller, CommandBuilder, PtySize, native_pty_system};
use serde::Serialize;
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const FINAL_OUTPUT_LIMIT: usize = 1024 * 1024;
const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Completed,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    Running,
    Exited,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Pending,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessState {
    Starting,
    Ready,
    RunningTurn,
    NeedsAuth,
    NeedsTrust,
    NeedsInput,
    Finished,
}

#[derive(Debug, Clone, Serialize)]
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
}

#[derive(Debug, Serialize)]
pub struct OutputChunk {
    pub session: String,
    pub after: u64,
    pub cursor: u64,
    pub truncated: bool,
    pub text: String,
    pub normalized_text: String,
    pub status: SessionStatus,
    pub process_status: ProcessStatus,
    pub outcome: TaskOutcome,
}

#[derive(Debug, Serialize)]
pub struct InterruptDelivery {
    pub signal_delivered: bool,
    pub turn_cancelled: bool,
    pub process_exited: bool,
}

pub struct Session {
    info: Arc<RwLock<SessionInfo>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    output_path: PathBuf,
    stop_requested: Arc<AtomicBool>,
    interrupt_requested: Arc<AtomicBool>,
}

impl Session {
    pub fn info(&self) -> SessionInfo {
        self.info
            .read()
            .expect("session info lock poisoned")
            .clone()
    }

    pub fn send(&self, message: &str) -> Result<()> {
        let info = self.info();
        if info.status != SessionStatus::Running {
            bail!("session '{}' is not running", info.name);
        }
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
        let mut writer = self.writer.lock().expect("PTY writer lock poisoned");
        writer.write_all(message.as_bytes())?;
        writer.write_all(b"\r")?;
        writer.flush()?;
        drop(writer);
        self.set_running_turn();
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
        if info.status != SessionStatus::Running {
            bail!("session '{}' is not running", info.name);
        }
        if bytes.is_empty() {
            bail!("raw input cannot be empty");
        }
        let mut writer = self.writer.lock().expect("PTY writer lock poisoned");
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    pub fn interrupt(&self) -> Result<InterruptDelivery> {
        let info = self.info();
        if info.status != SessionStatus::Running {
            bail!("session '{}' is not running", info.name);
        }
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
        self.stop_requested.store(true, Ordering::SeqCst);
        self.killer
            .lock()
            .expect("PTY killer lock poisoned")
            .kill()?;

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
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Ok(OutputChunk {
            session: info.name,
            after: start,
            cursor,
            truncated: cursor < len,
            normalized_text: normalize_output(&text),
            text,
            status: info.status,
            process_status: info.process_status,
            outcome: info.outcome,
        })
    }

    fn set_running_turn(&self) {
        let mut info = self.info.write().expect("session info lock poisoned");
        if info.status == SessionStatus::Running {
            info.readiness = ReadinessState::RunningTurn;
            info.blocked_reason = None;
        }
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
        let launch = provider::resolve(
            &request.provider,
            &request.command,
            request.prompt.as_deref(),
            request.mode,
            request.safety,
        )?;
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
        let created_at_ms = now_ms();
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
            created_at_ms,
            finished_at_ms: None,
            duration_ms: None,
            process_id,
            exit_code: None,
            exit_signal: None,
            output_cursor: 0,
            blocked_reason: None,
            final_text: None,
            provider_error: None,
        }));
        let stop_requested = Arc::new(AtomicBool::new(false));
        let interrupt_requested = Arc::new(AtomicBool::new(false));
        let session = Arc::new(Session {
            info: Arc::clone(&info),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            output_path: output_path.clone(),
            stop_requested: Arc::clone(&stop_requested),
            interrupt_requested: Arc::clone(&interrupt_requested),
        });

        self.sessions
            .write()
            .expect("session registry lock poisoned")
            .insert(request.name, Arc::clone(&session));

        let (reader_done_tx, reader_done_rx) = mpsc::channel();
        spawn_reader(
            reader,
            output_path.clone(),
            Arc::clone(&info),
            reader_done_tx,
        );
        spawn_waiter(
            child,
            Arc::clone(&info),
            output_path,
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

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    output_path: PathBuf,
    info: Arc<RwLock<SessionInfo>>,
    done: mpsc::Sender<()>,
) {
    thread::spawn(move || {
        let Ok(mut output) = OpenOptions::new().append(true).open(&output_path) else {
            let mut info = info.write().expect("session info lock poisoned");
            info.status = SessionStatus::Failed;
            info.outcome = TaskOutcome::Failed;
            info.provider_error = Some("failed to open session output log".to_string());
            let _ = done.send(());
            return;
        };
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if output.write_all(&buffer[..read]).is_err() || output.flush().is_err() {
                        let mut info = info.write().expect("session info lock poisoned");
                        info.status = SessionStatus::Failed;
                        info.outcome = TaskOutcome::Failed;
                        info.provider_error =
                            Some("failed to write session output log".to_string());
                        break;
                    }
                    let latest_output = String::from_utf8_lossy(&buffer[..read]);
                    let mut info = info.write().expect("session info lock poisoned");
                    info.output_cursor += read as u64;
                    update_readiness(&mut info, &latest_output);
                }
                Err(error) => {
                    let mut info = info.write().expect("session info lock poisoned");
                    if info.process_status == ProcessStatus::Running {
                        info.provider_error = Some(format!("read PTY output: {error}"));
                    }
                    break;
                }
            }
        }
        let _ = done.send(());
    });
}

fn update_readiness(info: &mut SessionInfo, output: &str) {
    if info.launch_mode == LaunchMode::Headless || info.status != SessionStatus::Running {
        return;
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
        return;
    };
    info.readiness = readiness;
    info.blocked_reason = blocked_reason;
}

#[allow(clippy::too_many_arguments)]
fn spawn_waiter(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    info: Arc<RwLock<SessionInfo>>,
    output_path: PathBuf,
    launch_mode: LaunchMode,
    stop_requested: Arc<AtomicBool>,
    interrupt_requested: Arc<AtomicBool>,
    reader_done: mpsc::Receiver<()>,
) {
    thread::spawn(move || {
        let result = child.wait();
        let _ = reader_done.recv_timeout(Duration::from_secs(2));
        let normalized = read_final_output(&output_path)
            .map(|output| normalize_output(&output))
            .unwrap_or_default();
        let finished_at_ms = now_ms();
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
                    return;
                }

                if interrupt_requested.load(Ordering::SeqCst) && !exit.success() {
                    info.status = SessionStatus::Stopped;
                    info.outcome = TaskOutcome::Cancelled;
                    info.blocked_reason =
                        Some("process exited after interrupt delivery".to_string());
                    return;
                }

                if launch_mode == LaunchMode::Headless {
                    classify_headless_result(&mut info, exit.success(), normalized);
                } else if exit.success() {
                    info.status = SessionStatus::Completed;
                    info.outcome = TaskOutcome::Succeeded;
                    info.final_text = (!normalized.is_empty()).then_some(normalized);
                } else {
                    info.status = SessionStatus::Failed;
                    info.outcome = TaskOutcome::Failed;
                    info.provider_error = (!normalized.is_empty())
                        .then_some(normalized)
                        .or_else(|| Some("interactive provider exited unsuccessfully".to_string()));
                }
            }
            Err(error) => {
                info.status = SessionStatus::Failed;
                info.outcome = TaskOutcome::Failed;
                info.provider_error = Some(format!("wait for provider process: {error}"));
            }
        }
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

fn read_final_output(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(FINAL_OUTPUT_LIMIT as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
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

    String::from_utf8_lossy(&clean)
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
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
    fn normalizes_ansi_and_carriage_returns() {
        let output = "\x1b[31mhello\x1b[0m\r\nworld\rnext";
        assert_eq!(normalize_output(output), "hello\nworld\nnext");
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
        update_readiness(&mut info, "Do you trust this folder?");
        assert_eq!(info.readiness, ReadinessState::NeedsTrust);
        assert!(info.blocked_reason.is_some());

        update_readiness(&mut info, "Welcome. Type a message.");
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
        }
    }
}
