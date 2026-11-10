# Changelog

## [Unreleased]

## [0.0.1] - 2026-11-10

The first release on crates.io, opening the 0.0.x line: the maintainer's
dogfood builds, each carrying the fixes from using the one before. The
state before the daemon was recorded as 0.1.0 on 2026-11-08 but never
published; the numbers were renumbered for this line, so that section is
the last part of this one.

The agent moved out of the terminal UI into a per-workspace daemon:
`rkat-tui` is now the client of `rkat-tui-mob`. `docs/evidence.md` holds
the detailed record of every change below, and `docs/mob-split.md` the
design.

### Security

- A workspace's state (sessions, approvals, rules, conversations, notes,
  the audit log) no longer lives in `<workspace>/.rkat/tui/`, where the
  agent's confined shell could rewrite it and a clone could carry it,
  but in `~/.rkat/tui/workspaces/<random id>/` (under `--config-dir` when
  given). The daemon refuses to start, and so does the TUI for its `!`
  shell, when the shell's confinement would reach that directory: a
  workspace that holds it (`-w ~` with the default config directory), or
  a `--confinement-write`/`extra_write` grant of `~/.rkat`. The agent's
  file tools cannot read it either.
- The manual's "Security decisions" says what rkat-tui does where the
  review asked for a choice: the network stays on by default
  (`--confinement=strict` turns it off), detached agents have no idle
  timeout, peer-started turns are unattended, and where the audit log is.
  A managed policy file and `cargo-vet` are deliberately not provided
  until asked.
- The sandbox's claims are pinned by tests on Linux: a confined command
  does not see `~/.ssh` or the Cargo home's credentials, and `strict`
  has no network, and it can neither read nor write the workspace's
  state directory. It can still write `.git/` inside the workspace (the
  engine cannot bind it read only yet); a test pins that gap too, so it
  is noticed when the engine closes it. CI runs these tests for real (the runner's
  user-namespace restriction lifted, and a skip there is a failure), and
  a test pins that the agent's file tools cannot write the project's
  `.rkat/` inputs (`mcp.toml`, `tui.toml`, `config.toml`, hook scripts).
- CI checks the dependencies with `cargo deny` (`deny.toml`): RustSec
  advisories, the licence allow-list and crates.io as the only source.
  Two advisories without a fix this release can take are ignored with a
  date and what lifts them: the bundled SQLite (RUSTSEC-2026-0459, fixed
  in the next Meerkat release) and rmcp's unbounded HTTP bodies
  (RUSTSEC-2026-0462, reachable only from an HTTP MCP server you
  configure).
- The daemon keeps an audit log: every approval decision (the tool, the
  command or paths whole, the tier, the verdict and who decided: a rule,
  the classifier, you through which connection, the mode, the policy) and
  every change of mode, rules, trust and agents, one JSON line each, in
  `audit.jsonl` in the workspace's state directory (outside the
  workspace and the agent's reach; rotated once at 50 MB). `/audit [n]` shows your agent's
  last lines, `rkat-tui --audit` prints it.
- A turn another agent's message started on your agent is unattended
  while no TUI is attached to it: an action that would need review is
  denied instead of going to the classifier (or waiting for you), the
  agent is told it needs you attached, and the next TUI that attaches to
  the agent shows what it wanted to do. Reads, allowlisted commands and
  the in-project edits your mode allows run as before; a turn you started
  is reviewed as usual after `/detach`.
- `[pricing] fetch = false` in `tui.toml` turns the community price-list
  download off, as `--no-pricing-fetch` does; the manual names the one
  host rkat-tui contacts besides your LLM endpoints.
- The confined shell no longer gets the whole Cargo home writable: only
  `registry/` and `git/` are, the rest (`bin/`, `config.toml`, …) is read
  only, and `credentials.toml` is not in its view. Before, a confined
  command could replace `~/.cargo/bin` tools, set `build.rustc-wrapper` or
  read the registry token.
- A helper on another model than its owner's runs through the owner's
  endpoint, or its spawn is refused naming the endpoints that would serve
  it. Before, it fell back to that model's provider default: the public
  URL with whatever key the environment held.
- A TUI no longer attaches to a daemon whose agent shell may do more than
  the TUI's own flags allow (started unconfined, or with network, or with
  more directories): it names both and says how to restart the daemon. A
  daemon whose shell may do less is attached with a warning. Before, a
  later TUI's `--confinement` flags were silently ignored.
- The agent's file tools can no longer read the daemon's attach token:
  the endpoint directories are sealed for `read_file`, `grep` and `glob`
  (tier 0, never a question for the classifier), and a search above them
  skips them. Before, a read outside the project went to the classifier,
  which could allow it.
- The attach handshake no longer sends the token. The daemon proves it
  holds it first (an HMAC over a nonce from each side), then the client
  does; a process that took a dead daemon's port, or answers during the
  shutdown window, is refused before the client sends anything it could
  attach with. A daemon of before this handshake is reported as too old;
  a client of before it still attaches.
- A project's `.rkat/tui.toml` can no longer pick the classifier's model:
  its `auto.classifier_model` is ignored with a notice, so a cloned
  repository cannot hand the review of the agent's actions to a model
  that is cheap to talk past.
- The approval dialog's Ctrl+O view of the whole arguments scrolls:
  PgUp/PgDn, Home and End reach every line on an ordinary terminal, and a
  line says which lines are shown. Before, it showed what fit and said the
  rest did not, so the end of an action too large for the classifier could
  not be read before allowing it.
- `@`-mentioned file content is data, not your words. The classifier
  records what you typed as your request and the attached files as a
  context line naming them; before, the whole expanded prompt, file
  content included, was recorded as the user's words, so an instruction
  planted in a mentioned file read as something you asked for. Resumed
  conversations are seeded the same way.
- The classifier sees the action it reviews whole. Before, every string
  argument was cut at 1 KiB, so a shell command padded past that hid its
  tail (`; curl … | sh`) from the review. An action larger than 48 KiB
  is no longer given to the classifier: you are asked ("too large to
  review"), or it is denied when nobody can be. The approval dialog says
  when it abbreviates the arguments, and Ctrl+O shows them all.
- A workspace realm config (`.rkat/tui/config.toml`) can no longer
  redirect or widen anything. Its `[confinement]` table is ignored, with a
  start-up notice (only `--confinement` and your own `tui.toml` set the
  sandbox; a realm outside the workspace still sets a floor). When the
  file is not trusted (it came with the repository, or changed since
  rkat-tui last wrote it or you trusted it), its provider connections
  (backend `base_url`s, credential sources such as host commands,
  bindings), `[self_hosted]` servers and `[skills] repositories` are left
  out as well as its MCP servers and hooks, in the daemon and in the
  client's model listing; the endpoint connections rkat-tui projects from
  your `tui.toml` stay. A realm directory that git tracks, or that holds
  files without the `.gitignore` rkat-tui writes, refuses the start with
  the `git rm -r --cached` recipe (`--trust-project` starts anyway); a
  split index is read through its shared index, and an index that cannot
  be read in full refuses. The
  realm rules file keeps its digest trust: a rule appended under the
  daemon gives only its deny rules and a notice until `/rules trust`.
  (On Linux the agent's shell can still write `.rkat/`; a read-only
  mount needs an engine change, recorded as an ask.)
- The manual and the design say what the daemon does not keep apart:
  every attached TUI receives every agent's approval requests (each shows
  only its own), and another agent's message can start a turn on an idle
  agent nobody prompted (the approval gate still judges it, and the
  classifier sees no consent of yours behind it).
- `--attach=HOST:PORT` sends the attach token only to a loopback address
  (what an `ssh -L` tunnel gives). Any other address is refused, naming
  the tunnel recipe, before the token file is read; `--attach-insecure`
  allows it. Before, any host named received the token in clear text.
- A connection drives only the agent it is bound to. The daemon refuses,
  with error `-32013` (`NOT_YOURS`), an engine request that would prompt,
  steer, start, stop, cancel, compact or append to another agent or a
  helper working for another agent, and a `session/compact` of any session
  but the caller's agent's current one. Before, any client could act on
  any agent by naming it, and its prompt was recorded as the user's words
  in that agent's classifier transcript.
- The daemon forwards only the engine methods its client uses
  (`protocol::engine::SERVED`) and refuses the rest with error
  `-32012` (`NOT_SERVED`). Before, an authenticated connection could
  call the engine's `session/create` (or `turn/start`, `mob/create`, …)
  and run a session outside the daemon's mob, whose tool calls never
  reached the approval gate.
- The daemon forwards only what it parsed. A client line that is not a
  JSON-RPC request object (unparsable, no `method`, a `jsonrpc` other
  than `"2.0"`, an `id` that is neither a string nor an integer) is
  answered by the daemon with a parse or invalid-request error and never
  reaches the engine, and a served request is forwarded as the daemon's
  own serialization of it. Before, a line the daemon could not parse was
  forwarded as sent, and the engine trims Unicode whitespace the daemon's
  parser does not: a `session/create` behind a leading form feed passed
  the allow-list above and ran outside the mob.
- The attach token lives outside the workspace and everything the agent's
  confined shell is granted: the daemon's endpoint file is
  `$XDG_STATE_HOME/rkat-tui/<key>/mob.endpoint` (else under
  `~/.local/state`; directory 0700, file 0600), or the same path under
  `$XDG_RUNTIME_DIR` when the shell would be granted the state directory,
  and the daemon refuses to start when neither is safe. A token file given
  with `--attach-token-file` is refused when its group or others can read
  or write it.

### Added

- `cargo install rkat-tui --locked` installs both binaries from
  crates.io. A plain install builds the bundled SQLite with its default
  process-wide locks (the daemon's log says so once, and it works); the
  README's Install section gives the `LIBSQLITE3_FLAGS` setting that
  builds it without them. `LICENSE-MIT` and `LICENSE-APACHE` are real
  files now.
- `rkat-tui --version` and `rkat-tui-mob --version` print the build id
  the attach handshake compares: the version plus the git commit when
  built from a checkout (`0.0.1+0123456789`), the version alone from
  crates.io.
- `/rehome <directory>` moves this workspace's state to another directory
  from inside the TUI: the daemon restarts there and the same `rkat-tui`
  reattaches to the same conversation without quitting. `/rehome <id|old
  path>` in the TUI and `rkat-tui --rehome <id|old path>` from the shell
  adopt the state of a workspace that was moved or renamed, replacing the
  empty state a first start in the new place made (removed); the TUI
  restarts its daemon on it and continues. A daemon on the state that
  moves is stopped and started again; a re-home is refused, changing
  nothing, while a turn runs on a daemon it would stop and when the
  destination's state holds a conversation or another client. An
  argument that reads both ways is refused naming both (`/rehome from
  <ref>`, `/rehome to <dir>`). `/rehome` alone lists the state whose
  workspace is gone; a start in a workspace with no state while such
  state exists offers it instead of adopting any. After any re-home, `-c`
  in the new directory continues the latest conversation.
- `--list` also lists every workspace's state: its id, the workspace
  (`gone` when it no longer exists), the git remote, the last use and
  whether a daemon runs for it.
- `/daemon start` connects to the workspace's daemon again, starting it
  when this `rkat-tui` may: after a failed attach or a `/daemon stop`.
- The daemon names its build (version and git commit) when a client
  attaches; a client of another build says so and shows it in the status
  bar.
- `rkat-tui-mob`, the workspace daemon. It bootstraps the workspace realm
  as `rkat-tui` did (config layers, endpoints, confinement, project trust,
  instructions, MCP), takes the same engine flags, and hosts the agent as
  a durable mob member (mob `tui`, member `main`) with rkat-tui's file and
  note tools, behind the approval gate (tiers, rules, classifier,
  permission modes, escalation timeout, `[thinking]`, the `[auto]`
  compaction policy). It serves Meerkat's own JSON-RPC methods to any
  number of clients over loopback TCP after an `rkat/attach` with the
  token from its endpoint file, plus its own `rkat/*` methods. A call
  that needs a human is one of the engine's durable decisions, pushed to
  every attached client and answered as the daemon's one principal
  (`[daemon] principal` in your own `tui.toml`, default your login
  name); decisions a stopped daemon left open are withdrawn at its next
  start. `rkat-tui-mob --foreground -w . --model …` runs it by hand.
- `rkat-tui` attaches to the workspace's daemon, starting it in the
  background when none answers (detached with `setsid` when it is on
  `PATH`; output in `<realm>/logs/rkat-tui-mob.stderr.log`). Prompts,
  steers, Esc, approvals (every attached TUI shows the dialog, the first
  answer counts), the permission mode, rules, `/trust`, `/model` (switched
  in place on the daemon's session), `/new`, `/resume`, `/compact`,
  `/context`, `/status`, `/cost` (priced, helpers included), the Ctrl-T
  checklist, the agent's file diffs, helpers and `!` commands all work
  through the daemon. A dropped connection is re-made with backoff
  (restarting a daemon that died), and a reattach resumes the agent's
  event stream from where it stopped (`.rkat/tui/attach.cursor`), so it
  shows only what happened since. The daemon's start-up notices show when
  a TUI first attaches.
- `/detach` leaves and keeps the agent working; run `rkat-tui` again to
  come back. `/quit` (also Ctrl-D, and Ctrl-C twice) stops the daemon
  when this `rkat-tui` started it and nobody detached since; with other
  TUIs attached it leaves the daemon running for them and says so;
  otherwise it asks: detach (the default) or terminate, saying when
  terminating interrupts a running turn. A closed terminal (SIGHUP) or a
  lost connection is a detach. After the TUI closes it says whether the
  daemon still runs, and reports a stop the daemon never confirmed only
  once the daemon is shown to be gone. `/daemon stop` stops the daemon
  from any attached TUI.
- `rkat-tui --list` (also `rkat-tui-mob --list`) prints every daemon that
  has an endpoint file, one line each: workspace, address, pid, uptime,
  attached clients, current conversation, running or idle, and the realm
  when it is not the workspace's own; `stale` for a file nothing answers
  at, removed when its process is gone. The token is never printed.
- `--attach=DIRECTORY` attaches to the running daemon of the workspace at
  or above that directory (never starting one); `!`, `@` mentions,
  `/export`, `/remember`, `/memory` and the input history then use that
  workspace. `--attach=existing` attaches only to a running daemon. The
  value needs `=` (`--attach "fix it"` attaches and sends the prompt).
- `--attach=HOST:PORT --attach-token-file <path>` attaches to a daemon
  reached through a tunnel (the daemon still listens on loopback only;
  the manual has the `ssh -L` recipe). Unless an endpoint file on this
  machine names that daemon for an existing workspace, the features that
  read or write local files (`!`, `@` mentions, `/remember`, `/memory`,
  `/export` to a path) are refused with the reason.
- `-c` attaches to the daemon's current conversation; `--resume <ref>`
  attaches and switches the daemon to that conversation, and `--resume`
  alone opens the picker once attached. A prompt on the command line
  waits until the TUI has attached.
- `/resume` lists sessions created before the daemon after the daemon's
  conversations, marked "view only, created before the daemon": choosing
  one shows its transcript (`/export` saves it) while the agent stays on
  its conversation. The engine seats the daemon's agent only on sessions
  it ran.
- Several agents per workspace, in one daemon: each is a member of the
  daemon's mob named `main-<id>` (shown in the status bar, `/agents`,
  `--list` and `rkat/status`; a daemon whose mob still has the single
  `main` keeps it). Each `rkat-tui` works with one agent, which its
  commands act on (`/new`, `/resume`, `/model`, prompts, Esc), and sees
  only its own agent's transcript, helpers, diffs and approval requests.
  `/agents` lists the agents (state, conversation, attached clients);
  `/agents <name>` switches this `rkat-tui` to another one. A
  conversation belongs to the agent that ran it: `/resume` of another
  agent's says where it is, and one whose agent quit is seated again
  under that agent's name. `--list` prints one line per agent under its
  daemon; the attach cursor is kept per agent.
- The agents of a workspace are wired to each other: each can message the
  others with the engine's peer tools (`peers`, `send_message`, …, behind
  the same gate), an idle agent wakes for a message, and the receiving
  `rkat-tui` shows it labelled with the sender. Every agent's instructions
  name it and the norms for working beside the others (announce the files
  you change, leave a file another agent announced alone, say when you are
  done, keep messages short), and a note tells each agent who works here
  and when one joins or leaves.

### Changed

- A workspace's state moved out of the workspace, into
  `~/.rkat/tui/workspaces/<id>/`, each directory with a `workspace.toml`
  naming the workspace it belongs to. Nothing is migrated: a
  `<workspace>/.rkat/tui/` of an earlier release is never read (a start
  says once that it is unused and can be deleted), so a workspace starts
  with empty history once. A workspace that is moved or renamed no
  longer keeps its sessions by itself: the start in the new place gets
  fresh state and offers the old one. The audit log moved there too,
  `/export` writes its default file there, and the daemon's endpoint
  directory is removed again when the daemon stops.
- The workspace's own realm files are yours now that nothing in the
  workspace can reach them: a hand edit of its `tui.toml` applies (no
  `/rules trust` needed), its `config.toml`'s `[confinement]` is a floor
  for the shell as a `--state-root` realm's always was (it was ignored
  with a notice), and its endpoints and MCP servers need no `/trust`. A
  realm placed inside the workspace with `--state-root` keeps the digest
  checks.
- `/quit` retires this rkat-tui's agent when other clients are attached
  to other agents (a running turn is interrupted first); its
  conversation stays for `/resume` and `--resume`, and nothing can wake
  it. With other clients on the same agent it just leaves; as the last
  client the P2 rule holds (stop when this rkat-tui started the daemon
  and no agent was left detached, else ask), and "terminate" retires
  every agent before the daemon stops. `/detach` (and a hangup) leaves
  the agent running; the detached flag is the agent's.
- `rkat-tui` beside a live daemon starts a new agent of its own (the
  first `rkat-tui` in a workspace still takes the daemon's first one) and
  names the agents already working there; `-c` joins the most recently
  active agent, and `--resume <ref>` the agent that owns that
  conversation, seated again on it when that agent quit.
- The agent always runs in the workspace's daemon, never in the
  `rkat-tui` process: closing the terminal no longer ends its turn, and
  any number of TUIs can attach to it. An `attach` key in a `tui.toml` is
  ignored with a start-up notice.
- `/compact` and `/compact now` ask the engine to compact at the
  session's next model call instead of rebuilding the session; when the
  check finds nothing to fold, the transcript says so with the reason.
- `/model` switches the session's model in place instead of rebuilding
  the session; helpers delegated afterwards still run on the new model
  (the engine's `delegate` reads the session's current model).
- `/rewind`, `/clear`, `/resume`, `/model`, `/init` and `/compact` share
  one idle check and one notice for what is in the way: `/model` and
  `/init` are refused while a `!` command runs, and `/clear` and `/resume`
  while a message waits to be sent.
- `--provider` is an alias of `--endpoint` (it was a separate flag that
  conflicted with it).
- The `mob` and `mcp` cargo features are gone: helpers, MCP servers and
  the daemon are always built.
- A host that refuses the confined launch's private `/proc` mount runs
  the command without `/proc` instead of refusing it.
- Built on Meerkat 0.9.11 from crates.io.

### Removed

- The in-process runner and `--no-attach` (with the dialog that offered to
  run beside a live daemon).
- The `/rewind` menu: the daemon cannot rewind a conversation (the engine
  refuses a transcript fork of a mob member's session; a same-identity
  rewind is designed upstream, lukacf/meerkat#2126, not yet built).
  `/rewind` and Esc Esc on an empty input say so.
- The TUI's own `fork` tool: the agent forks itself with the engine's
  `fork_off`, behind the same gate.
- The hidden no-op `--mouse` flag (`--no-mouse` stays).

### Fixed

- A new agent's attach is answered at once while another agent (or its
  helper) runs a turn. The note telling the busy agent that one joined
  held the seating lock until that turn ended, so the attach waited 30 s
  or more; it now lands when the busy agent's turn ends.
- An agent lost in a failed `/new` or `/resume` (the new conversation
  failed to start and the old one could not be seated again) is no longer
  listed as live: the daemon drops it, tells the other agents it left, and
  the error says it was lost. Its conversations stay for `--resume`.
- A prompt that quotes a message from another agent is shown as the
  prompt it is, not as a message from that agent: a peer message is
  recognised only when the engine's framing opens the input.
- Only a refused connection proves an endpoint file stale. A daemon no
  longer starts beside a file whose address did not answer within 2 s or
  that cannot be read (it says so and names the file), and `--list` shows
  such a file as `unknown (…)` and keeps it, instead of `stale`.
- An agent whose state the daemon cannot read is shown as `unknown` in
  `/agents` and `--list`, not `idle`, and retiring it cancels its turn
  anyway. Before, a failed status read retired a running agent without
  cancelling its turn.
- The daemon no longer buffers without limit for a client that stops
  reading (a suspended `rkat-tui`, a stalled tunnel): when the client's
  queue (4096 lines) stays full for 5 s, the daemon drops the connection,
  and the client reconnects and catches up. A failed `accept` (for
  example out of file descriptors) no longer ends the daemon: it logs it
  and accepts again 100 ms later.
- A daemon's stop removes the per-workspace directory of its endpoint file
  once it is empty, and the test suite no longer writes into the user's
  state directory (one test left a `rkat-tui/<key>/` directory there on
  every run).
- A failed first attach is no longer final and silent. The client retries
  it three times with a growing wait (starting the daemon again when it
  may), and the status bar says why it failed, when it retries and where
  the daemon's log is (`daemon exited (exit status: 3) · retry 2s · log
  …`). Before, the status said `detached` and nothing happened again,
  and `/detach` and `/quit` acted as if a daemon had been reached.
- An approval the human gives is never turned into a denial afterwards:
  rkat-tui recorded every escalation in the realm's approval store and
  denied the call when that write failed, even after the human allowed
  it. Nothing read those records, so the record is gone.
- The `+ N files` line under a prompt with `@` mentions names what was
  actually attached as the prompt was sent, and the `@` menu lists the
  workspace without blocking the UI.
- A slash command with wrong arguments answers with the usage its
  completion row shows (`/resume`'s two usage texts disagreed).

### Before the daemon (2026-11-08)

The harness as it moved out of the Meerkat monorepo (lukacf/meerkat PR
2069) into this repository, and the Claude Code parity work that
followed, before the daemon: the agent ran in the `rkat-tui` process.
`docs/ux-parity.md` is the parity audit.

#### Added

- `/rewind` (aliases `/checkpoint`, `/undo`), and Esc Esc on an empty idle
  input, open a menu of the session's messages (newest first, one numbered
  line each; ↑/↓, Enter, Esc). Choosing one forks the session just before
  that message through the engine's transcript fork
  (`SessionServiceTranscriptEditExt::fork_session_at`) and switches to the
  fork: the transcript is cut before the message, the helper panel loses
  what came after, the message goes back in the input, and a notice names
  the previous session, which keeps the whole conversation (`/resume` it).
  Idle only, with nothing queued; files are not restored, and the fork's
  task checklist starts empty (the engine scopes tasks by session).
- Ctrl+T shows or hides a task checklist under the input (before the
  helper panel): the session's tasks from the engine's task store
  (`task_create`/`task_update`), `☐` pending, `◐` in progress, `☑` done,
  newest-updated last, at most five lines (`… and N more`). A resumed
  session gets its list back; the choice is saved as `[ui] checklist` in
  the realm `tui.toml` (default hidden), and the idle editor title names
  Ctrl+T while there are tasks.
- `rkat-tui`, an interactive terminal coding harness on the Meerkat
  engine. Its sessions run on the runtime-backed session
  path (durable Queue/Steer input while the agent works, interrupt, resume
  with `-c`/`/resume`); every tool call, helpers' included, passes an
  auto-mode approval gate (tiers, user rules, a reasoning-blind two-stage
  classifier, escalation dialogs) installed as the build's
  `ToolDispatchAdmission`; the shell runs under OS confinement by default
  (`--confinement workspace|strict|off`, `--no-confinement`); models come
  from live endpoint lists with hot swap and a per-model cost readout;
  `delegate` runs helpers as mob members, on models only an endpoint lists
  too and on the model a `/model` switch chose; instruction files (user-global
  `SYSTEM.md`/`AGENTS.md`, ancestor `AGENTS.md`, `AGENTS.local.md`),
  `/remember`, notes (`note_*` tools, indexed as untrusted context), native
  semantic memory, MCP servers from `mcp.toml` (the project file's only once
  trusted with `/trust` or `--trust-project`), and a native/warn/auto
  policy over native auto-compaction with `/compact now`. Configured
  endpoints and the `[models.<id>]` profiles of models only an endpoint
  knows are written into the realm's own `config.toml`, so `rkat`, REST and
  RPC resolve a TUI session's binding too; a realm `[confinement]` is a
  floor the command line may only narrow; turns end with typed outcomes
  (completed, failed, interrupted, pending, unknown); the cost readout
  states its confidence. Endpoint keys come from the environment only
  (`api_key_env`; an inline `api_key` refuses the start, and the realm
  document records only the variable's name). A host that cannot confine
  refuses the start with an explanation of what blocked it (on Linux the
  user-namespace switches, with the AppArmor profile or sysctl that fixes
  it) and how to run unconfined; `--confinement-read`/`--confinement-write`
  add directories to the sandbox; a confined command inherits none of
  `rkat-tui`'s environment but a curated set of it (`PATH`, `HOME`, locale,
  TLS roots, proxies and a few tool directories; provider API keys stay
  outside). User-global files live in `~/.rkat/tui`
  (`--config-dir` overrides; an existing `~/.config/rkat-tui` is used with a
  notice until the new directory exists). File tools never write the gate's
  own `.rkat/` state or `.git/`, nor, wherever they lie, the git directories
  governing the project (an enclosing repository's, a worktree's and its
  common dir), the realm directory or the user config directory (sealed
  paths: denied before the classifier and any allow rule, for every target
  of a patch), nor a file with another hard link, and `fork_off` is denied
  to sessions and helpers (a fork child would seat outside the approval
  gate); the session's own `fork`
  tool runs a helper that starts with the session's conversation, rendered
  as text, as context, behind the same gate. A mouse copy goes out with
  OSC 52 to the clipboard and the primary selection, written after the
  frame, and its notice says the terminal must support OSC 52. An
  escalation that arrives over an open model or resume picker parks the
  picker and restores it once no escalation remains, and a held Enter or
  Esc never answers, selects or closes a dialog. A prompt re-sent with the
  same text right after a failed turn (which already committed it) retries
  that turn instead of adding the message twice. Helpers nest at most
  `[helpers] max_depth` levels (default 1: helpers cannot delegate); a
  helper at the limit has no tool that starts helpers and a deeper spawn
  is refused. Each helper shows as one live header line (state, calls,
  last activity, tokens) that Ctrl+↑/↓ and Enter, a click or
  `/helpers expand|fold` open to show its work (`[ui] helpers` sets the
  default). An interrupt abandons the messages still queued for the
  running turn before it stops the run, so none of them is lost or runs
  twice, and the turn is reported as interrupted, not failed; ↑ on an empty
  input pulls back the steers the runtime still holds queued without
  stopping the run, and the first Ctrl-C (or Esc) after a mouse selection
  only clears the selection. The terminal is asked for keyboard disambiguation only, not
  event types: kitty 0.32 sent a Backspace release as a second press byte,
  which deleted two characters per keystroke. A call the classifier
  allowed shows a dim `allowed by classifier: <reason>` line under its tool
  block, a denial's tag reads `[classifier checked N files, M searches]`,
  and a block for lack of user intent asks the agent to get the user's
  confirmation instead of claiming the user never asked. A file-tool write
  to a protected path in the project (`.env*`, `build.rs`, `.cargo/`,
  `AGENTS.md`, `.gitmodules`, …) always asks you in the escalation dialog,
  naming the path and why, instead of going to the classifier; an allow
  rule for the path (the dialog's "allow and add a rule") stops the prompts.
  Running helpers (and those finished this turn) are listed in a panel
  under the input; Ctrl+↑/↓ select one and Enter on an empty input opens
  its full view (its prose, reasoning, tool calls and nested helpers) in
  place of the transcript until Esc. Long lines in the input editor wrap
  at the field width instead of scrolling sideways; the field grows up to
  six rows and ↑/↓ move by wrapped rows. See `docs/manual.md`. Keys,
  commands and the approval prompt follow Claude Code where both have the
  feature (`docs/ux-parity.md` is the audit):
  - Ctrl+Enter sends now (interrupts a running turn and sends the input
    next, like Ctrl-X Ctrl-S) instead of inserting a newline; `\` right
    before Enter at a line end inserts a newline.
  - Ctrl+O shows or hides detail: the last reasoning block and the last
    tool output together (one key instead of two); a click still toggles
    one tool.
  - A kill ring: Ctrl+K (kill to the line end), Ctrl+U, Ctrl+W,
    Ctrl/Alt+Backspace and Alt+D (kill the word after) push the deleted
    text; Ctrl+Y yanks it and Alt+Y cycles older kills.
  - `/copy [N]` copies the latest (or Nth latest) answer, now that Ctrl+Y
    yanks. A mouse selection still copies on release.
  - Ctrl+P/Ctrl+N work as ↑/↓ in the editor (rows first, then history).
  - Esc twice (within 1.5 s) on an idle draft clears it and saves it to
    the prompt history.
  - Ctrl+_ undoes input edits (by word, kill, yank, paste or clear; 64
    steps).
  - Ctrl+S stashes the input and, on an empty input, restores it (Ctrl+X
    Ctrl+S still sends now).
  - Ctrl+L clears the screen and redraws it.
  - Shift+Tab switches auto mode off and on (as `/auto off|on`; now it
    cycles the permission modes, below).
  - Ctrl+D on an idle empty input asks for a second press (within 3 s)
    before quitting.
  - Esc (like Ctrl+C) always interrupts a running turn, and the messages
    queued behind it are sent as the next turn instead of returning to the
    editor; ↑ on an empty input takes queued messages back.
  - `/compact` compacts (as `/compact now`, kept as an alias, did);
    `/compact status` shows the policy; `/compact <instructions>` compacts
    with a warning that focus instructions are not supported yet.
  - `/permissions` is the rules command (`/rules` stays an alias),
    `/usage` the cost readout (`/cost` and `/stats` are aliases), and
    `/tasks` an alias of `/helpers`.
  - `rkat-tui --resume` without a reference opens the session picker.
  - `/context` prints the context gauge, the compaction policy and what the
    context holds (system prompt size, message counts, instruction files,
    rkat-tui's own tools).
  - `/status` prints the version, model and endpoint, session id,
    workspace and realm, confinement, auto mode, the trust state of
    `tui.toml` and of each project input `/trust` covers, and the helper
    depth limit.
  - `/export [file]` writes the transcript as plain text (default
    `.rkat/tui/export-<session>-<time>.txt` in the workspace).
  - `/init` asks the agent, as an ordinary turn, to write or update the
    workspace `AGENTS.md` (commands, layout, conventions; under 200 lines).
  - A terminal bell rings when a turn ends or an approval prompt opens
    while the terminal is unfocused (focus reports); `[ui] bell = false`
    turns it off.
  - Alt+P opens the model picker (as `/model`) and keeps the draft.
  - Ctrl+Z suspends to the shell on Unix (`fg` resumes).
  - `/memory [user|system]` opens the workspace `AGENTS.md`, the user
    `AGENTS.md` or `SYSTEM.md` in `$VISUAL`/`$EDITOR`/`vi`; an edited
    `AGENTS.md` reaches the live session, `SYSTEM.md` the next one.
  - Ctrl+G and Ctrl+X Ctrl+E edit the input in `$VISUAL`/`$EDITOR`/`vi`.
  - The approval prompt lists numbered choices with a cursor ("1. Yes,
    allow once", "2. Yes, allow and add a rule (<the rule>)", "3. No, and
    tell the agent what to do instead (Esc)"): ↑/↓ or `j`/`k` and Enter,
    or `1`-`3`, pick a row; `y`, `a`, `n`/Esc still answer, `s` stops the
    turn and Ctrl+C stops every prompt (a footer hint, not a row).
  - Row 3 opens a one-line comment field: Enter denies with the text, which
    the model receives in the tool error (`denied by the user: <text>`, with
    an instruction to follow it) and the approval record keeps; an empty
    Enter or Esc denies without a comment.
  - `?` on an empty input opens a shortcut panel (the keys part of
    `/help`); any key closes it. The idle editor title hints `? keys`.
  - `/` at the start of a one-line input opens a command menu above the
    input (`/name usage — summary`, filtered by name or alias prefix):
    ↑/↓ choose, Tab completes, Enter completes a partial name and sends a
    complete one, Esc closes it for the word. The parser, `/help`'s command
    list and the menu share one `CommandSpec` table.
  - `@` at the start of a word opens the same menu on workspace paths
    (`.gitignore` honoured, sealed paths left out; a directory keeps the
    menu open on its contents). On send, each `@file` is attached after the
    prompt as a `<file path="…">` block (UTF-8, 64 KiB per file, 256 KiB in
    all; binary or over-budget files as a one-line note) and each `@dir` as
    a listing; the transcript keeps the typed text with a `+ N files` line,
    and sealed paths are never read.
  - `!` shell mode: an input starting with `!` (not `!!`; `\!` sends a
    literal `!`) runs the rest as a command under the agent shell's own
    confinement and curated environment, in the workspace, with no approval
    prompt, for at most `[shell] user_timeout_secs` (default 120). Its
    output (stdout and stderr together, 64 KiB at most) shows as a foldable
    `$ <cmd>` block with the exit code and joins the session as durable
    system context (`[shell run by the user] $ <cmd>`, output, `(exit N)`),
    which the model reads on its next call; no turn starts. A command typed
    while the agent works waits as a queued `$ <cmd>` and runs when the turn
    ends, before the queued messages are sent; ↑ takes it back, and Esc or
    Ctrl-C cancels a running one. The editor title says when the input is in
    shell mode.
  - Permission modes, as in Claude Code: Shift+Tab cycles manual → accept
    edits → plan → auto, `/mode [default|accept-edits|plan|auto]` shows or
    sets one, and the status bar names it (`manual`, `accept edits`,
    `plan`, `auto (deny …)`; a denial pause still shows `auto: PAUSED`).
    Manual mode (`/auto off`, `--no-auto`) asks you for in-project edits and
    for what the classifier would review, instead of denying the latter;
    accept-edits mode allows in-project, unprotected edits and asks for the
    rest; plan mode denies writes and commands that are not read-only with a
    tool error asking for a plan, tells the session through a system note
    when it starts and ends, and opens a "Plan ready" dialog (proceed in
    accept-edits mode, keep planning, or proceed in manual mode) when a turn
    ends with an answer; auto mode is unchanged. Helpers act under the
    session's mode. The mode is saved as `[auto] mode` in the realm
    `tui.toml`, so the next start comes back in it; a project `tui.toml`
    cannot choose one. The idle editor title hints `Shift+Tab mode`.
- `[thinking]` in `tui.toml` sets Anthropic thinking per model
  (`"adaptive"`, `{ enabled = <budget> }`, or `"default"` for no
  override), keyed by `endpoint:id` or a bare id. It rides in the
  session's provider params on create, `/model`, resume and rewind, so a
  proxy alias outside the catalog can get the engine's thinking-block
  binding control (lukacf/meerkat#2095). Non-Anthropic endpoints and types
  a catalogued model refuses are ignored with a start-up notice.

#### Changed

- Built on a git revision of Meerkat `main` with rkat-tui's fixes on
  top (the Linux sandbox's pid namespace and procfs, `[thinking]` for
  uncatalogued Anthropic ids, host trust over the hook bytes the factory
  reads), all merged upstream since (lukacf/meerkat #2100, #2104, #2105).
- `rkat-tui` holds a realm lease (`meerkat_store::start_realm_lease_in`,
  surface `rkat-tui`) from the moment the realm's persistence is open until
  it quits, as `rkat-rpc`, `rkat-rest` and the mob host do, so
  `rkat realm prune` and realm inspection see the realm as live while a TUI
  has it open.
- Session references (`last`, `~N`, id prefixes) and the `/resume` picker
  no longer offer a session another surface (`rkat`, REST, RPC) archived.
- rkat-tui's writes into the realm `config.toml` (the configured
  endpoints' connections and the `[models.<id>]` profiles) no longer
  interleave with another surface's write (`rkat config`, REST, RPC); a
  write that lost the race is rebuilt from the new document and retried.

#### Fixed

- A hook written into the project `.rkat/config.toml` while a session was
  being built (for example by a confined shell command) could run once
  without trust: rkat-tui checked the file before the build and the engine
  read it again. The engine's factory now asks the host about the hook
  source bytes it read (lukacf/meerkat#2105), and rkat-tui answers the
  factory's `HookSourceTrust` question over the exact bytes it read: a
  project hook file whose digest the trust record does not hold (and no
  `--trust-project`) has every hook refused when it would run.
- A Rust toolchain runs in the confined shell. Two engine fixes
  (lukacf/meerkat#2100): the Linux
  sandbox gives each launch its own pid namespace and a fresh `/proc`,
  which the rustup proxies, rustc's library lookup, `cargo clippy`, `cargo
  fmt` and the linker wrapper all need (`no /proc/self/exe available`); and
  its filter allows Unix stream and seqpacket socketpairs with IP, which
  std's fork-and-exec spawn path needs (before, rustc could not run its
  linker and cargo could not run rustc: `Operation not permitted`). On the
  TUI side, workspace mode grants `/etc/alternatives` (Debian's `cc` link)
  and `/usr/libexec` (gcc's `collect2` and LTO plugin) read-only. Inside
  the sandbox a command runs as pid 2 under a small init and sees no host process (its `/proc` holds task entries only). A test
  compiles, links and runs a program in the sandbox.
- An escalation whose timeout ran out while it waited behind another dialog is denied before it is shown; before, it could reach the user with no budget left (also the cause of the flaky `parallel_escalations_share_one_timeout_including_the_queue` test).
- The shell lexer no longer treats a carriage return as a blank. bash, dash
  and zsh read `\r` as a word character, so `git status \r# ; rm -rf x` was
  judged as the allowlisted `git status` while the shell ran `rm`. Any control
  character other than tab and newline (`\r`, a `\r\n` line ending, NUL,
  ESC, `\v`, `\f`, DEL, C1) now makes the command unsupported, so the gate
  escalates it.
- One trust decision covers every workspace file that makes the host run
  something: the project `.rkat/mcp.toml`, the project `.rkat/config.toml`'s
  `[hooks]` and the workspace realm's `.rkat/tui/config.toml`
  (`[[tools.mcp_servers]]`, `[hooks]`). Before, only `mcp.toml` had one: a
  cloned repository or a confined shell (which can write `.rkat/` on Linux)
  could plant realm-config MCP servers or command hooks that ran as the host
  at the next launch. Each file's SHA-256 is kept in the existing host trust
  record; until it matches, the file's MCP servers do not start, its hooks
  are overridden by disabled entries (helpers included), and a start-up
  notice names the file, why (never trusted, or changed since) and what did
  not start. `/trust` trusts them all (`/mcp trust` is an alias),
  `--trust-project` for one run (`--allow-project-mcp` is an alias);
  rkat-tui's own writes keep the realm config trusted only when it was
  trusted before. `/status` lists each file's state. An existing realm
  `config.toml` that defines MCP servers or hooks needs one `/trust` after
  upgrading.
