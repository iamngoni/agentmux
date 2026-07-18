#![cfg(unix)]

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use std::{
    ffi::OsStr,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct DaemonGuard<'a> {
    state_dir: &'a Path,
}

impl Drop for DaemonGuard<'_> {
    fn drop(&mut self) {
        let _ = command(self.state_dir, ["--json", "daemon", "stop"]);
    }
}

#[test]
fn background_daemon_survives_starter_process_exit() {
    let state = tempfile::tempdir().expect("create isolated state directory");
    let _guard = DaemonGuard {
        state_dir: state.path(),
    };

    assert_success(command(state.path(), ["--json", "daemon", "start"]));
    thread::sleep(Duration::from_millis(300));
    let status = json(command(state.path(), ["--json", "daemon", "status"]));
    assert_eq!(status["status"], "ok");
}

#[test]
fn persistent_session_can_be_observed_and_steered() {
    let state = tempfile::tempdir().expect("create isolated state directory");
    let _guard = DaemonGuard {
        state_dir: state.path(),
    };

    assert_success(command(state.path(), ["--json", "daemon", "start"]));
    assert_success(command(
        state.path(),
        ["--json", "spawn", "e2e", "--provider", "shell"],
    ));
    assert_success(command(
        state.path(),
        ["--json", "send", "e2e", "printf \"first-proof\\n\""],
    ));

    let first = poll_json(state.path(), ["--json", "output", "e2e"], |value| {
        value["text"]
            .as_str()
            .is_some_and(|text| text.contains("first-proof"))
    });
    let cursor = first["cursor"].as_u64().expect("output cursor");

    assert_success(command(
        state.path(),
        ["--json", "send", "e2e", "printf \"second-proof\\n\""],
    ));
    let after = cursor.to_string();
    let second = poll_json(
        state.path(),
        ["--json", "output", "e2e", "--after", &after],
        |value| {
            value["text"]
                .as_str()
                .is_some_and(|text| text.contains("second-proof"))
        },
    );
    assert_eq!(second["after"].as_u64(), Some(cursor));
    assert!(
        !second["text"]
            .as_str()
            .expect("output text")
            .contains("first-proof")
    );

    assert_success(command(state.path(), ["--json", "send", "e2e", "exit 0"]));
    let status = poll_json(state.path(), ["--json", "status", "e2e"], |value| {
        value["status"] == "completed"
    });
    assert_eq!(status["exit_code"].as_u64(), Some(0));
    assert_eq!(status["process_status"], "exited");
    assert_eq!(status["outcome"], "succeeded");
    assert!(status["finished_at_ms"].is_number());
    assert!(status["duration_ms"].is_number());
}

#[test]
fn terminal_control_and_lifecycle_results_are_truthful() {
    let state = tempfile::tempdir().expect("create isolated state directory");
    let _guard = DaemonGuard {
        state_dir: state.path(),
    };

    assert_success(command(state.path(), ["--json", "daemon", "start"]));
    assert_success(command(
        state.path(),
        ["--json", "spawn", "controls", "--provider", "shell"],
    ));

    let empty = command(state.path(), ["--json", "send", "controls", ""]);
    assert_failure_with(&empty, "semantic messages cannot be empty");

    let raw_command = STANDARD.encode("printf 'raw-proof\\n'");
    assert_success(command(
        state.path(),
        ["--json", "input", "controls", &raw_command],
    ));
    assert_success(command(
        state.path(),
        ["--json", "key", "controls", "enter"],
    ));
    poll_json(state.path(), ["--json", "output", "controls"], |value| {
        value["normalized_text"]
            .as_str()
            .is_some_and(|text| text.contains("raw-proof"))
    });

    let interrupt = json(command(state.path(), ["--json", "interrupt", "controls"]));
    assert_eq!(interrupt["signal_delivered"], true);
    assert_eq!(interrupt["turn_cancelled"], false);
    assert_eq!(interrupt["process_exited"], false);

    let stopped = json(command(state.path(), ["--json", "stop", "controls"]));
    assert_eq!(stopped["status"], "stopped");
    assert_eq!(stopped["process_status"], "exited");
    assert_eq!(stopped["outcome"], "cancelled");
    assert!(stopped["process_id"].is_null());
}

#[test]
fn headless_final_text_and_empty_result_classification_are_truthful() {
    let state = tempfile::tempdir().expect("create isolated state directory");
    let _guard = DaemonGuard {
        state_dir: state.path(),
    };

    assert_success(command(state.path(), ["--json", "daemon", "start"]));
    assert_success(command(
        state.path(),
        [
            "--json",
            "spawn",
            "headless-ok",
            "--provider",
            "fixture",
            "--mode",
            "headless",
            "--",
            "/bin/sh",
            "-c",
            "printf 'headless-proof\\n'",
        ],
    ));
    let completed = poll_json(state.path(), ["--json", "status", "headless-ok"], |value| {
        value["process_status"] == "exited"
    });
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["outcome"], "succeeded");
    assert_eq!(completed["final_text"], "headless-proof");
    assert!(completed["final_text"].as_str().expect("final text").len() < 20 * 1024);

    assert_success(command(
        state.path(),
        [
            "--json",
            "spawn",
            "headless-empty",
            "--provider",
            "fixture",
            "--mode",
            "headless",
            "--",
            "/bin/sh",
            "-c",
            "printf 'no output produced\\n'",
        ],
    ));
    let failed = poll_json(
        state.path(),
        ["--json", "status", "headless-empty"],
        |value| value["process_status"] == "exited",
    );
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["outcome"], "failed");
    assert!(failed["final_text"].is_null());
    assert!(
        failed["provider_error"]
            .as_str()
            .is_some_and(|error| error.contains("no output produced"))
    );
}

#[test]
fn wait_wakes_on_output_and_times_out_when_unchanged() {
    let state = tempfile::tempdir().expect("create isolated state directory");
    let _guard = DaemonGuard {
        state_dir: state.path(),
    };

    assert_success(command(state.path(), ["--json", "daemon", "start"]));
    assert_success(command(
        state.path(),
        ["--json", "spawn", "waiter", "--provider", "shell"],
    ));
    let initial = json(command(state.path(), ["--json", "output", "waiter"]));
    let cursor = initial["cursor"].as_u64().expect("initial cursor");

    let mut waiting = command_builder(state.path());
    let child = waiting
        .args([
            "--json",
            "wait",
            "waiter",
            "--after",
            &cursor.to_string(),
            "--timeout-ms",
            "2000",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wait command");
    thread::sleep(Duration::from_millis(100));
    assert_success(command(
        state.path(),
        ["--json", "send", "waiter", "printf 'wait-proof\\n'"],
    ));
    let first_wake = json(child.wait_with_output().expect("wait command output"));
    assert_eq!(first_wake["changed"], true);
    assert_eq!(first_wake["timed_out"], false);

    let mut next_cursor = first_wake["output"]["cursor"]
        .as_u64()
        .expect("first wait cursor");
    let mut observed = first_wake["output"]["text"]
        .as_str()
        .is_some_and(|text| text.contains("wait-proof"));
    for _ in 0..5 {
        if observed {
            break;
        }
        let cursor = next_cursor.to_string();
        let wake = json(command(
            state.path(),
            [
                "--json",
                "wait",
                "waiter",
                "--after",
                &cursor,
                "--timeout-ms",
                "500",
            ],
        ));
        next_cursor = wake["output"]["cursor"].as_u64().expect("wait cursor");
        observed = wake["output"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("wait-proof"));
    }
    assert!(observed, "wait never delivered the expected output marker");

    thread::sleep(Duration::from_millis(100));
    let settled = json(command(state.path(), ["--json", "output", "waiter"]));
    next_cursor = settled["cursor"].as_u64().expect("settled cursor");
    let next_cursor = next_cursor.to_string();
    let timed_out = json(command(
        state.path(),
        [
            "--json",
            "wait",
            "waiter",
            "--after",
            &next_cursor,
            "--timeout-ms",
            "50",
        ],
    ));
    assert_eq!(timed_out["changed"], false);
    assert_eq!(timed_out["timed_out"], true);
    assert_success(command(state.path(), ["--json", "stop", "waiter"]));
}

#[test]
fn completed_sessions_survive_restart_and_can_be_deleted_or_pruned() {
    let state = tempfile::tempdir().expect("create isolated state directory");
    let _guard = DaemonGuard {
        state_dir: state.path(),
    };

    assert_success(command(state.path(), ["--json", "daemon", "start"]));
    spawn_fixture(state.path(), "recoverable", "printf 'persisted-proof\\n'");
    poll_json(state.path(), ["--json", "status", "recoverable"], |value| {
        value["process_status"] == "exited"
    });
    assert_success(command(state.path(), ["--json", "daemon", "stop"]));
    assert_success(command(state.path(), ["--json", "daemon", "start"]));

    let recovered = json(command(state.path(), ["--json", "status", "recoverable"]));
    assert_eq!(recovered["status"], "completed");
    assert_eq!(recovered["final_text"], "persisted-proof");
    assert!(recovered["usage"].is_null());

    let deleted = json(command(state.path(), ["--json", "delete", "recoverable"]));
    assert_eq!(deleted["deleted"]["name"], "recoverable");
    assert_failure_with(
        &command(state.path(), ["--json", "status", "recoverable"]),
        "not found",
    );

    spawn_fixture(state.path(), "prune-me", "printf 'old-proof\\n'");
    poll_json(state.path(), ["--json", "status", "prune-me"], |value| {
        value["process_status"] == "exited"
    });
    thread::sleep(Duration::from_millis(5));
    let pruned = json(command(
        state.path(),
        ["--json", "prune", "--older-than-ms", "1"],
    ));
    assert_eq!(pruned["count"], 1);
    assert_eq!(pruned["deleted"][0], "prune-me");
}

#[test]
fn crashed_daemon_reconciles_running_session_as_orphaned() {
    let state = tempfile::tempdir().expect("create isolated state directory");
    let _guard = DaemonGuard {
        state_dir: state.path(),
    };

    assert_success(command(state.path(), ["--json", "daemon", "start"]));
    let spawned = json(command(
        state.path(),
        ["--json", "spawn", "orphan", "--provider", "shell"],
    ));
    let child_pid = spawned["process_id"].as_u64().expect("child pid");
    let daemon_pid = std::fs::read_to_string(state.path().join("agentmux.pid"))
        .expect("daemon pid")
        .trim()
        .to_string();
    assert!(
        Command::new("kill")
            .args(["-9", &daemon_pid])
            .status()
            .expect("kill test daemon")
            .success()
    );
    thread::sleep(Duration::from_millis(100));
    assert_success(command(state.path(), ["--json", "daemon", "start"]));

    let orphaned = json(command(state.path(), ["--json", "status", "orphan"]));
    assert_eq!(orphaned["status"], "orphaned");
    assert_eq!(orphaned["process_status"], "orphaned");
    assert_eq!(orphaned["outcome"], "unknown");
    assert_failure_with(
        &command(state.path(), ["--json", "send", "orphan", "hello"]),
        "not running",
    );
    let _ = Command::new("kill")
        .args(["-9", &child_pid.to_string()])
        .status();
    assert_success(command(state.path(), ["--json", "delete", "orphan"]));
}

#[test]
fn output_is_redacted_and_rotated_with_cursor_gap_reporting() {
    let state = tempfile::tempdir().expect("create isolated state directory");
    let _guard = DaemonGuard {
        state_dir: state.path(),
    };

    assert_success(command(state.path(), ["--json", "daemon", "start"]));
    assert_success(command(
        state.path(),
        ["--json", "spawn", "retention", "--provider", "shell"],
    ));
    assert_success(command(
        state.path(),
        [
            "--json",
            "send",
            "retention",
            "printf 'Authorization: Bearer abc123 API_TOKEN=secret me@example.com\\n'",
        ],
    ));
    let redacted = poll_json(state.path(), ["--json", "output", "retention"], |value| {
        value["redactions_applied"] == true
    });
    for secret in ["abc123", "secret", "me@example.com"] {
        assert!(
            !redacted["text"]
                .as_str()
                .unwrap_or_default()
                .contains(secret)
        );
        assert!(
            !redacted["normalized_text"]
                .as_str()
                .unwrap_or_default()
                .contains(secret)
        );
    }
    let raw = json(command(
        state.path(),
        ["--json", "output", "retention", "--raw"],
    ));
    assert_eq!(raw["raw"], true);
    assert!(!raw["text"].as_str().unwrap_or_default().contains("abc123"));

    assert_success(command(
        state.path(),
        [
            "--json",
            "send",
            "retention",
            "i=0; while [ $i -lt 500 ]; do printf 'rotation-line-%04d-xxxxxxxxxxxxxxxx\\n' $i; i=$((i+1)); done",
        ],
    ));
    poll_json(state.path(), ["--json", "status", "retention"], |value| {
        value["output_cursor"]
            .as_u64()
            .is_some_and(|cursor| cursor > 5000)
    });
    let rotated = json(command(
        state.path(),
        ["--json", "output", "retention", "--after", "0"],
    ));
    assert_eq!(rotated["dropped_before"], true);
    assert!(rotated["after"].as_u64().is_some_and(|after| after > 0));
    for entry in std::fs::read_dir(state.path().join("sessions")).expect("session files") {
        let entry = entry.expect("session entry");
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".log") || name.ends_with(".log.1") {
            assert!(entry.metadata().expect("log metadata").len() <= 1024);
        }
    }
    assert_success(command(state.path(), ["--json", "stop", "retention"]));
}

#[test]
fn mcp_mixed_reader_stress_survives_terminal_transition_and_daemon_replacement() {
    let state = tempfile::tempdir().expect("create isolated state directory");
    let _guard = DaemonGuard {
        state_dir: state.path(),
    };

    assert_success(command(state.path(), ["--json", "daemon", "start"]));
    spawn_fixture(
        state.path(),
        "mcp-stress",
        "i=0; while [ $i -lt 120 ]; do printf 'stress-line-%03d\\n' $i; i=$((i+1)); sleep 0.005; done",
    );
    let (mut mcp, mut input, output) = start_mcp(state.path());

    let mut next_id = 2_u64;
    for _ in 0..100 {
        let mut expected = Vec::new();
        for reader in 0..16 {
            let (name, arguments) = match reader % 3 {
                0 => (
                    "agents_status",
                    serde_json::json!({ "session": "mcp-stress" }),
                ),
                1 => (
                    "agents_output",
                    serde_json::json!({
                        "session": "mcp-stress",
                        "after": 0,
                        "limit": 1024
                    }),
                ),
                _ => (
                    "agents_wait",
                    serde_json::json!({
                        "session": "mcp-stress",
                        "after": 0,
                        "limit": 1024,
                        "timeout_ms": 50
                    }),
                ),
            };
            write_mcp(
                &mut input,
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": next_id,
                    "method": "tools/call",
                    "params": { "name": name, "arguments": arguments }
                }),
            );
            expected.push(next_id);
            next_id += 1;
        }
        input.flush().expect("flush MCP request wave");
        for _ in 0..expected.len() {
            let response = read_mcp(&output);
            assert!(
                response["error"].is_null(),
                "MCP protocol error: {response}"
            );
            assert_ne!(
                response["result"]["isError"], true,
                "MCP tool error: {response}"
            );
            let id = response["id"].as_u64().expect("MCP response id");
            assert!(
                expected.contains(&id),
                "unexpected MCP response: {response}"
            );
        }
    }

    let terminal = poll_json(state.path(), ["--json", "status", "mcp-stress"], |value| {
        value["process_status"] == "exited"
    });
    assert_eq!(terminal["outcome"], "succeeded");
    assert_success(command(state.path(), ["--json", "daemon", "stop"]));

    let reconnect_started = Instant::now();
    write_mcp(
        &mut input,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": next_id,
            "method": "tools/call",
            "params": {
                "name": "agents_status",
                "arguments": { "session": "mcp-stress" }
            }
        }),
    );
    input.flush().expect("flush reconnect request");
    let reconnected = read_mcp(&output);
    assert_ne!(
        reconnected["result"]["isError"], true,
        "MCP did not reconnect: {reconnected}"
    );
    assert!(
        reconnect_started.elapsed() < Duration::from_secs(2),
        "MCP reconnect exceeded two seconds"
    );
    assert_success(command(state.path(), ["--json", "daemon", "status"]));
    close_mcp(&mut mcp, input);
}

#[test]
fn daemon_shutdown_is_bounded_while_long_poll_readers_are_active() {
    let state = tempfile::tempdir().expect("create isolated state directory");
    let _guard = DaemonGuard {
        state_dir: state.path(),
    };

    assert_success(command(state.path(), ["--json", "daemon", "start"]));
    spawn_fixture(state.path(), "shutdown-load", "sleep 30; printf 'done\\n'");
    let mut readers = Vec::new();
    for _ in 0..16 {
        readers.push(
            command_builder(state.path())
                .args([
                    "--json",
                    "wait",
                    "shutdown-load",
                    "--after",
                    "0",
                    "--timeout-ms",
                    "30000",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn active wait reader"),
        );
    }
    thread::sleep(Duration::from_millis(100));

    let started = Instant::now();
    assert_success(command(state.path(), ["--json", "daemon", "stop"]));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "graceful daemon shutdown exceeded five seconds"
    );

    for reader in readers {
        let output = reader.wait_with_output().expect("reap active wait reader");
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("connection_lost") || stderr.contains("daemon_unavailable"),
                "wait reader failed without a typed transport error: {stderr}"
            );
        }
    }
}

fn spawn_fixture(state_dir: &Path, name: &str, script: &str) {
    assert_success(command(
        state_dir,
        [
            "--json",
            "spawn",
            name,
            "--provider",
            "fixture",
            "--mode",
            "headless",
            "--",
            "/bin/sh",
            "-c",
            script,
        ],
    ));
}

fn poll_json<I, S, F>(state_dir: &Path, args: I, predicate: F) -> Value
where
    I: Clone + IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    F: Fn(&Value) -> bool,
{
    let mut last = None;
    for _ in 0..80 {
        let output = command(state_dir, args.clone());
        assert_success_ref(&output);
        let value: Value = serde_json::from_slice(&output.stdout).expect("valid JSON output");
        if predicate(&value) {
            return value;
        }
        last = Some(value);
        thread::sleep(Duration::from_millis(25));
    }
    panic!("condition was not reached; last response: {last:?}");
}

fn command<I, S>(state_dir: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    command_builder(state_dir)
        .args(args)
        .output()
        .expect("run agentmux")
}

fn command_builder(state_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentmux"));
    command
        .env("AGENTMUX_STATE_DIR", state_dir)
        .env("AGENTMUX_LOG_SEGMENT_BYTES", "1024");
    command
}

fn start_mcp(state_dir: &Path) -> (Child, ChildStdin, mpsc::Receiver<Value>) {
    let mut child = command_builder(state_dir)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn MCP server");
    let mut input = child.stdin.take().expect("MCP stdin");
    let stdout = child.stdout.take().expect("MCP stdout");
    let (responses, output) = mpsc::channel();
    thread::spawn(move || {
        let mut stdout = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match stdout.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => match serde_json::from_str(&line) {
                    Ok(response) => {
                        if responses.send(response).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
            }
        }
    });
    write_mcp(
        &mut input,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "agentmux-e2e", "version": "1" }
            }
        }),
    );
    input.flush().expect("flush MCP initialize");
    let initialized = read_mcp(&output);
    assert_eq!(initialized["id"], 1);
    write_mcp(
        &mut input,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }),
    );
    input.flush().expect("flush MCP initialized notification");
    (child, input, output)
}

fn write_mcp(input: &mut ChildStdin, request: &Value) {
    writeln!(input, "{request}").expect("write MCP request");
}

fn read_mcp(output: &mpsc::Receiver<Value>) -> Value {
    output
        .recv_timeout(Duration::from_secs(3))
        .expect("MCP response within three seconds")
}

fn close_mcp(child: &mut Child, input: ChildStdin) {
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if child.try_wait().expect("poll MCP process").is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("MCP server did not exit within two seconds after stdin closed");
}

fn json(output: Output) -> Value {
    assert_success_ref(&output);
    serde_json::from_slice(&output.stdout).expect("valid JSON output")
}

fn assert_success(output: Output) {
    assert_success_ref(&output);
}

fn assert_success_ref(output: &Output) {
    assert!(
        output.status.success(),
        "agentmux failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure_with(output: &Output, expected: &str) {
    assert!(
        !output.status.success(),
        "agentmux unexpectedly succeeded\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(expected),
        "stderr did not contain {expected:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
