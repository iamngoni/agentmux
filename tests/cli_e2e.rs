#![cfg(unix)]

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use std::{
    ffi::OsStr,
    path::Path,
    process::{Command, Output},
    thread,
    time::Duration,
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
    Command::new(env!("CARGO_BIN_EXE_agentmux"))
        .args(args)
        .env("AGENTMUX_STATE_DIR", state_dir)
        .output()
        .expect("run agentmux")
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
