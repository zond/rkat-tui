# rkat-tui

An interactive terminal coding harness built on the
[Meerkat](https://github.com/lukacf/meerkat) agent engine. The agent runs in a
per-workspace daemon (`rkat-tui-mob`, a durable mob member on Meerkat's
runtime-backed session path) that `rkat-tui` attaches to, starting it when
none runs; `/detach` leaves the agent working and a later `rkat-tui`
reattaches. Every tool call goes through an **auto-mode** approval
model: routine actions run without interruption, risky ones are judged by a
reasoning-blind classifier, and you are asked only after repeated blocks. The
shell runs inside an OS sandbox by default, and the keys, the commands and the
approval prompt follow Claude Code's wherever both have the feature.

## Install

rkat-tui builds with plain `cargo` on Rust 1.97 (`rust-toolchain.toml` pins
1.97.0):

```bash
cargo install --path .     # installs rkat-tui and the rkat-tui-mob daemon
cargo run -- "fix the failing test"   # or run it from the checkout
```

On Linux the sandbox needs unprivileged user namespaces; a host that cannot
confine the shell refuses to start and explains how to enable them (for
example an AppArmor profile on Ubuntu 24.04 and later). macOS uses Seatbelt;
other platforms have no sandbox. `--no-confinement` runs the shell
unsandboxed.

## Quick start

Set a provider key first: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or
`GEMINI_API_KEY` (also `GOOGLE_API_KEY`; each can be given as
`RKAT_<NAME>` too). A key in the realm config or the `rkat` token store also
works, and other servers are added with `--endpoint-def` or
`[endpoints.<name>]` in `tui.toml` (see
[Models, endpoints and cost](docs/manual.md#models-endpoints-and-cost)).

```bash
rkat-tui "fix the failing test"
rkat-tui -c                       # continue the latest session of this workspace
rkat-tui --resume ~2
rkat-tui --model llama3:8b        # bare ids are looked up in every endpoint's model list
rkat-tui --allow 'shell=cargo nextest run*' --set auto.compact=auto
```

### The workspace daemon

`rkat-tui-mob` hosts the agent as a long-lived Meerkat mob member instead of
inside the terminal UI, and serves Meerkat's own JSON-RPC methods for the
workspace to any number of clients, which can disconnect and come back while
it works. It listens on loopback TCP and writes the address and an access
token to `~/.local/state/rkat-tui/<workspace key>/mob.endpoint` (outside
the workspace, where the agent's confined shell cannot read it); every
connection must first send `rkat/attach` with that token. It takes the same engine flags as `rkat-tui`:

```bash
rkat-tui-mob --foreground -w . --model claude-sonnet-4-6
```

`rkat-tui` runs the TUI against it, starting it when none answers;
`/detach` leaves it running, `/quit` stops a daemon this TUI started (else
it detaches or asks) and `/daemon stop` shuts it down. `--attach=DIRECTORY`
attaches to another workspace's daemon, `--attach=HOST:PORT` to one through
a tunnel, and `--no-attach` hosts the agent in-process instead. The agent works behind the same
approval gate, with its helpers, `/new`, `/resume` and the model picker; see
[Attach mode](docs/manual.md#the-workspace-daemon-attach-mode) and
[`docs/mob-split.md`](docs/mob-split.md).

## Highlights

- **Sessions that survive restarts.** The transcript, the model and the cost
  totals persist; `-c` continues the workspace's latest session, `--resume`
  or `/resume` any other, and Enter while the agent works steers the running
  turn.
- **Auto-mode approval**, first match wins:
  - read-only tools and commands and narrow allow rules run;
  - in-project edits outside protected paths run;
  - everything else (other shell commands, MCP tools, out-of-project paths)
    goes to the classifier, and after 3 consecutive or 20 total blocks you
    are asked.
- **Permission modes.** Shift+Tab cycles manual → accept edits → plan → auto,
  as in Claude Code; plan mode is read-only.
- **Sandboxed shell.** The agent's shell (and your own `!` commands) runs in
  an empty private mount view holding only the workspace and a few granted
  directories, with a curated environment that keeps provider keys out;
  `strict` also denies the network.
- **Helpers and `fork`.** The agent can `delegate` subtasks to helper agents
  (Meerkat mob members) or `fork` a side quest with the conversation as
  context (attached to the daemon, the engine's `fork_off`); their calls pass the same gate, and a panel shows what they do.
- **Any endpoint.** `anthropic`, `openai` (also LiteLLM and other
  OpenAI-compatible proxies) and `gemini` dialects, `/model` to hot-swap
  between them, and a context gauge and cost estimate in the status bar.
- **Instructions and memory.** `AGENTS.md` and `SYSTEM.md` files,
  `/remember`, notes the agent keeps across sessions, and Meerkat's semantic
  memory over compacted history.
- **`/rewind`** forks the session just before an earlier message, leaving
  the previous session intact.

## Documentation

- [`docs/manual.md`](docs/manual.md): the user manual (sessions, approval,
  confinement, helpers, rules and `tui.toml`, endpoints, memory, keys and
  commands, where state lives).
- [`docs/testing.md`](docs/testing.md): the test protocol.
- [`docs/ux-parity.md`](docs/ux-parity.md): the Claude Code parity audit.
- [`CHANGELOG.md`](CHANGELOG.md).

## Status

Early, and moving fast. The engine is the crates.io release of Meerkat
0.9.11. The agent runs in the `rkat-tui-mob` daemon by default; the
in-process runner (`--no-attach`) is kept for one release as the escape
hatch.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
