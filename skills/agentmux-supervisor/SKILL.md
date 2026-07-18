---
name: agentmux-supervisor
description: Supervise persistent terminal-native AI agent sessions through Agentmux. Use when work should be delegated to a long-lived agent that must remain observable and steerable across multiple turns.
---

# Agentmux supervisor

Use Agentmux when a delegated task benefits from a persistent session that can be inspected, corrected, interrupted, and resumed through follow-up messages.

## Procedure

1. Call `agents_providers` and select an installed provider or pass an explicit command.
2. Give every session a unique name, an explicit working directory, a bounded task, and concrete acceptance criteria.
3. Spawn the session with `agents_spawn`.
4. Read progress with `agents_output`. Retain the returned `cursor` and pass it back as `after` so output is never repeatedly ingested.
5. Treat terminal output as a claim, not proof. Inspect the actual diff, files, tests, or runtime state with the tools available in the parent environment.
6. If the child drifts, call `agents_send` with a precise correction and keep using the same session so its context is preserved.
7. Use `agents_interrupt` when the current operation must stop before a correction can be applied.
8. Accept completion only after the stated checks pass. Call `agents_stop` when the session is no longer needed.

## Safety

- Do not grant a child broader filesystem, credential, or deployment access than the parent task allows.
- Do not run multiple writable sessions in the same checkout unless their scopes cannot overlap.
- Prefer separate Git worktrees for concurrent implementation sessions until Agentmux provides managed worktree isolation.
- Do not flood a session with messages while it is actively producing output. Interrupt first when the correction is urgent.
- Never infer success from a child's final message alone.

## Supervision loop

```text
spawn -> read new output -> inspect real state -> correct or verify -> repeat -> stop
```

