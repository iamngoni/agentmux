use anyhow::{Result, bail};
use serde::Serialize;
use std::{env, path::PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub name: &'static str,
    pub command: &'static str,
    pub installed: bool,
    pub integration: &'static str,
}

const PROVIDERS: [(&str, &str); 7] = [
    ("shell", "/bin/sh"),
    ("claude", "claude"),
    ("codex", "codex"),
    ("grok", "grok"),
    ("kimi", "kimi"),
    ("antigravity", "agy"),
    ("gemini", "agy"),
];

pub fn list() -> Vec<ProviderInfo> {
    PROVIDERS
        .iter()
        .map(|(name, command)| ProviderInfo {
            name,
            command,
            installed: command_exists(command),
            integration: "pty",
        })
        .collect()
}

pub fn resolve(provider: &str, explicit_command: &[String]) -> Result<Vec<String>> {
    if !explicit_command.is_empty() {
        return Ok(explicit_command.to_vec());
    }

    let command = PROVIDERS
        .iter()
        .find_map(|(name, command)| (*name == provider).then_some(*command))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown provider '{provider}'; pass a command after -- for a custom provider"
            )
        })?;

    if !command_exists(command) {
        bail!("provider '{provider}' is not installed (expected command '{command}')");
    }

    Ok(vec![command.to_string()])
}

fn command_exists(command: &str) -> bool {
    let candidate = PathBuf::from(command);
    if candidate.components().count() > 1 {
        return candidate.is_file();
    }

    env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|dir| dir.join(command).is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_command_supports_unknown_providers() {
        let command = vec!["my-agent".to_string(), "--interactive".to_string()];
        assert_eq!(resolve("future-agent", &command).unwrap(), command);
    }

    #[test]
    fn unknown_provider_without_command_is_rejected() {
        let error = resolve("future-agent", &[]).unwrap_err().to_string();
        assert!(error.contains("unknown provider"));
    }

    #[test]
    fn antigravity_and_gemini_use_the_agy_cli() {
        let providers = list();
        for provider_name in ["antigravity", "gemini"] {
            let provider = providers
                .iter()
                .find(|provider| provider.name == provider_name)
                .expect("provider preset");
            assert_eq!(provider.command, "agy");
        }
    }

    #[test]
    fn kimi_uses_the_official_kimi_command() {
        let providers = list();
        let kimi = providers
            .iter()
            .find(|provider| provider.name == "kimi")
            .expect("Kimi provider preset");
        assert_eq!(kimi.command, "kimi");
    }
}
