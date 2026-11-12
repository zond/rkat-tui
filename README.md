# rkat-tui

A terminal coding agent with Claude Code's keys and feel, built on the
[Meerkat](https://github.com/lukacf/meerkat) agent engine. You type what
you want done; the agent reads, edits and runs things in your project.
Every tool call goes through an **auto-mode** approval model: routine
actions run without interruption, risky ones are judged by a
reasoning-blind classifier, and you are asked only after repeated blocks.
The agent's shell runs inside an OS sandbox.

## Install

0.0.x is the maintainer's dogfood line: each release carries the fixes
from using the one before. Expect sharp edges, and read the
[changelog](CHANGELOG.md) before you upgrade.

```bash
cargo install rkat-tui --locked   # installs rkat-tui and its daemon, rkat-tui-mob
export ANTHROPIC_API_KEY=…        # or OPENAI_API_KEY or GEMINI_API_KEY
```

It needs Rust 1.97 or later and runs on Linux and macOS. On Linux the
sandbox needs unprivileged user namespaces; a host that cannot confine the
shell refuses to start and explains how to enable them (for example an
AppArmor profile on Ubuntu 24.04 and later). macOS uses Seatbelt; other
platforms have no sandbox. `--no-confinement` runs the shell unsandboxed.

The daemon's log may say that SQLite runs with process-wide locks: a plain
install builds the bundled SQLite with its defaults, which work but make
concurrent sessions in the daemon queue on three global locks;
`LIBSQLITE3_FLAGS="-DSQLITE_DEFAULT_MEMSTATUS=0 -USQLITE_ENABLE_MEMORY_MANAGEMENT -DSQLITE_DISABLE_PAGECACHE_OVERFLOW_STATS" cargo install rkat-tui --locked`
builds it without them (this checkout's `.cargo/config.toml` sets the same,
so `cargo install --path .` here gets them too).

Your projects stay clean: a workspace's sessions, approvals and audit log
live under `~/.rkat/tui/workspaces/<id>/`, not in the project (see
[Where things live](docs/manual.md#where-things-live)).

## Start

You need a provider key: export `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or
`GEMINI_API_KEY`. Then, in your project:

```bash
rkat-tui                          # type a request and press Enter
rkat-tui "fix the failing test"   # or start with one
```

**A LiteLLM proxy** (or vLLM, Ollama, any OpenAI-compatible server) is an
endpoint you name. Its key stays in an environment variable; you tell
rkat-tui the variable's name, never the key:

```bash
export WORK_LLM_KEY='sk-…'
rkat-tui --endpoint-def work=openai=https://litellm.example.com,key_env=WORK_LLM_KEY \
         --model work:claude-opus-5-5     # <endpoint>:<model as the proxy lists it>
```

To keep it, put the same in `~/.rkat/tui/tui.toml` and pick the model with
`/model` inside the app:

```toml
[endpoints.work]
dialect = "openai"
base_url = "https://litellm.example.com"
api_key_env = "WORK_LLM_KEY"
```

More (Chat Completions servers, gateways, prices, the classifier's model):
[Models, endpoints and cost](docs/manual.md#models-endpoints-and-cost).

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
- **Sandboxed shell.** The agent's shell sees only the workspace and a
  few granted directories, with an environment that keeps provider keys
  out; `--confinement=strict` also denies the network. Your own `!`
  commands run as you, with no sandbox, like anything else you type in a
  terminal.
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
- [Security model and its limits](docs/manual.md#security-model-and-its-limits):
  what the sandbox covers and what it does not (the file tools, `!`,
  shared `/tmp` and `~/.cache`, git's repository configuration, user
  notes, `@` attachments, remote attach), and how to tighten each.
- [`docs/ux-parity.md`](docs/ux-parity.md): the Claude Code parity audit.
- [`CHANGELOG.md`](CHANGELOG.md).
- Working on rkat-tui itself: [`docs/testing.md`](docs/testing.md) (the
  test protocol), [`docs/mob-split.md`](docs/mob-split.md) (the daemon's
  design) and [`docs/evidence.md`](docs/evidence.md) (the engineering
  record).

Early and moving fast; built on Meerkat 0.9.13 from crates.io.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
