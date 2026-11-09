# Changelog

## [Unreleased]

Moved out of the Meerkat monorepo (lukacf/meerkat PR 2069) into this repository as the `rkat-tui` package. The entries below are the harness's history in that PR; `docs/evidence.md` holds the detailed record and `docs/ux-parity.md` the Claude Code parity audit.

### Added

- `/resume` (and `--resume <id>`) lists the realm's sessions created
  in-process before the daemon, marked "view only, created before the
  daemon": choosing one shows its transcript (`/export` saves it) and the
  agent stays on its conversation, with a notice saying why. The engine
  seats the daemon's agent only on sessions it ran.
- The daemon's endpoint key includes its realm when that is not the
  workspace's own, so an `--isolated` or `--realm-backend memory` daemon
  no longer takes the workspace's endpoint file (a plain start in that
  workspace attached to the throwaway realm). An `--isolated` client
  starts its daemon on the realm it minted. `--list` names such a realm
  after the workspace.
- `--attach=HOST:PORT --attach-token-file <path>` attaches to a daemon
  reached through a tunnel (the daemon still binds loopback only; the
  manual has the `ssh -L` recipe). The token file is refused when its
  group or others can read or write it (any of mode `0o077`).
  `rkat/status` now names the daemon's host. Unless an endpoint file on
  this machine names the daemon (by its token, for its workspace; the
  same host name and path are not enough), its workspace counts as not
  on this machine: `!`, `@` mentions,
  `/remember`, `/memory` and `/export` to a path say why they are off,
  and everything else works. A `/quit` whose tunnel drops before the
  daemon confirms the shutdown says the daemon may still be running.
- `--attach=DIRECTORY` attaches to the running daemon of another
  workspace; `!`, `@` mentions, `/export`, `/remember`, `/memory` and the
  input history then use that workspace. The `--attach` value is `auto`,
  `existing`, a directory (containing `/`, or starting with `.` or `~`)
  or `HOST:PORT`; the `=` stays required.
- `--no-attach` while the workspace's daemon runs says so before the first
  prompt (which daemon, and that running alongside means no memory search
  and two agents in one tree); at a terminal it asks whether to attach,
  run alongside, or stop the daemon first, and with a prompt on the
  command line it shows a notice and runs alongside. `--no-attach -c` on
  the daemon's current conversation says it runs in the daemon instead of
  the engine's "served by another runtime owner".
- When the agent's confined shell would be granted the state directory
  (a workspace that contains `~/.local/state`), the daemon writes its
  endpoint file to `$XDG_RUNTIME_DIR/rkat-tui/<key>/` instead of refusing
  to start; clients and `--list` look in both places. It still refuses,
  with the same message, when neither is safe.
- `rkat-tui --list` (and `rkat-tui-mob --list`) prints every daemon that
  has an endpoint file, one per line: workspace, address, pid, uptime,
  attached clients, current conversation, running or idle; `stale` for a
  file nothing answers at, which is removed when its process is gone. The
  token is never printed.
- Attach mode: `/detach` leaves and keeps the agent running. `/quit` (and
  Ctrl-D) now stops the daemon when this rkat-tui started it and nobody
  detached since; with other clients attached it detaches and says so;
  otherwise it asks "detach (keep the agent running) / terminate (stop
  the daemon)", detach by default, and says when terminating interrupts a
  running turn. Unsent messages are saved either way. A closed terminal
  (SIGHUP) restores the terminal and leaves as a detach. After the UI
  closes, rkat-tui says whether the daemon is still running; a shutdown
  the daemon never confirmed is reported as stopped only once its
  endpoint file is gone, its process ended or its address closed, else
  as possibly still running.
- The daemon knows its lifecycle: how many clients are attached, whether
  anyone detached from it, and which client started it (a start token the
  launching client hands it). `rkat/status` reports them with the
  workspace, pid, start time, current conversation and whether a turn
  runs; `rkat/detach` records a detach and `rkat/shutdown` stops the
  daemon. The client that started the daemon losing its connection
  without stopping it (a hangup, a crash) counts as a detach; the daemon
  keeps running.
- The daemon's endpoint file names the workspace it serves (its canonical
  path) and when it started (`started_at`, RFC 3339); a file an older
  daemon wrote still reads.
- `/context` in attach mode reports what the context holds, as
  in-process: the conversation's system prompt and message counts read
  from the daemon (`rkat/transcript`) and the instruction files on disk.
- `/model` in attach mode switches the agent's session in place through the
  daemon (`rkat/set_model`, the engine's LLM reconfigure, as in-process)
  instead of riding the next prompt as a `mob/turn_start`: the next prompt
  is an ordinary send, and detaching no longer cancels the first turn on
  the new model. Like in-process, a switch waits for the running turn.
- Ctrl+T's task checklist works in attach mode: the daemon reads the
  conversation's tasks from the engine's task store (`rkat/tasks`) on
  attach, after a switch and after each of the agent's task calls, and
  saves the view as the realm's `[ui] checklist` (`rkat/checklist_view`).
- An in-process `/model` switch no longer retires and resumes the session so
  that helpers follow it: the engine's `delegate` reads the session's
  current model (#2102). The session is still rebuilt when the switch moves
  the compaction threshold or needs a new model profile.
- `/compact now` in-process asks the engine to compact at the session's next
  model call (`MeerkatMachine::request_compaction`) instead of rebuilding the
  session twice around a one-token threshold; a long tool loop no longer
  compacts again in that turn. When the check finds nothing to fold, the
  transcript says so with the reason (`RequestedCompactionSkipped`), in both
  modes.
- `rkat-tui-mob` (experimental): a second binary, the workspace's mob
  daemon. It bootstraps the workspace realm exactly as `rkat-tui` does
  (config layers, endpoints, confinement, project trust, instructions, MCP)
  and takes the same engine flags, but hosts the agent as a durable,
  turn-driven mob member (mob `tui`, member `main`) with the TUI's file,
  note and shell tools, behind the TUI's approval gate (tiers, rules,
  classifier, permission modes, escalation timeout; the deny rules are the
  members' application tool policy, which the engine records with each
  member; `[thinking]` applies), and serves Meerkat's own JSON-RPC methods
  to any number of clients over loopback TCP (`--listen`, default an
  ephemeral port on 127.0.0.1). Clients find it, and the token every
  connection must present first with `rkat/attach`, in
  `$XDG_STATE_HOME/rkat-tui/<workspace key>/mob.endpoint` (else under
  `~/.local/state`; mode 0600), outside everything the agent's shell is
  granted. A call that needs a human is one of the engine's durable
  decisions, pushed to every attached client as it opens
  (`decision/notice`) and answered with `decision/decide` as the daemon's
  one principal (`[daemon] principal` in your own `tui.toml`, default your
  login name); decisions a stopped daemon left open are withdrawn at its
  next start, and the decision is the only record (nothing is written to
  `approvals.json`). Its own methods: `rkat/get_mode`, `rkat/set_mode`,
  `rkat/rules`, `rkat/trust`, `rkat/conversations`, `rkat/new`,
  `rkat/resume`, `rkat/transcript`, `rkat/usage`.
- `rkat-tui --attach` (experimental): the TUI as a client of the
  workspace's `rkat-tui-mob` daemon, with the in-process TUI's keys and
  dialogs. `--attach` (= `--attach=auto`) starts the daemon in the
  background when none answers (output in
  `<realm>/logs/rkat-tui-mob.stderr.log`; detached with `setsid` when it is
  on `PATH`), `--attach=existing` does not, `--no-attach` (the default)
  keeps the in-process TUI, and `attach = true` in your own `tui.toml`
  makes attach mode the default. Prompts, steers, Esc, `/model` and the
  model picker (this TUI's endpoints), approvals (every attached TUI shows
  the dialog, the first answer counts), Shift+Tab, `/mode` and `/rules` on
  the daemon's gate, `/trust`, `/remember`, `/memory`, `/compact now`
  (`mob/compact_member`), `/new` and `/resume` (every attached TUI follows),
  helpers (`delegate`, `spawn_member`, and `fork_off`, behind the same gate)
  with the in-process follower view and `/cost`, `!` commands (run by the
  TUI), and the token counter and gauge all work. `/quit` detaches and
  leaves the daemon and a running turn going; a dropped connection is
  re-made with backoff (restarting a daemon that died); a reattach resumes
  the agent's event stream from its `{epoch, seq}` cursor (kept in
  `.rkat/tui/attach.cursor`), and a first attach, or one after a daemon
  restart, shows the conversation from its start. `/daemon stop` shuts the
  daemon down. `/rewind` says why it is not offered (same-identity rewind
  waits on the engine). A daemon too old to push decisions is refused at
  attach with a "restart it" error.
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
- `meerkat-tui`: a new crate whose `rkat-tui` binary is an interactive
  terminal coding harness. Its sessions run on the runtime-backed session
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

### Removed

- The TUI's own `fork` tool (in-process only): the agent forks with the
  engine's `fork_off` in the daemon, behind the same gate.

### Changed

- Attach mode: `-c` and `--resume` are no longer refused. `-c` (and
  `--resume last`) attaches to the daemon's current conversation;
  `--resume <ref>` attaches and switches the daemon to that conversation;
  `--resume` alone opens the picker once attached. A session started
  in-process cannot be resumed in the daemon; the message says so and
  names `--no-attach --resume <ref>`.
- `rkat-tui` now runs the agent in the workspace's `rkat-tui-mob` daemon
  by default: it attaches to the running daemon, or starts one and
  attaches. `--no-attach` hosts the agent in-process (the escape hatch);
  `attach = false` in your own `tui.toml` makes that the default. The
  first attach says what the daemon is instead of calling attach mode
  experimental.
- `--provider` is an alias of `--endpoint` (it was a separate flag that
  conflicted with it), and the hidden no-op `--mouse` is gone.
- The `mob` and `mcp` cargo features are gone: helpers, MCP servers, the
  `rkat-tui-mob` daemon and attach mode are always built.
- `/remember`, `/memory`, `/export`, `!` shell mode, the cost readout and
  the model picker are one implementation shared by the in-process and the
  attach runner; their notices now read the same in both modes ("the agent
  reads it from its next model call … from the next session"), and in
  attach mode a `!` command's output reaching the daemon is bounded by the
  same timeout as every other daemon call.
- The engine is now the crates.io Meerkat 0.9.11 release: every `meerkat-*`
  dependency names `version = "0.9.11"` and no git pin is left. The release
  (tag `v0.9.11`) contains the previous pin `bffcd8f98a`.
- The engine pin moved to upstream `lukacf/meerkat` `main` `bffcd8f98a`, the
  merge of the 0.9.11 release integration (lukacf/meerkat#2124; the
  crates.io publish is pending, so the pin stays a git revision). It carries
  the fixes for the seven test-only reports: fork children inherit the
  source's dispatch admission (#2112), the compaction completion is
  delivered after a recovery and compact-now arrives as `session/compact`
  and `mob/compact_member` (#2113), `mob/stream_open` takes an
  `{epoch, seq}` resume cursor and parameter refusals carry the request id
  (#2114), decision openings are pushed in process (`subscribe_openings`,
  #2115), session events go to the connection that started the turn
  (#2116), `mob/fork_member` is an operator method (#2117), and `delegate`
  follows the member's current identity (#2102). This move only follows the
  API; adopting the new methods is a follow-up. The one drift: the engine's
  `ConfigRuntime` now writes through `ConfigStore::set_held` /
  `patch_held` under the document's admission fence, so the realm config
  store forwards the fence and records project trust on those writes too.
- The engine pin moved to upstream `lukacf/meerkat` `main` `74a4f843b7`.
  All three fixes the harness carried on the fork are upstream now, in the
  maintainers' versions: the sandbox fixes (lukacf/meerkat#2100: a pid
  namespace with a static init as pid 1 and the command as pid 2, a
  read-only best-effort procfs, Unix stream/seqpacket socketpairs alongside
  IP, a network namespace of its own without IP), the thinking-block
  binding for uncatalogued model ids (#2104) and host trust over the hook
  source bytes the factory reads (#2105). The `zond/meerkat` fork branch
  `rkat-tui-pin` is retired; the next move is to a crates.io release. A
  host that refuses the confined launch's `/proc` mount now runs the
  command without `/proc` instead of refusing the launch.
- The engine pin moved to `zond/meerkat` `fb1295de4a` (branch
  `rkat-tui-pin`: upstream `main` plus the sandbox fixes of
  lukacf/meerkat#2094 (the relay now tears down and reaps the namespace
  init, so a pid-1 host leaves no zombie, also under a group SIGKILL; an
  IP-denied launch gets a network namespace of its own), the hook-source trust of #2096 (deciding each file once) and the narrowed
  thinking-block binding of #2095). With it, an uncatalogued Anthropic id
  such as a LiteLLM alias gets the binding control when `[thinking]`
  configures adaptive or enabled thinking for it; with thinking omitted the
  request keeps the shape upstream `main` sends.
- The engine pin moved to upstream Meerkat `main` (70121a6157) plus two
  sandbox fixes (a pid namespace and procfs per Linux confined launch; Unix
  stream/seqpacket socketpairs alongside IP). The implicit-mob customizer
  (`mobs::endpoint_models`) is gone: the engine now names the session's
  resolved provider route in its implicit `delegate` mob itself, so a realm
  `[models.<id>]` model delegates without help. The `fork` tool cannot yet
  name that route (the engine's route-aware entry point is not exported):
  on a model outside the catalog it fails with an error saying so, unless a
  `delegate` call already created the session's helper mob. The detected
  read-only file grants (`/etc/resolv.conf`, …) are now judged by a local
  copy of the sandbox backend's rule, which upstream keeps private.
- `rkat-tui` holds a realm lease (`meerkat_store::start_realm_lease_in`,
  surface `rkat-tui`) from the moment the realm's persistence is open until
  it quits, as `rkat-rpc`, `rkat-rest` and the mob host do, so
  `rkat realm prune` and realm inspection see the realm as live while a TUI
  has it open.
- Session references (`last`, `~N`, id prefixes) and the `/resume` picker
  list the realm's sessions through `SessionService::list`, as `rkat` does,
  instead of the raw session store: a session another surface archived is
  no longer offered or matched.
- rkat-tui's writes into the realm `config.toml` (the configured endpoints'
  connections and the `[models.<id>]` profiles, at start-up and on a model
  switch) go through the realm's `ConfigRuntime`, under its writer lock and
  generation sidecar (`config_state.json`) like `rkat config`, REST and RPC,
  instead of writing the document directly: they no longer interleave with
  another surface's write, and a write that lost the generation race is
  rebuilt from the new document and retried. The start-up read takes the
  reader lock too. The trust record follows each write inside the lock.

### Fixed

- In attach mode, the same prompt typed again right after a failed turn
  is a retry ("(retrying the request above)", with a notice) instead of
  a second copy of the message, as in-process.
- Attach mode shows what the approval engine reports, as the in-process
  TUI did: the classifier's note under a tool call it allowed, a line for
  a blocked call or a human's answer, the auto-mode pause notice, the
  classifying indicator and the classifier's usage in `/cost`. The
  classifier of a conversation the daemon restored or `/resume` switched
  to sees what the user asked earlier in it.
- `/model` to a model only a configured endpoint lists works with the
  daemon: the daemon registers the model's profile from the endpoint's
  list (rebuilding the agent on its conversation when the profile is
  new), and the status bar names the configured endpoint after a daemon
  restart. A prompt typed while a conversation switch runs is sent once
  the switch ended, to the conversation it ended on.
- The daemon agent's member-scoped mob queries (`list_members`,
  `member_status`) run without review, like the other mob queries; its
  member-scoped mob actions (`spawn_member`, `retire_member`,
  `wire_members`, …) are listed as reviewed tools.
- The daemon's agent follows the `[auto]` compaction policy
  (`auto.compact = "auto"` compacts at `compact_auto_percent`, `warn`
  never compacts by size), as the in-process runner did. The policy is
  taken when the workspace's daemon mob is first created.
- The daemon counts a client before it answers its attach, so a client
  that has attached is never missing from another client's status
  (`/quit`'s decision, `--list`); an attach whose answer cannot be
  written is not counted and is no detach.
- Attach mode: a prompt on the command line (`rkat-tui "fix it"`) is
  held until the TUI has attached instead of failing as "not attached".
- Attach mode: after a `/quit` the daemon never confirmed, a connect to
  its address that timed out or failed with a local error no longer
  counts as proof it stopped; only a refused connect, its process gone or
  its endpoint file gone does.
- Attached, the editor title and the help say that Ctrl-D, Ctrl-C twice and
  `/quit` detach (the daemon's turn goes on) instead of quitting, and Up on
  an empty input says why a message already sent to the daemon cannot be
  taken back (the engine answers no input id to cancel it by). The manual
  and README name `fork_off` where attach mode has no `fork`.
- Attach mode never showed the daemon's start-up notices (its shell's
  confinement, rule-loading warnings, ignored project settings, semantic
  memory off): the daemon now keeps them (`rkat/notices`) and a TUI shows
  them when it first attaches to that daemon.
- Attach mode showed no diffs of the agent's file changes: the daemon's
  file tools reported none. They now report every change, and the daemon
  pushes it to every attached client (`rkat/file_diff`), shown as
  in-process.
- The `+ N files` line under a prompt with `@` mentions names what was
  actually attached as the prompt was sent (it used to be a separate scan
  at submit time), and the `@` menu lists the workspace without blocking
  the UI.
- A slash command with wrong arguments answers with the usage its
  completion row shows: `/resume`'s two usage texts disagreed
  (`[ref]` and `[last | ~N | id-prefix]`); both are now the latter.
- `/rewind`, `/clear`, `/resume`, `/model`, `/init` and `/compact` share
  one idle check and one notice for what is in the way: `/model` and
  `/init` are now refused while a `!` command runs, and `/clear` and
  `/resume` while a message waits to be sent next or recalled. `/compact`'s
  help and notices no longer say it rebuilds the session (it asks the
  engine to compact at the next model call).
- Quitting a busy attached TUI no longer warns that the turn is
  interrupted: attach mode detaches and the turn goes on in the daemon, and
  the warning now says so.

- `--isolated` with `attach = true` in your `tui.toml` no longer reads two
  different throwaway realms: the workspace and its `tui.toml` layers are
  read once at start and handed to whichever mode runs.
- Attach mode caught up with the in-process TUI: `/cost` is priced (the
  `[pricing]` overrides, the community list and the endpoints' prices; it
  showed `$?`), the `!` hint says "unconfined" under `--no-confinement`,
  messages still held at quit are saved to `tui_unsent.txt` and announced
  at the next start, and `--resume`/`-c` with `--attach` are refused with an
  error instead of being ignored.
- Attach mode: a conversation switch read on a connection that was lost
  before the client reattached is no longer installed; it named the dead
  connection's stream, and the client then silently dropped every event of
  the agent. A `/resume` the client refused itself (already in that
  conversation, no such conversation) is the only notice that ends a
  switch.
- An approval the human gives is never turned into a denial afterwards.
  The in-process TUI recorded every escalation in the realm's approval store
  and denied the call when that write failed, even after the human allowed
  it; nothing read those records, so the record is gone. The dialog still
  closes when the call times out or is abandoned.
- Attach mode, from a review of the daemon and its clients: the daemon
  refuses `/new` and `/resume` while the agent's turn runs, whichever client
  started it (it retired the member mid-turn); `/resume last` (`~0`, `~N`)
  counts the conversations other than the current one, as in-process (it
  named the current one and was refused); an answered approval dialog no
  longer comes back from a list read before the answer landed; a helper's
  `shell` is judged as the native shell, so allow rules match it as
  in-process; a slow `rkat/*` call (`/resume`, the conversation list) no
  longer holds back the same client's Esc and approval answers; the
  conversation list reads only the first page of each history; a daemon
  restarted in plan mode tells the agent, and leaving plan mode afterwards
  says so; a timed-out call leaves nothing behind in the client.

- The daemon's endpoint file, which holds the attach token, moved out of the
  workspace (`<workspace>/.rkat/tui/mob.endpoint`, readable by the agent's
  confined shell, which could then attach and approve its own escalations)
  to `$XDG_STATE_HOME/rkat-tui/<workspace key>/mob.endpoint` (else under
  `~/.local/state`; directory 0700). The daemon refuses to start when the
  shell's confinement would grant that directory; `rkat-tui-mob
  --endpoint-file` places it explicitly.

- A hook written into the project `.rkat/config.toml` while a session was
  being built (for example by a confined shell command) could run once
  without trust: rkat-tui checked the file before the build and the engine
  read it again. The engine pin moved to `zond/meerkat` `414265f683`
  (branch `rkat-tui-pin`: the previous pin plus "fix: the factory asks the
  host about the hook source bytes it read"), and rkat-tui answers the
  factory's `HookSourceTrust` question over the exact bytes it read: a
  project hook file whose digest the trust record does not hold (and no
  `--trust-project`) has every hook refused when it would run.
- A Rust toolchain runs in the confined shell. Two engine fixes (pinned
  from the `zond/meerkat` fork, branch `fix/sandbox-procfs`): the Linux
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
