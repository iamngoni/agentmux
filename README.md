# Agentmux

Persistent, observable, steerable sessions for AI agents.

Agentmux is the local control plane behind the idea of “tmux for AI agents.” A daemon owns provider processes and pseudo-terminals (PTYs); a CLI and MCP server operate those same sessions through a small Unix-socket protocol.

This repository currently contains an early vertical slice for macOS and Linux. It supports safe headless tasks, long-running interactive sessions, incremental observation, semantic steering, raw terminal control, and truthful process/task outcomes. It does not yet provide daemon-restart recovery, Git worktree isolation, native SDK/ACP integrations, terminal screen reconstruction, log rotation, long-poll subscriptions, or SQLite persistence.

## Supported agent CLIs

Agentmux can currently offload work to these CLI harnesses. A supplied prompt defaults to the provider's concise headless mode and the `read_only` safety profile; omit the prompt or pass `--mode interactive` to keep a steerable TUI session.

| Provider | Command | Harness | Headless automation preset |
| --- | --- | --- | --- |
| `claude` | `claude` | Claude Code | Print mode, plan permissions, no session persistence |
| `codex` | `codex` | OpenAI Codex CLI | `exec`, read-only sandbox, ephemeral, no color |
| `grok` | `grok` | Grok Build | Single-turn plain output, plan permissions, read-only sandbox |
| `kimi` | `kimi` | Kimi Code CLI | Prompt mode, text output, provider `auto` policy |
| `antigravity` | `agy` | Antigravity CLI | Print mode, plan mode, sandboxed auto-approval |
| `gemini` | `agy` | Alias of `antigravity` | Same Antigravity adapter |
| `shell` | `/bin/sh` | Generic interactive shell | Interactive only |

Run `agentmux providers` to see which harnesses are installed on the current machine. Any other terminal-native agent can use the generic adapter by supplying its command after `--`:

```bash
agentmux spawn custom-worker \
  --provider custom-agent \
  --cwd . \
  -- custom-agent --interactive
```

`agentmux providers` reports each harness's installed version, alias relationship, integration modes, and capabilities. Authentication is currently reported as `unknown` because no provider exposes one common, reliable auth probe.

Headless presets use centrally defined `read_only` or `workspace_write` arguments. Read-only is the default; request `--safety workspace-write` only when the delegated task must change the checkout. Antigravity's current print mode needs auto-approval to produce a result, so Agentmux only enables it together with `agy --sandbox` and exposes the resulting profile with a `_sandbox` suffix.

Kimi 0.27's prompt mode is the exception: its CLI rejects `--prompt` with `--plan` and documents that non-interactive prompts use Kimi's `auto` permission policy. Agentmux therefore rejects a false `read_only` headless claim. Use `--safety provider_default` explicitly for a one-turn Kimi task, or `--mode interactive --safety read_only` to start Kimi in plan mode with semantic steering.

## Build

```bash
cargo build
```

## Try it

The daemon starts automatically when a client command needs it.

```bash
cargo run -- daemon start
cargo run -- providers

cargo run -- spawn demo --provider shell
cargo run -- send demo 'printf "hello from the child\\n"'
cargo run -- output demo
cargo run -- send demo 'exit'
cargo run -- status demo
```

Launch a built-in agent preset:

```bash
agentmux spawn backend \
  --provider codex \
  --cwd ./server \
  --safety workspace-write \
  --prompt 'Implement the health endpoint and tests.'
```

That command is a one-turn headless task with write access limited by Codex's workspace sandbox. Omit `--safety workspace-write` for review, research, and other read-only work. Final status contains separate `process_status`, `outcome`, `final_text`, and `provider_error` fields. Exit code zero with an empty result or `no output produced` is classified as failure.

Use interactive mode when the same harness must accept follow-up steering:

```bash
agentmux spawn backend-live \
  --provider codex \
  --mode interactive \
  --cwd ./server \
  --prompt 'Inspect the authentication implementation.'

agentmux send backend-live 'Now focus only on the token refresh path.'
```

`send` is semantic: it submits exactly one message using a terminal Enter (`CR`). Terminal controls are separate:

```bash
agentmux key backend-live escape
agentmux key backend-live ctrl_c
agentmux input backend-live cHJpbnRmICJoaVxuIg0=
```

`input` accepts standard-base64 raw PTY bytes. Prefer `send` unless a harness is displaying a startup, trust, approval, or other terminal-native dialog.

Launch any terminal-native agent without changing Agentmux:

```bash
agentmux spawn reviewer \
  --provider my-agent \
  --cwd . \
  --prompt 'Review the current diff.' \
  -- my-agent --interactive
```

Read only new terminal output by retaining the returned byte cursor:

```bash
agentmux --json output backend --after 0
agentmux --json output backend --after 18420
```

Useful lifecycle commands:

```text
agentmux list
agentmux status <session>
agentmux send <session> <message>
agentmux key <session> <enter|escape|ctrl_c|up|down>
agentmux input <session> <base64>
agentmux output <session> --after <cursor>
agentmux interrupt <session>
agentmux stop <session>
agentmux daemon stop
```

Set `AGENTMUX_STATE_DIR` to override the default state directory at `~/.local/state/agentmux`.

## MCP

Configure an MCP client to run:

```bash
agentmux mcp
```

The stdio server exposes:

```text
agents_providers
agents_spawn
agents_list
agents_status
agents_send
agents_key
agents_input
agents_output
agents_interrupt
agents_stop
```

All tools call the same local daemon used by the CLI. `agents_interrupt` confirms only that Ctrl-C was delivered; it does not claim that a turn was cancelled. `agents_stop` waits until the child process is observed gone before returning. The bundled [`agentmux-supervisor` skill](skills/agentmux-supervisor/SKILL.md) teaches a parent agent to choose headless or interactive mode, poll incrementally, verify actual workspace state, and steer a child until acceptance criteria pass.

## Architecture

```text
CLI ---------+
             +--> Unix socket --> daemon --> headless/PTY provider adapters --> agent CLIs
MCP stdio ---+
```

Use an explicit command after `--` when provider-specific flags or a custom harness are required. Add `--mode headless` to classify that command by final result, or leave it interactive for PTY steering. Native structured drivers such as Kimi's ACP mode can be added later without changing the CLI/MCP contract.
