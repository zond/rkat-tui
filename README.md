# rkat-tui

A coding agent for your terminal. You say what you want done; the agent
reads, edits and runs things in your project. The keys and commands are
Claude Code's, the engine is [Meerkat](https://github.com/lukacf/meerkat),
and it works with any model you can reach.

## What is different

- **The agent outlives your terminal.** It runs in a background daemon,
  one per project directory. `/detach` leaves it working, a dropped ssh
  connection does not stop it, and `rkat-tui -c` brings you back.
- **Auto mode with any model.** Routine actions just run. Risky ones are
  judged by a second model that sees only what you asked for and what the
  agent is about to do, never the agent's own reasoning, so the agent
  cannot talk its way past it. You are asked only after repeated blocks.
  `/auto check` measures how well your chosen model does this job.
- **Any endpoint.** Anthropic, OpenAI and Gemini directly, or any
  OpenAI-compatible server (LiteLLM, vLLM, Ollama). `/model` switches
  in the middle of a conversation.
- **A sandboxed shell by default.** The agent's shell sees the project
  and a few directories you grant, and none of your provider keys.
- **Several agents that cooperate.** The agent hands subtasks to helpers
  without waiting for them. A second `rkat-tui` in the same directory
  gets its own agent, and the two tell each other which files they are
  changing. `/wire` connects agents in other projects, or on other
  machines over ssh.
- **Timers and file watches.** An agent can wake itself on a schedule or
  when files change.
- **Your project stays clean.** Conversations, approvals and logs live
  under `~/.rkat/tui/`, not in the repository.

## Install

```bash
cargo install rkat-tui --locked    # or: cargo binstall rkat-tui
```

This installs `rkat-tui` and its daemon, `rkat-tui-mob`. It runs on Linux
and macOS and builds with Rust 1.97 or later. `cargo binstall` fetches a
prebuilt archive on x86-64 Linux and Apple-silicon macOS.

On Linux the sandbox needs unprivileged user namespaces. If the host does
not allow them, rkat-tui refuses to start and says how to enable them;
`--no-confinement` runs without the sandbox.

0.0.x is the maintainer's daily-use line. Expect sharp edges, and read the
[changelog](CHANGELOG.md) before you upgrade.

## Start

Export a provider key, then run it in your project:

```bash
export ANTHROPIC_API_KEY=…        # or OPENAI_API_KEY or GEMINI_API_KEY
rkat-tui                          # type a request and press Enter
rkat-tui "fix the failing test"   # or start with one
```

To use a proxy or a local server, name it as an endpoint. You give the
name of the environment variable that holds its key, never the key:

```bash
export WORK_LLM_KEY='sk-…'
rkat-tui --endpoint-def work=openai=https://litellm.example.com,key_env=WORK_LLM_KEY \
         --model work:claude-opus-5-5
```

To keep the endpoint, put it in `~/.rkat/tui/tui.toml`:

```toml
[endpoints.work]
dialect = "openai"
base_url = "https://litellm.example.com"
api_key_env = "WORK_LLM_KEY"
```

Inside the app, `?` on an empty input lists the keys and `/help` lists
the commands.

| To | Use |
|---|---|
| leave the agent working | `/detach` |
| come back to it | `rkat-tui -c` |
| finish and retire the agent | `/quit` |
| pick an earlier conversation | `/resume` |
| see every running daemon and agent | `rkat-tui --list` |
| change permission mode (manual, accept edits, plan, auto) | Shift+Tab |

## Documentation

- [User manual](docs/manual.md): everything above in detail.
- [Security model and its limits](docs/manual.md#security-model-and-its-limits):
  what the sandbox and the approval model cover, and what they do not.
- [Changelog](CHANGELOG.md).
- [Claude Code parity](docs/ux-parity.md): which keys and behaviours match.
- For working on rkat-tui itself: [testing](docs/testing.md), the
  [daemon's design](docs/mob-split.md) and the
  [engineering record](docs/evidence.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
