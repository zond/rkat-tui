# rkat-tui

An interactive terminal coding harness built on the
[Meerkat](https://github.com/lukacf/meerkat) agent engine. It hosts its
sessions in-process on Meerkat's runtime-backed session path (the one `rkat`,
REST and RPC use) and gates every tool call through an **auto-mode** approval
model: routine actions run without interruption, risky ones are judged by a
reasoning-blind classifier, and you are asked only after repeated blocks. The
shell runs inside an OS sandbox by default, and the keys, the commands and the
approval prompt follow Claude Code's wherever both have the feature.

## Install

rkat-tui builds with plain `cargo` on Rust 1.97 (`rust-toolchain.toml` pins
1.97.0):

```bash
cargo install --path .     # installs the rkat-tui binary
cargo run -- "fix the failing test"   # or run it from the checkout
```

On Linux the sandbox needs unprivileged user namespaces; a host that cannot
confine the shell refuses to start and explains how to enable them (for
example an AppArmor profile on Ubuntu 24.04 and later). macOS uses Seatbelt;
other platforms have no sandbox. `--no-confinement` runs the shell
unsandboxed.

## Quick start

```bash
rkat-tui "fix the failing test"
rkat-tui -c                       # continue the latest session of this workspace
rkat-tui --resume ~2
rkat-tui --model llama3:8b        # bare ids are looked up in every endpoint's model list
rkat-tui --allow 'shell=cargo nextest run*' --set auto.compact=auto
```

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
  context; their calls pass the same gate, and a panel shows what they do.
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

Early, and moving fast. The engine comes from a pinned revision of a fork
of Meerkat (`zond/meerkat`): upstream `main` plus the sandbox fixes the
confined shell needs, proposed upstream as
[lukacf/meerkat#2094](https://github.com/lukacf/meerkat/pull/2094). A
second proposal, [lukacf/meerkat#2095](https://github.com/lukacf/meerkat/pull/2095),
covers thinking replay on proxied Anthropic models. The pin moves to
upstream once they land, and to a crates.io release after that.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
