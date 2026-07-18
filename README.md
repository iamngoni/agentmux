# Agentmux

Persistent, observable, steerable sessions for AI agents.

Agentmux is the local control plane behind the idea of “tmux for AI agents.” A daemon owns provider processes and pseudo-terminals (PTYs); a CLI and MCP server operate those same sessions through a small Unix-socket protocol.

This repository currently contains an early vertical slice for macOS and Linux. It supports safe headless tasks, long-running interactive sessions, incremental observation, semantic steering, raw terminal control, truthful process/task outcomes, durable completed-session metadata, terminal screen reconstruction, bounded logs, and long-poll waits. A daemon restart reloads completed sessions and explicitly marks formerly running sessions as `orphaned` because PTY ownership cannot be recovered safely.

Agentmux does not yet provide Git worktree isolation, native SDK/ACP integrations, reconnectable live PTYs, or SQLite persistence. Versioned JSON sidecars are the current durable store.

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

Headless presets use centrally defined `read_only` or `workspace_write` arguments. Read-only is the default; request `--safety workspace-write` only when the delegated task must change the checkout. Antigravity's current non-interactive print mode needs approval bypass to produce a result. Agentmux keeps `agy` plan mode and its sandbox as the enforcement boundary, reports the effective profile as `read_only_sandbox_auto_approve` or `workspace_write_sandbox_auto_approve`, and exposes a provider safety note instead of implying that approval prompts remain active.

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

Read only new terminal output by retaining the returned logical byte cursor:

```bash
agentmux --json output backend --after 0
agentmux --json output backend --after 18420
```

Output is normalized and redacted by default. It also includes a reconstructed `screen_text` snapshot for full-screen TUIs. Use `--raw` only when ANSI/control details are needed; raw reads are still redacted. If bounded-log rotation has discarded the requested cursor, `dropped_before` is true and the response begins at the oldest retained byte.

Long-poll instead of repeatedly polling:

```bash
agentmux --json wait backend --after 18420 --timeout-ms 10000
```

`wait` wakes when output or lifecycle state changes and caps each wait at 30 seconds. Advance to the returned `output.cursor`; a wake can represent a state transition before new output arrives.

Useful lifecycle commands:

```text
agentmux list
agentmux status <session>
agentmux send <session> <message>
agentmux key <session> <enter|escape|ctrl_c|up|down>
agentmux input <session> <base64>
agentmux output <session> --after <cursor>
agentmux wait <session> --after <cursor> --timeout-ms <milliseconds>
agentmux interrupt <session>
agentmux stop <session>
agentmux delete <finished-or-orphaned-session>
agentmux prune --older-than-ms <milliseconds>
agentmux daemon stop
```

Daemon shutdown acknowledges first, stops accepting new requests, terminates live sessions in parallel, and waits up to five seconds for the exact daemon to exit. If it returns `daemon_shutdown_timeout`, read the exact PID from the path in the error and terminate only that process after verifying it still belongs to Agentmux.

Set `AGENTMUX_STATE_DIR` to override the default state directory at `~/.local/state/agentmux`. Session metadata, reconstructed screens, and two bounded log segments live under `sessions/`. Files are owner-only; returned output and persisted final text apply best-effort token, credential, and account-identifier redaction. Token/cost usage fields stay `null` until a provider adapter can report them truthfully.

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
agents_wait
agents_interrupt
agents_stop
agents_delete
agents_prune
```

All tools call the same local daemon used by the CLI. Each request uses a fresh bounded Unix-socket connection, so a long-running MCP process automatically reaches a replacement daemon on its next call. Concurrent MCP work is admission-limited; overload returns `server_busy` instead of hanging. Transport failures use stable prefixes including `daemon_unavailable`, `daemon_timeout`, and `connection_lost`. Read-only requests may reconnect once; mutations are never replayed after an ambiguous delivery.

Prefer `agents_wait` with the last output cursor for efficient supervision. `agents_interrupt` confirms only that Ctrl-C was delivered; it does not claim that a turn was cancelled. `agents_stop` waits until the child process is observed gone before returning. `agents_delete` refuses to remove a live session; `agents_prune` only removes non-running sessions older than the requested retention period. The bundled [`agentmux-supervisor` skill](skills/agentmux-supervisor/SKILL.md) teaches a parent agent to choose headless or interactive mode, wait incrementally, verify actual workspace state, and steer a child until acceptance criteria pass.

## Architecture

```text
CLI ---------+
             +--> Unix socket --> daemon --> headless/PTY provider adapters --> agent CLIs
MCP stdio ---+
```

Use an explicit command after `--` when provider-specific flags or a custom harness are required. Add `--mode headless` to classify that command by final result, or leave it interactive for PTY steering. Native structured drivers such as Kimi's ACP mode can be added later without changing the CLI/MCP contract.
