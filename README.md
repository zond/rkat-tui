# rkat-tui

A terminal coding agent with Claude Code's keys and feel, built on the
[Meerkat](https://github.com/lukacf/meerkat) agent engine. You type what
you want done; the agent reads, edits and runs things in your project.
Every tool call goes through an **auto-mode** approval model: routine
actions run without interruption, risky ones are judged by a
reasoning-blind classifier, and you are asked only after repeated blocks.
The agent's shell runs inside an OS sandbox.

## Install

rkat-tui builds with plain `cargo` on Rust 1.97 (`rust-toolchain.toml` pins
1.97.0):

```bash
cargo install --path .     # installs rkat-tui and its daemon, rkat-tui-mob
```

On Linux the sandbox needs unprivileged user namespaces; a host that cannot
confine the shell refuses to start and explains how to enable them (for
example an AppArmor profile on Ubuntu 24.04 and later). macOS uses Seatbelt;
other platforms have no sandbox. `--no-confinement` runs the shell
unsandboxed.

## Start

You need a provider key: export `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or
`GEMINI_API_KEY` (other servers, such as a LiteLLM proxy or a local model,
are added as endpoints; see
[Models, endpoints and cost](docs/manual.md#models-endpoints-and-cost)).
Then, in your project:

```bash
rkat-tui                          # type a request and press Enter
rkat-tui "fix the failing test"   # or start with one
```

`?` on an empty input lists the keys and `/help` the commands.

## The agent lives in a daemon

The agent does not run inside the terminal UI. The first `rkat-tui` in a
workspace starts that workspace's daemon, `rkat-tui-mob`, in the
background and attaches to it; every later `rkat-tui` there attaches to
the same one. So the agent keeps working when you leave:

- `/detach` leaves and keeps the agent running; `rkat-tui -c` brings
  you back to it.
- `/quit` means you are done: your agent retires (its conversation
  stays), and the daemon stops when this `rkat-tui` started it and is the
  last one attached (or asks, when an agent was left running).
- `rkat-tui -c` continues with the agent active last, `/resume` picks
  another conversation, and `rkat-tui --list` shows every running daemon
  and its agents.

A second `rkat-tui` in the same directory gets an agent of its own in the
same daemon, wired to the others so they can tell each other which files
they are changing (`/agents` lists them; see the manual's "Several
agents").

## Highlights

- **Auto-mode approval.** Read-only tools and narrow allow rules run,
  in-project edits outside protected paths run, everything else goes to
  the classifier; after 3 consecutive or 20 total blocks you are asked.
  Shift+Tab cycles manual → accept edits → plan → auto, as in Claude Code.
- **Sandboxed shell.** The agent's shell (and your own `!` commands) sees
  only the workspace and a few granted directories, with an environment
  that keeps provider keys out; `--confinement=strict` also denies the
  network.
- **Helpers.** The agent can `delegate` subtasks to helper agents or fork
  itself; their calls pass the same gate, and a panel shows what they do.
- **Any endpoint.** `anthropic`, `openai` (also OpenAI-compatible proxies)
  and `gemini`; `/model` switches between them, with a context gauge and a
  cost estimate in the status bar.
- **Instructions and memory.** `AGENTS.md` and `SYSTEM.md` files,
  `/remember`, notes the agent keeps across sessions, and semantic memory
  over compacted history.

## Documentation

- [`docs/manual.md`](docs/manual.md): the user manual, starting with
  "First run".
- [`docs/ux-parity.md`](docs/ux-parity.md): the Claude Code parity audit.
- [`CHANGELOG.md`](CHANGELOG.md).
- Working on rkat-tui itself: [`docs/testing.md`](docs/testing.md) (the
  test protocol), [`docs/mob-split.md`](docs/mob-split.md) (the daemon's
  design) and [`docs/evidence.md`](docs/evidence.md) (the engineering
  record).

Early and moving fast; built on Meerkat 0.9.11 from crates.io.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
