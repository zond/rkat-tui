# Changelog

## [Unreleased]

### Fixed

- A Markdown table wider than the transcript no longer loses its right-hand
  columns. When the columns' natural widths do not fit, the widest ones
  shrink (narrow ones keep their width, none below 6 columns) and their
  cells wrap within the column, word by word with an over-long word broken,
  so one row spans several lines with the separators on each. A table with
  too many columns for even that is shown stacked: one `header: cell` line
  per cell, its rows divided by a short rule. A table that fits looks as
  before.
- A `watch_paths` watch could report the agent's own `write_file` or
  `edit_file` back to it when the daemon was busy. The file tools now mark
  the write before the file changes, and a change is judged by when it
  happened rather than when its batch left.
- An interrupt no longer waits on a stop that stopped nothing: when the
  stop finds the run the client knew is not the agent's current one, or
  the run has not ended 10 s after Esc or Ctrl+Enter, the client
  hard-cancels whatever the agent runs then and waits once more. A turn
  whose interrupt is still not confirmed ends with a notice that the run
  may still be going, not as "turn interrupted". The daemon logs each
  interrupt it forwards.
- A Gemini model behind an OpenAI-compatible proxy (LiteLLM to Vertex AI)
  could not run at all: Vertex refused the request because the engine's
  `task_list` schema left its optional `status` without a `type`. Every
  agent's and helper's request now carries its tool schemas with `$ref`s
  inlined, optional fields' `anyOf [T, null]` folded into `T`, `oneOf` as
  `anyOf` and a `type` on every constrained node. `/context` measures the
  definitions as sent.
- The agents' hint says what a helper's status (`member_status`,
  `mob_check_member`) does not show: while a helper runs its first turn
  its preview and tokens are empty, and a fork's preview is the answer it
  inherited, not its own work; to know how a running helper does, ask it.
- A `send_message` call reads recipient first:
  `send_message → tui/main/main-bbbb: I am changing src/lib.rs` (the
  peer's name from the conversation's `peers` result, else its id), and a
  successful receipt folds to `sent to <name>` with the raw JSON behind
  the toggle; a failed send shows its error unfolded.
- The note other agents get when an agent leaves says why: "its user
  quit" only after `/quit`; one retired from `/agents` says who retired
  it, and one whose session closed otherwise says so.
- A `fork_off` child's outcome no longer fills screens with raw JSON:
  the transcript shows one header (`fork <name> finished (completed) ·
  11 turns · 39 tool calls · 12.3k tokens`) and the fork's report as
  Markdown folded to four lines (click or Ctrl-O unfolds it), live and in
  a restored conversation; a failed or cancelled one shows its error
  unfolded. The agent still reads the engine's notice unchanged.
- A helper seated with `spawn_member` or `spawn_many_members` is wired to
  the agent that seats it without being asked (rkat-tui sets
  `auto_wire_parent`), as a `delegate` helper already was: each sees the
  other in `peers`. An agent's message to its own helper is tier 1 like
  one to another agent. The instructions now say who reaches whom: the
  agent announces its helpers' file changes, a helper reaches only its
  parent, and a `fork_off` child (which the engine wires to no one) is
  told not to try.
- `/agents` stays open after `d` then `y`: once the daemon confirms the
  retire it lists the agents again without the retired one, the
  highlight on its neighbour, so several can be retired in a row; a
  refused retire (a running agent, one with a client, your own) shows its
  reason on the picker. Enter still closes it. `/schedules` and
  `/permissions` already behaved this way.
- `/compact` on an idle agent compacts at once instead of waiting for
  your next message. The engine compacts only at the check before a
  run's model request and has no call that compacts an idle session, so
  rkat-tui starts that run itself with a short prompt of its own: it
  costs one short model call after the summary, and its prompt and
  one-word reply stay in the conversation but are not shown (live or
  restored). A message typed meanwhile is sent after it. While any
  compaction runs, the status bar shows `compacting… Ns`.
- While a model request runs longer than five seconds, the status bar
  says that queued messages wait for the model's reply, and for how long,
  instead of a bare "delivering…".

## [0.0.2] - 2026-11-13

### Added

- `cargo binstall rkat-tui` installs prebuilt binaries: every release
  from this one on has an archive for x86-64 Linux (glibc 2.35 or later)
  and for Apple-silicon macOS on its GitHub release, built with the
  SQLite settings a plain `cargo install` leaves out. Other targets are
  built from source as before.
- A skills directory of your own, `~/.rkat/tui/skills/`
  (`<config-dir>/skills/`): every workspace's agents and their `delegate`
  helpers list its skills, ahead of the realm config's skill
  repositories and the project's `.rkat/skills`. A project skill with the
  name of one of yours is not listed; yours is. `[skills] enabled =
  false` in the realm config still turns skills off. `/context` names
  your skills directory and the project's with how many skills each
  holds; the manual's new "Skills" section says what a skill looks like.
- `skill_read`, so an agent can use its skills: given a skill's name it
  returns the skill's `SKILL.md` body (your copy when the project has one
  of the same name, at most 32 KiB), without a name it lists every skill
  with its source and description. It is read-only and allowed in every
  permission mode, plan mode included, and `delegate` helpers have it
  too. It costs every request about 370 bytes of tool definition.
- The skill inventory names your and the project's skills with their
  descriptions: when the realm config leaves `[skills]
  inventory_threshold` at Meerkat's default of 12 (which its own 12
  skills fill), rkat-tui uses 32. Past 32 skills the inventory counts
  them per source and `skill_read` lists them. `/context` says which of
  the two the agent's inventory does.

### Changed

- The engine is Meerkat 0.9.14 from crates.io (was 0.9.13). What it
  changes for rkat-tui: a conversation moved between two OpenAI-family
  endpoints (the OpenAI API, a self-hosted or LiteLLM server in
  Responses mode, Azure, Copilot) no longer fails its first turn on the
  new one, because encrypted reasoning and response ids go back only to
  the service that issued them; after the daemon restarts, earlier turns
  are sent without that material, so the provider's prompt cache misses
  once. A request an OpenAI-family service refuses inside its response
  stream fails at once with the service's reason instead of being sent
  again until the retries run out (`retry exhausted`), and the ChatGPT backend's
  `usage_limit_reached` is an exhausted quota, not retried. A write the
  operating system refuses (a full disk, a quota) no longer leaves the
  realm's `realm_manifest.json` empty while the open reports success.
  Stored state is unchanged: a realm 0.0.1 wrote opens as it was.
- The agent's note on its hidden mob tools now says what they do:
  `mob_create` and `mob_spawn_member` start helpers that stay after their
  task until retired, a `council`'s summary and a `fork_off` child's
  outcome arrive later in the conversation. A further line says that
  `spawn_member`, which needs no search, seats a helper in the agent's
  own mob, wired to it with `auto_wire_parent` so the two can message
  each other.
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

- Short conversation ids (in `/resume`, `/agents`, the status bar and
  the notices) are an id's last eight characters, not its first: ids
  start with the time, so conversations made within a minute or so showed
  the same short id. `/resume <short id>` takes that form; a prefix still
  works.
- More commands open a list you act on, as `/agents` does: `/schedules`
  lists this agent's schedules (Enter or `p` pauses or resumes one, `d`
  then `y` deletes it, and the list shows the change at once);
  `/permissions` lists the approval rules with their pattern or literal
  and layer, and `d` then `y` removes one (for good for the workspace's
  own rules, until the daemon restarts for one written in another layer,
  as `/permissions rm` does; adding stays `/permissions add`);
  `/helpers` lists this conversation's helpers as their headers show
  them, nested ones indented: Enter opens one's full view, `f` folds or
  expands its block (`/helpers list` prints the old notice); `/mode`
  lists the four modes with what each asks and allows, the current one
  marked, and Enter switches (on a paused auto mode it resumes it, as
  `/auto resume`; `/auto` still shows the mode as a notice); `/memory`
  lists the workspace `AGENTS.md`, the user `AGENTS.md` and `SYSTEM.md`
  with each path and size (or that it is not written yet), and Enter
  edits one. Bare `/memory` used to open the workspace `AGENTS.md`; that
  is now `/memory workspace`. The list
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

- The manual's `fork_off` paragraph said a fork's result comes back as
  the call's outcome; the call returns at once and the outcome is written
  into the conversation later.
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
