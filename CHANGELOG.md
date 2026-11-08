# Changelog

## [Unreleased]

Moved out of the Meerkat monorepo (lukacf/meerkat PR 2069) into this repository as the `rkat-tui` package. The entries below are the harness's history in that PR; `docs/evidence.md` holds the detailed record and `docs/ux-parity.md` the Claude Code parity audit.

### Added

- Approvals in attach mode (experimental): the `rkat-tui-mob` daemon's
  agent and its helpers run behind the TUI's approval gate (tiers, rules,
  classifier, permission modes, escalation timeout). A call that needs you
  is a record in the realm's approval store; every attached TUI shows the
  dialog (listed with `approval/list`) and the first `approval/decide`
  wins, comments, "allow + add rule" and "deny and stop" included.
  Shift+Tab, `/mode`, `/auto resume` and `/rules` act on the daemon through
  its own `rkat/get_mode`, `rkat/set_mode` and `rkat/rules` methods. The
  deny rules are installed as the members' application tool policy
  (`ToolConsequencePolicyRegistry`), which the engine records with each
  member. `[thinking]` now applies to the daemon's agent (spawn, rebuild
  and `/model`), and `!` commands work in attach mode (run by the TUI,
  output to the agent's context).
- `rkat-tui --attach` (experimental): the TUI as a client of the
  workspace's `rkat-tui-mob` daemon. It reads `.rkat/tui/mob.endpoint`,
  attaches with its token and drives the orchestrator member with Meerkat's
  own methods: prompts and steers through `mob/member_send`, live output
  from `mob/stream_open`, Esc through `mob/stop_member_run` (a second Esc
  `mob/force_cancel`), `/model <provider>:<id>` riding the next prompt's
  `mob/turn_start`. `--attach` (= `--attach=auto`) starts the daemon in the
  background when none answers (output in
  `<realm>/logs/rkat-tui-mob.stderr.log`), `--attach=existing` does not,
  `--no-attach` (the default) keeps the in-process TUI. `/quit` detaches and
  leaves the daemon and a running turn going; a dropped connection is
  re-made with backoff; every attach catches up from the member's durable
  events (`events/list_since`) and shows each event once (the cursor is kept
  in `.rkat/tui/attach.cursor`). A status-bar segment shows the connection;
  `/daemon stop` shuts the daemon down. The daemon's agent runs without the
  approval gate for now, which the TUI says on attach; `/new`, `/resume`,
  `/rewind`, `/compact`, approvals, `!`, helpers and `fork` answer "not
  available in attach mode yet".
- `rkat-tui-mob` (experimental): a second binary, the workspace's mob
  daemon. It bootstraps the workspace realm exactly as `rkat-tui` does
  (config layers, endpoints, confinement, project trust, instructions, MCP)
  and takes the same engine flags, but hosts the agent as a durable,
  turn-driven mob member (mob `tui`, member `main`) with the TUI's file,
  note and shell tools, and serves Meerkat's own JSON-RPC methods
  (`mob/member_send`, `mob/turn_start`, `mob/stream_open`, `mob/members`,
  `events/list_since`, …) to any number of clients over loopback TCP
  (`--listen`, default an ephemeral port on 127.0.0.1). Clients find it, and
  the token every connection must present first with `rkat/attach`, in
  `<workspace>/.rkat/tui/mob.endpoint` (mode 0600). Clients may disconnect
  and reattach while a turn sent with `mob/member_send` runs; a restart
  restores the member and its session. The member's own calls pass no
  approval gate yet, and the TUI does not attach to it yet
  (`docs/mob-split.md`).
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

### Changed

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
