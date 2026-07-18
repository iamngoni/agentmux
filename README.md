# Agentmux

Persistent, observable, steerable terminal sessions for AI agents.

Agentmux is the local control plane behind the idea of “tmux for AI agents.” A daemon owns long-lived pseudo-terminals (PTYs); a CLI and MCP server operate those same sessions through a small Unix-socket protocol.

This repository currently contains an early vertical slice for macOS and Linux. It proves session ownership, incremental observation, and mid-session steering. It does not yet provide daemon-restart recovery, Git worktree isolation, native provider protocols, terminal screen emulation, or SQLite persistence.

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
  --prompt 'Implement the health endpoint and tests.'
```

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
agents_output
agents_interrupt
agents_stop
```

All tools call the same local daemon used by the CLI. The bundled [`agentmux-supervisor` skill](skills/agentmux-supervisor/SKILL.md) teaches a parent agent to poll incrementally, verify actual workspace state, and steer a child until acceptance criteria pass.

## Architecture

```text
CLI ---------+
             +--> Unix socket --> daemon --> PTY sessions --> agent CLIs
MCP stdio ---+
```

Provider presets are intentionally thin. `claude`, `codex`, `grok`, and `kimi` launch their matching commands through the generic PTY adapter. `antigravity` and `gemini` both launch the current Antigravity CLI command, `agy`; use an explicit `agy --agent ...` command when a specific Antigravity-hosted model is required. Native structured drivers such as Kimi's ACP mode can be added later without changing the CLI/MCP contract.
