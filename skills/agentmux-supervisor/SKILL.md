---
name: agentmux-supervisor
description: Supervise persistent terminal-native AI agent sessions through Agentmux. Use when work should be delegated to a long-lived agent that must remain observable and steerable across multiple turns.
---

# Agentmux supervisor

Use Agentmux when a delegated task benefits from a persistent session that can be inspected, corrected, interrupted, and resumed through follow-up messages.

## Procedure

1. Call `agents_providers` and select an installed provider or pass an explicit command. Inspect version, alias, headless support, and safety metadata.
2. Give every session a unique name, an explicit working directory, a bounded task, and concrete acceptance criteria.
3. Choose the launch mode deliberately:
   - Omit `mode` (or use `auto`) with a prompt for a safe, concise, one-turn headless task.
   - Use `mode: interactive` when the same live session must accept follow-up steering.
   - Keep the default `safety: read_only` for inspection and review. Use `safety: workspace_write` only when the assigned task must modify the checkout.
   - Kimi headless prompt mode cannot enforce read-only mode. Use `safety: provider_default` only when Kimi's documented non-interactive `auto` policy is acceptable; otherwise use Kimi with `mode: interactive, safety: read_only`.
4. Spawn the session with `agents_spawn`.
5. Read progress with `agents_output`. Retain the returned `cursor` and pass it back as `after` so output is never repeatedly ingested. Prefer `normalized_text`; fetch raw `text` only when terminal details matter.
6. For a finished headless task, inspect `outcome`, `final_text`, and `provider_error`. Never infer task success from process exit code alone.
7. Treat provider output as a claim, not proof. Inspect the actual diff, files, tests, or runtime state with the tools available in the parent environment.
8. In interactive mode, call `agents_send` with a precise correction. It submits one semantic message; never embed control characters in the message.
9. Use `agents_key` for Enter, Escape, Ctrl-C, or arrow keys and `agents_input` only for raw base64-encoded PTY input.
10. Use `agents_interrupt` when the current operation must stop before a correction can be applied. Its response confirms signal delivery, not observed turn cancellation.
11. Accept completion only after the stated checks pass. Call `agents_stop` when the session is no longer needed; it returns only after the process is observed gone.

## Safety

- Do not grant a child broader filesystem, credential, or deployment access than the parent task allows.
- Do not run multiple writable sessions in the same checkout unless their scopes cannot overlap.
- Prefer separate Git worktrees for concurrent implementation sessions until Agentmux provides managed worktree isolation.
- Do not flood a session with messages while it is actively producing output. Interrupt first when the correction is urgent.
- Never infer success from a child's final message alone.
- Do not treat `signal_delivered: true` as proof that an interactive turn stopped. Re-read session state and output.
- A headless session is intentionally one-turn and cannot accept `agents_send`; use interactive mode when follow-up context is required.

## Supervision loop

```text
spawn -> read new output -> inspect real state -> correct or verify -> repeat -> stop
```
