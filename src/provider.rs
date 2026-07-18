use crate::protocol::{SafetyProfile, SpawnMode};
use anyhow::{Result, bail};
use serde::Serialize;
use std::{env, path::PathBuf, process::Command};

#[derive(Debug, Clone, Serialize)]
pub struct ProviderCapabilities {
    pub interactive: bool,
    pub headless: bool,
    pub semantic_messages: bool,
    pub raw_input: bool,
    pub interactive_safety_profiles: Vec<&'static str>,
    pub headless_safety_profiles: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub name: &'static str,
    pub command: &'static str,
    pub installed: bool,
    pub version: Option<String>,
    pub auth_state: &'static str,
    pub integration: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias_of: Option<&'static str>,
    pub capabilities: ProviderCapabilities,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchMode {
    Interactive,
    Headless,
}

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub command: Vec<String>,
    pub mode: LaunchMode,
    pub safety_profile: &'static str,
    pub initial_input: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct ProviderDescriptor {
    name: &'static str,
    command: &'static str,
    alias_of: Option<&'static str>,
    headless: bool,
}

const PROVIDERS: [ProviderDescriptor; 7] = [
    ProviderDescriptor {
        name: "shell",
        command: "/bin/sh",
        alias_of: None,
        headless: false,
    },
    ProviderDescriptor {
        name: "claude",
        command: "claude",
        alias_of: None,
        headless: true,
    },
    ProviderDescriptor {
        name: "codex",
        command: "codex",
        alias_of: None,
        headless: true,
    },
    ProviderDescriptor {
        name: "grok",
        command: "grok",
        alias_of: None,
        headless: true,
    },
    ProviderDescriptor {
        name: "kimi",
        command: "kimi",
        alias_of: None,
        headless: true,
    },
    ProviderDescriptor {
        name: "antigravity",
        command: "agy",
        alias_of: None,
        headless: true,
    },
    ProviderDescriptor {
        name: "gemini",
        command: "agy",
        alias_of: Some("antigravity"),
        headless: true,
    },
];

pub fn list() -> Vec<ProviderInfo> {
    PROVIDERS
        .iter()
        .map(|provider| {
            let installed = command_exists(provider.command);
            ProviderInfo {
                name: provider.name,
                command: provider.command,
                installed,
                version: installed
                    .then(|| command_version(provider.command))
                    .flatten(),
                auth_state: "unknown",
                integration: if provider.headless {
                    "headless_and_pty"
                } else {
                    "pty"
                },
                alias_of: provider.alias_of,
                capabilities: ProviderCapabilities {
                    interactive: true,
                    headless: provider.headless,
                    semantic_messages: true,
                    raw_input: true,
                    interactive_safety_profiles: if provider.name == "kimi" {
                        vec!["read_only", "workspace_write", "provider_default"]
                    } else {
                        vec!["provider_default"]
                    },
                    headless_safety_profiles: match provider.name {
                        "kimi" => vec!["provider_default_auto"],
                        "shell" => vec![],
                        _ => vec!["read_only", "workspace_write"],
                    },
                },
            }
        })
        .collect()
}

pub fn resolve(
    provider_name: &str,
    explicit_command: &[String],
    prompt: Option<&str>,
    requested_mode: SpawnMode,
    safety: SafetyProfile,
) -> Result<LaunchSpec> {
    if !explicit_command.is_empty() {
        let mode = match requested_mode {
            SpawnMode::Auto | SpawnMode::Interactive => LaunchMode::Interactive,
            SpawnMode::Headless => LaunchMode::Headless,
        };
        let mut command = explicit_command.to_vec();
        let initial_input = match (mode, prompt) {
            (LaunchMode::Interactive, prompt) => prompt.map(ToOwned::to_owned),
            (LaunchMode::Headless, Some(prompt)) => {
                command.push(prompt.to_string());
                None
            }
            (LaunchMode::Headless, None) => None,
        };
        return Ok(LaunchSpec {
            command,
            mode,
            safety_profile: "unmanaged_explicit",
            initial_input,
        });
    }

    let descriptor = PROVIDERS
        .iter()
        .find(|provider| provider.name == provider_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown provider '{provider_name}'; pass a command after -- for a custom provider"
            )
        })?;

    if !command_exists(descriptor.command) {
        bail!(
            "provider '{provider_name}' is not installed (expected command '{}')",
            descriptor.command
        );
    }

    build_launch(*descriptor, prompt, requested_mode, safety)
}

fn build_launch(
    descriptor: ProviderDescriptor,
    prompt: Option<&str>,
    requested_mode: SpawnMode,
    safety: SafetyProfile,
) -> Result<LaunchSpec> {
    let mode = match requested_mode {
        SpawnMode::Auto if prompt.is_some() && descriptor.headless => LaunchMode::Headless,
        SpawnMode::Auto | SpawnMode::Interactive => LaunchMode::Interactive,
        SpawnMode::Headless if !descriptor.headless => {
            bail!("provider '{}' has no headless adapter", descriptor.name)
        }
        SpawnMode::Headless => LaunchMode::Headless,
    };

    if mode == LaunchMode::Headless && prompt.is_none() {
        bail!("headless mode requires a prompt");
    }
    if mode == LaunchMode::Headless {
        if descriptor.name == "kimi" && safety != SafetyProfile::ProviderDefault {
            bail!(
                "Kimi prompt mode cannot enforce '{safety:?}'; use --safety provider_default for headless mode or --mode interactive --safety read_only for Kimi plan mode"
            );
        }
        if descriptor.name != "kimi" && safety == SafetyProfile::ProviderDefault {
            bail!(
                "provider_default headless safety is currently supported only by Kimi; choose read_only or workspace_write"
            );
        }
    }

    let prompt = prompt.unwrap_or_default();
    let safety_profile = match safety {
        SafetyProfile::ReadOnly => "read_only",
        SafetyProfile::WorkspaceWrite => "workspace_write",
        SafetyProfile::ProviderDefault => "provider_default",
    };
    let (command, initial_input, safety_profile) = match (descriptor.name, mode) {
        ("claude", LaunchMode::Headless) => (
            strings(&[
                "claude",
                "-p",
                "--output-format",
                "text",
                "--permission-mode",
                match safety {
                    SafetyProfile::ReadOnly => "plan",
                    SafetyProfile::WorkspaceWrite => "acceptEdits",
                    SafetyProfile::ProviderDefault => unreachable!(),
                },
                "--no-session-persistence",
                prompt,
            ]),
            None,
            safety_profile,
        ),
        ("codex", LaunchMode::Headless) => (
            strings(&[
                "codex",
                "exec",
                "--sandbox",
                match safety {
                    SafetyProfile::ReadOnly => "read-only",
                    SafetyProfile::WorkspaceWrite => "workspace-write",
                    SafetyProfile::ProviderDefault => unreachable!(),
                },
                "--ephemeral",
                "--color",
                "never",
                "--skip-git-repo-check",
                prompt,
            ]),
            None,
            safety_profile,
        ),
        ("grok", LaunchMode::Headless) => (
            strings(&[
                "grok",
                "-p",
                prompt,
                "--output-format",
                "plain",
                "--permission-mode",
                match safety {
                    SafetyProfile::ReadOnly => "plan",
                    SafetyProfile::WorkspaceWrite => "acceptEdits",
                    SafetyProfile::ProviderDefault => unreachable!(),
                },
                "--sandbox",
                match safety {
                    SafetyProfile::ReadOnly => "read-only",
                    SafetyProfile::WorkspaceWrite => "workspace-write",
                    SafetyProfile::ProviderDefault => unreachable!(),
                },
            ]),
            None,
            safety_profile,
        ),
        ("kimi", LaunchMode::Headless) => (
            strings(&["kimi", "-p", prompt, "--output-format", "text"]),
            None,
            "provider_default_auto",
        ),
        ("antigravity" | "gemini", LaunchMode::Headless) => (
            strings(&[
                "agy",
                "--mode",
                match safety {
                    SafetyProfile::ReadOnly => "plan",
                    SafetyProfile::WorkspaceWrite => "accept-edits",
                    SafetyProfile::ProviderDefault => unreachable!(),
                },
                "--sandbox",
                "--dangerously-skip-permissions",
                "--print",
                prompt,
            ]),
            None,
            match safety {
                SafetyProfile::ReadOnly => "read_only_sandbox",
                SafetyProfile::WorkspaceWrite => "workspace_write_sandbox",
                SafetyProfile::ProviderDefault => unreachable!(),
            },
        ),
        ("claude", LaunchMode::Interactive) => (
            with_optional_prompt("claude", prompt),
            None,
            "provider_default",
        ),
        ("codex", LaunchMode::Interactive) => (
            with_optional_prompt("codex", prompt),
            None,
            "provider_default",
        ),
        ("grok", LaunchMode::Interactive) => (
            with_optional_prompt("grok", prompt),
            None,
            "provider_default",
        ),
        ("antigravity" | "gemini", LaunchMode::Interactive) if !prompt.is_empty() => (
            strings(&["agy", "--prompt-interactive", prompt]),
            None,
            "provider_default",
        ),
        ("kimi", LaunchMode::Interactive) => {
            let command = match safety {
                SafetyProfile::ReadOnly => strings(&["kimi", "--plan"]),
                SafetyProfile::WorkspaceWrite => strings(&["kimi", "--auto"]),
                SafetyProfile::ProviderDefault => vec!["kimi".to_string()],
            };
            (
                command,
                (!prompt.is_empty()).then(|| prompt.to_string()),
                safety_profile,
            )
        }
        (_, LaunchMode::Interactive) => (
            vec![descriptor.command.to_string()],
            (!prompt.is_empty()).then(|| prompt.to_string()),
            "provider_default",
        ),
        (_, LaunchMode::Headless) => unreachable!("all headless adapters are covered"),
    };

    Ok(LaunchSpec {
        command,
        mode,
        safety_profile,
        initial_input,
    })
}

fn with_optional_prompt(command: &str, prompt: &str) -> Vec<String> {
    if prompt.is_empty() {
        vec![command.to_string()]
    } else {
        strings(&[command, prompt])
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn command_version(command: &str) -> Option<String> {
    if command.starts_with('/') {
        return None;
    }
    let output = Command::new(command).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
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

    fn descriptor(name: &str) -> ProviderDescriptor {
        *PROVIDERS
            .iter()
            .find(|provider| provider.name == name)
            .expect("provider descriptor")
    }

    #[test]
    fn explicit_command_supports_unknown_providers() {
        let command = vec!["my-agent".to_string(), "--interactive".to_string()];
        let launch = resolve(
            "future-agent",
            &command,
            None,
            SpawnMode::Auto,
            SafetyProfile::ReadOnly,
        )
        .unwrap();
        assert_eq!(launch.command, command);
        assert_eq!(launch.mode, LaunchMode::Interactive);
    }

    #[test]
    fn unknown_provider_without_command_is_rejected() {
        let error = resolve(
            "future-agent",
            &[],
            None,
            SpawnMode::Auto,
            SafetyProfile::ReadOnly,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown provider"));
    }

    #[test]
    fn prompt_defaults_to_safe_headless_adapter() {
        let launch = build_launch(
            descriptor("grok"),
            Some("inspect only"),
            SpawnMode::Auto,
            SafetyProfile::ReadOnly,
        )
        .expect("headless launch");
        assert_eq!(launch.mode, LaunchMode::Headless);
        assert_eq!(launch.safety_profile, "read_only");
        assert_eq!(
            launch.command,
            strings(&[
                "grok",
                "-p",
                "inspect only",
                "--output-format",
                "plain",
                "--permission-mode",
                "plan",
                "--sandbox",
                "read-only",
            ])
        );
        assert!(launch.initial_input.is_none());
    }

    #[test]
    fn antigravity_flags_precede_print_prompt() {
        let launch = build_launch(
            descriptor("antigravity"),
            Some("inspect only"),
            SpawnMode::Headless,
            SafetyProfile::ReadOnly,
        )
        .expect("headless launch");
        assert_eq!(
            launch.command,
            strings(&[
                "agy",
                "--mode",
                "plan",
                "--sandbox",
                "--dangerously-skip-permissions",
                "--print",
                "inspect only",
            ])
        );
    }

    #[test]
    fn workspace_write_is_an_explicit_central_profile() {
        let launch = build_launch(
            descriptor("codex"),
            Some("implement this"),
            SpawnMode::Headless,
            SafetyProfile::WorkspaceWrite,
        )
        .expect("workspace-write launch");
        assert_eq!(launch.safety_profile, "workspace_write");
        assert!(
            launch
                .command
                .windows(2)
                .any(|pair| pair == ["--sandbox", "workspace-write"])
        );
    }

    #[test]
    fn interactive_prompt_is_passed_at_launch_when_supported() {
        let launch = build_launch(
            descriptor("claude"),
            Some("inspect only"),
            SpawnMode::Interactive,
            SafetyProfile::ReadOnly,
        )
        .expect("interactive launch");
        assert_eq!(launch.command, strings(&["claude", "inspect only"]));
        assert!(launch.initial_input.is_none());
    }

    #[test]
    fn gemini_is_an_antigravity_alias() {
        let providers = list();
        let gemini = providers
            .iter()
            .find(|provider| provider.name == "gemini")
            .expect("Gemini provider preset");
        assert_eq!(gemini.command, "agy");
        assert_eq!(gemini.alias_of, Some("antigravity"));
    }

    #[test]
    fn kimi_headless_uses_its_documented_prompt_policy() {
        let launch = build_launch(
            descriptor("kimi"),
            Some("inspect only"),
            SpawnMode::Auto,
            SafetyProfile::ProviderDefault,
        )
        .expect("Kimi headless launch");
        assert_eq!(
            launch.command,
            strings(&["kimi", "-p", "inspect only", "--output-format", "text",])
        );
        assert_eq!(launch.safety_profile, "provider_default_auto");
    }

    #[test]
    fn kimi_rejects_a_false_headless_read_only_claim() {
        let error = build_launch(
            descriptor("kimi"),
            Some("inspect only"),
            SpawnMode::Auto,
            SafetyProfile::ReadOnly,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("cannot enforce"));
    }
}
