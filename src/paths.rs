use anyhow::{Context, Result};
use std::{env, path::PathBuf};

pub fn state_dir() -> Result<PathBuf> {
    let path = if let Some(path) = env::var_os("AGENTMUX_STATE_DIR") {
        PathBuf::from(path)
    } else if let Some(path) = env::var_os("XDG_STATE_HOME") {
        PathBuf::from(path).join("agentmux")
    } else if let Some(path) = env::var_os("HOME") {
        PathBuf::from(path).join(".local/state/agentmux")
    } else {
        env::temp_dir().join(format!("agentmux-{}", current_uid_fallback()))
    };
    std::fs::create_dir_all(&path)
        .with_context(|| format!("create state directory {}", path.display()))?;
    Ok(path)
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("agentmux.sock"))
}

pub fn pid_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("agentmux.pid"))
}

pub fn daemon_log_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("daemon.log"))
}

fn current_uid_fallback() -> String {
    env::var("USER").unwrap_or_else(|_| "local".to_string())
}
