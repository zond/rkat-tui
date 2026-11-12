# Changelog

## [Unreleased]

### Changed

- `glob` and `grep` are bounded like the other tools: they walk the tree
  on several threads and stop after 10 s (`[search] budget_secs`) or
  500 000 files and directories examined (`[search] max_entries`),
  answering with what they found and a note that says the answer is
  partial, how far the search got, which directories held most entries
  and how to narrow it. `grep`'s reading of each file counts against the
  budget. A big directory no longer holds a call (or a helper waiting on
  it) for the engine's whole ten-minute tool timeout.
- A file tool's work stops when its call goes away (the tool timeout,
  Esc, a cancelled helper): a search stops walking, and a write, edit or
  note change not yet made is not made. Before, a timed-out `glob` kept
  walking in the daemon after its call had ended.
- When `grep`'s output fills up it stops searching, and the lines it
  shows are sorted by path but no longer necessarily the first matches by
  path; the truncation line says so.

- More commands open a list you act on, as `/agents` does: `/schedules`
  lists this agent's schedules (Enter or `p` pauses or resumes one, `d`
  then `y` deletes it, and the list shows the change at once);
  `/permissions` lists the approval rules with their pattern or literal
  and layer, and `d` then `y` removes one (for good for the workspace's
  own rules, until the daemon restarts for one written in another layer,
  as `/permissions rm` does; adding stays `/permissions add`). The list
  moves with ↑/↓ or j/k and closes with Esc, `q` or Ctrl-C; an approval
  request takes over and the list comes back after it. The typed forms
  (`/schedules pause <name>` and the like) work as before.

- `/agents` opens a picker of the workspace's agents, like `/model` and
  `/resume`: each row shows the agent's name, running or idle, its
  conversation (short id and first message), its clients, `detached` when
  it has none, and marks your own. Enter switches to the highlighted
  agent, the last row starts a new agent for this TUI, and `d` then `y`
  retires an idle agent nobody works with (its conversation stays in
  `/resume`); the daemon refuses to retire a running agent, one with a
  client, and your own, saying why. `/agents <name>` still switches
  directly.
- A start that gets a new agent beside others says so in one notice: how
  many others work there, how many are idle without a client, and that
  `/agents` picks one (`-c` would have joined the most recent). It
  replaces the old "other agents work in this workspace" notice for that
  case.
- `rkat-tui --help` says which question each of `--attach` (which
  daemon), `-c` and `--resume` (which agent in it) answers, and has
  examples for joining and for a new agent in another workspace's daemon.

- A `delegate` call that times out no longer leaves its helper seated:
  the daemon retires the helper when the call ends without its result
  (timed out, or still open when the agent's run ends), as the engine
  does after a helper's turn completes or fails. A `spawn_member` member
  is never retired this way.
- A `delegate` call waits for its helper three times as long as any one
  tool call may run (30 min with the default 600 s tool timeout), unless
  the realm's `config.toml` sets `[tools.tool_timeouts] delegate`. Before,
  it waited exactly as long as one tool call of its helper, so a helper's
  long shell command timed out together with the `delegate` and the
  helper could not report it. Nothing is written to the config.
- The agent is told that a `delegate` helper is a member of the mob
  `implicit-<its session id>`, not of the workspace's mob, and that
  `mob_list` shows the mobs, so it no longer takes `member_not_found` in
  the workspace's mob for a helper that is gone.

### Documentation

- The manual opens "The workspace daemon" with "Which daemon, which
  agent": a table of the four starts (this workspace's daemon or another,
  joining an agent or starting a new one), what idle agents keep, and
  what stopping the daemon does to them; the README's daemon section has
  the same table, and the other places that explain `--attach`, `-c`,
  `--resume` and several agents follow it.

- The README shows how to use a LiteLLM (or any OpenAI-compatible) proxy:
  the key in an environment variable, the endpoint on the command line or
  in `~/.rkat/tui/tui.toml`. The manual's "Models, endpoints and cost"
  opens with the same as a short setup guide; how endpoints reach the
  engine follows it.

## [0.0.1] - 2026-11-12

The first release. rkat-tui is an interactive terminal coding agent with
Claude Code's keys and feel, built on the Meerkat agent engine: you type
what you want done, and the agent reads, edits and runs things in your
project. Every tool call passes an approval gate, and the agent's shell
runs in an OS sandbox. 0.0.x is the maintainer's dogfood line;
[`docs/manual.md`](docs/manual.md) describes all of it.

### The daemon and the client

- The agent runs in a per-workspace daemon, `rkat-tui-mob`, as a durable
  Meerkat mob member. `rkat-tui` is its client and starts it on demand.
- `/detach` leaves the agent working and `rkat-tui -c` comes back to it.
  `/quit` retires your agent (its conversation stays) and stops the
  daemon when this client started it and is the last one; otherwise it
  asks. A turn ends when the daemon reports the agent idle.
- `-c`, `--resume [ref]` and `/resume` continue a conversation, `/new`
  starts one. `--attach=DIRECTORY` attaches to another workspace's daemon,
  `--attach=HOST:PORT` through a tunnel; the daemon listens on loopback
  only, and the attach token is proved, never sent. `--list` lists the
  running daemons and their agents.
- A workspace's state (sessions, approvals, rules, notes) lives outside
  it, in `~/.rkat/tui/workspaces/<id>/`; `/rehome` follows a moved
  workspace. The audit log there records every approval decision and
  every change of mode, rules and trust (`/audit`, `--audit`).

### Agents and helpers

- Each `rkat-tui` started beside a running daemon, without `-c` or
  `--resume`, gets an agent of its own. The agents are wired as peers and
  tell each other which files they change; `/agents` lists them and
  switches between them.
- A turn that another agent, a schedule or a file watch started while no
  client is attached is unattended: anything that needs review is
  denied, and the next client to attach shows what was denied.
- The agent hands subtasks to helpers with `delegate` and forks itself
  with `fork_off`. Helpers pass the same gate; a panel under the input
  shows them, with a full view per helper.

### Approvals and the sandbox

- Auto mode, the default: read-only tools and narrow allow rules run,
  in-project edits outside protected paths run, and everything else goes
  to a classifier that sees your messages and the agent's tool calls, not
  its reasoning or tool output. After 3 consecutive or 20 total blocks you
  are asked instead. Shift+Tab cycles manual, accept edits, plan and auto.
- Writes to protected paths (`.env*`, `build.rs`, `AGENTS.md`, …) always
  ask; sealed paths (the project's `.rkat/` and `.git/`, the workspace's
  state, `~/.rkat/tui`) are never written by the file tools. Rules live in
  layered `tui.toml` files and `/permissions` edits them. A project's MCP
  servers and hooks run only after `/trust`.
- On Linux the shell sees only the workspace, the temp dir, the Cargo home
  and a few system directories, with an environment that leaves provider
  keys out; `--confinement=strict` also removes the network. A host that
  cannot confine refuses to start and says how to fix it;
  `--no-confinement` runs the shell unsandboxed. `!` runs a command as
  you, with no sandbox.

### Tools

- File tools with diffs, the engine's shell, task tools, notes kept across
  sessions and semantic memory over compacted history. Instructions come
  from `SYSTEM.md` and `AGENTS.md`; `/remember`, `/memory` and `/init`
  edit them.
- `tool_search` reveals the schedule tools and the mob tools other than
  `delegate` only when the agent searches for them: a fresh
  conversation's requests carry about 55 KB of tool definitions, not
  122 KB.

### Models and endpoints

- `anthropic`, `openai` and `gemini` endpoints. A LiteLLM proxy or other
  OpenAI-compatible server runs its models as `<endpoint>:<model>`, with
  the window and prices it reports, over the Responses API or Chat
  Completions. `/model` switches the live session, across endpoints too.
- The status bar shows a context gauge and a cost estimate; `/usage` and
  `/context` break them down. Compaction is Meerkat's own; `/compact` asks
  for one at the next model call.

### Schedules and watches

- An agent can set one-shot and recurring timers; creating or changing
  one is reviewed, and the daemon fires them while you are detached too.
  `/schedules` lists, pauses, resumes and deletes them.
- `watch_paths` tells the agent when workspace files change.

### The UI

- Claude Code's keys where both have the feature: Enter sends, or queues
  while the agent works; Ctrl+Enter sends now; Esc interrupts; Up takes
  back queued messages the agent has not read yet, which show as on their
  way until it does (one the runtime drops unread returns to the editor,
  saying why); `?` lists the keys. The approval dialog offers the
  same three numbered choices.
- A `/` command menu, `@` file mentions, Ctrl+R history search, readline
  editing, Ctrl+G external editor, Ctrl+T task checklist, `/export`,
  `/copy`, mouse selection over OSC 52, and a bell when a turn ends while
  the terminal is unfocused. [`docs/ux-parity.md`](docs/ux-parity.md)
  compares the keys with Claude Code's.

### Requirements

- Rust 1.97 or later; `cargo install rkat-tui --locked` installs both
  binaries. Meerkat 0.9.13 from crates.io.
- A provider key (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or
  `GEMINI_API_KEY`) or a configured endpoint.
- Linux, with unprivileged user namespaces for the sandbox (on Ubuntu
  24.04 and later an AppArmor profile; the start-up error shows it).
- macOS: the shell is meant to be confined with Seatbelt there, but CI
  builds and tests on Linux only, and rkat-tui has not been tested on
  macOS. Other platforms have no sandbox.

### Known limits

- [Security model and its limits](docs/manual.md#security-model-and-its-limits):
  the file tools run as you, outside the sandbox; the sandbox shares
  `/tmp` and `~/.cache`; read-only git commands run what `.git/config`
  names; a non-loopback attach is unencrypted. Each limit says how to
  tighten it.
- The daemon's [limits for now](docs/manual.md#the-workspace-daemon):
  `/compact now` needs the agent to have run since the daemon started, and
  a `.rkat/mcp.toml` trusted later does not reach a running agent.
- There is no rewind: the engine cannot fork a mob member's session under
  the same identity.
- The comms (peer) tools and the mob operator tools (`spawn_member` and
  the rest) cannot be hidden: the engine mounts them, so their
  definitions ride on every request.
- The shell runs commands in the foreground only.
