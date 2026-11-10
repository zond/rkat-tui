# rkat-tui as a client of a running mob

## Status (2026-11-10)

Built and on `main`; the record of each phase, with its probes and test
names, is in `docs/evidence.md`:

| Phase | Date | `docs/evidence.md` section |
|---|---|---|
| P1 the `rkat-tui-mob` daemon | 2026-11-08 | "P1: the `rkat-tui-mob` daemon" |
| P2 `rkat-tui --attach` | 2026-11-08 | "P2: `rkat-tui --attach`" |
| P3 approvals on the daemon side | 2026-11-08 | "P3: approvals on the daemon side" |
| P4 helpers, conversations, detach | 2026-11-08 | "P4: helpers, conversations and detach in attach mode" |
| Review fixes for P1–P4 | 2026-11-08 | "Daemon and attach fixes after the P1–P4 review" |
| P4b attach-mode polish | 2026-11-08 | "P4b: attach-mode polish" |
| P3b and the 0.9.11 adoption | 2026-11-09 | "P3b and the 0.9.11 adoption" |
| Always-daemon P2 lifecycle (`/detach`, `/quit`, hangup, `--list`, endpoint fallback) | 2026-11-09 | "The daemon's lifecycle state and its owner (`rkat/status`)", "`/quit`, `/detach` and hangup in attach mode", "A shutdown without an answer is not reported as done", "`--list` (`daemon::list`)", "The endpoint file falls back to the runtime directory" |
| Always-daemon P1 parity (2026-11-09) | 2026-11-09 | "The task checklist in attach mode (`rkat/tasks`)", "Input history in attach mode (verified, no gap)", "`/model` in attach mode switches in place (`rkat/set_model`)", "`/context` in attach mode", "File diffs in attach mode (`rkat/file_diff`)", "The daemon's start-up notices reach the clients (`rkat/notices`)", "P1 parity: what stays an engine limit, and the texts" |
| Always-daemon P3 default flip (attach by default, `-c`/`--resume`, `--attach=DIRECTORY`, `--attach=HOST:PORT`, `--no-attach` alongside) | 2026-11-09 | "The default flip (P3): attach, else start", "The command line's prompt waits for the first attach", "`-c` and `--resume` under attach; in-process sessions stay in-process", "`--no-attach` beside a live daemon", "`--attach` values and `--attach=DIRECTORY`", "`--attach=HOST:PORT` with a token file" |
| P5 several agents per workspace (members `main-<id>`, a binding per connection, `/quit` retires, start semantics, wiring, the persona and notes, peer messages shown) | 2026-11-10 | "P5: several agents per workspace" (its subsections) |
| Always-daemon P4 (the in-process runner deleted; every test on the daemon rig; `/status` parity; the proxy's method allow-list) | 2026-11-10 | "The endpoint key includes the realm (P4)", "Sessions created before the daemon are view only (P4)", "P4: the in-process tests and where they went", "P4: the in-process runner is deleted", "`/status` reads the daemon's side (`rkat/report`)", "The daemon's proxy serves an allow-list of engine methods", "The proxy forwards only what it parsed", "The daemon's stop waits for the realm's databases to close" |

Phases P1 to P4 of the always-daemon plan are done: every session runs in
the daemon and `rkat-tui` is only its client; the in-process runner and
`--no-attach` are gone, and with them the limit that an in-process TUI and
the daemon could not share a workspace. P5 is done too: one daemon hosts
several agents per workspace, wired to each other ("The member model"
below).

What remains, in order of priority:

| Item | State |
|---|---|
| Steer correlation (ask 6): which client's steer a boundary delivered when several steer one turn | The upstream fix is in progress ("Upstream follow-up: ask 6" below); the client attributes first in, first out until a release carries it |
| `/rewind` on the engine's implementation (ask 1) | The same-identity rewind design is merged upstream (lukacf/meerkat#2126, db8547a532); its implementation is not scheduled. `/rewind` explains why it is not available |
| Seating a member on a non-member session (ask 7) | Sessions the in-process runner created stay view only until the engine can seat the daemon's agent on them |
| MCP servers added or trusted after the daemon's mob was created | They do not reach the agent until the mob is made again (the manual's limits) |
| The governed realm (ask 2, #2122) | Lower priority now that remote attach goes through a tunnel to a loopback daemon (`--attach=HOST:PORT`) rather than a governed connection |

## Direction (decided 2026-11-09): every session runs in the daemon

`rkat-tui` becomes only a client of `rkat-tui-mob`; the in-process runner
is deleted at the end of this work. The reasons: one runner instead of
two (the copied attach runner had already drifted), detach at any moment
without a hand-off, one process owning the per-workspace singletons (the
realm config, the memory notes, the semantic index with its exclusive
lock) so several clients and several agents per workspace need no special
cases, and an agent that is always reachable over Meerkat's own JSON-RPC.
The cost is parity work first, a daemon spawn and handshake at start, an
exact lifecycle rule, and moving the in-process tests onto the daemon rig.
This follows the review batches that touch the UI and the config layer
(they are the client's) and precedes the docs pass, which then describes
the end state.

### Rules (what the user asked for, made precise)

- **Storage.** Everything a user cares about stays under `<workspace>/.rkat/`: sessions, the conversations list, memory notes, the semantic index, rules. Moving the directory carries it. The only out-of-workspace state is the daemon's endpoint file (address, token, pid, workspace path, start time), which is runtime state, is deleted on shutdown, and must stay out of the workspace because the confined shell reads the workspace and reaches loopback.
- **One daemon per workspace, several agents possible.** Several clients attach to the same daemon. "Several rkat-tui in one directory" is several mob members in that daemon, not several daemons: the realm config, notes and the memory index (exclusive lock) are per workspace and belong to one process. Multiple agents per daemon is the last phase and optional.
- **Start.** Default: attach if a daemon is running for this workspace, else start one and attach. `-c` with a running daemon attaches to its current conversation; `--resume <ref>` attaches and asks the daemon to switch (`rkat/resume`). `--attach=DIRECTORY` attaches to that workspace's daemon. `--attach=HOST:PORT` attaches to a daemon reached through a tunnel, with the token from `--attach-token-file` (the daemon keeps binding loopback only; the manual documents `ssh -L`). The `=` stays required because a bare `--attach` followed by a word is a prompt. `--no-attach` is kept for one release as the in-process escape hatch and then deleted with the in-process runner.
- **`/detach`** means "keep going without me": the client exits, the daemon records that it has been detached from, the turn goes on.
- **`/quit`** means "I am done": it stops the daemon when this client started it and nobody has detached from it since. If a detach ever happened, or the daemon does not know (it restarted), the client asks: detach or terminate. If other clients are attached, `/quit` detaches without asking. A terminal hangup, a dropped connection or a client crash is a detach: the daemon exits only on an explicit terminate (`rkat/shutdown` from `/quit`, or `/daemon stop`). Terminating while a turn runs interrupts it; the prompt says so.
- **`--list`** prints every daemon whose endpoint file exists: workspace, address, pid, uptime, live or stale, and for live ones the current conversation, running or idle, and the attached-client count (asked over the wire with the token). Never the token. Stale files (no answer, pid gone) are reported and removed.
- **Starting alongside.** If a daemon is live and the user forces `--no-attach` (while it exists), the TUI says so before the first prompt and offers attach, run alongside (no memory search, two agents in one tree), or stop the daemon first. `--no-attach -c` on the daemon's own conversation gets a plain message, not the engine's served-elsewhere error.

### Phases

#### P1 Parity (attach mode does everything in-process does; done 2026-11-09)
- The agent's fork: the TUI's `fork` tool is built on the in-process session service; in the daemon the agent has the engine's `fork_off`, already gated and followed. Decide per feature: `src/fork/` is deleted with the in-process runner, and the manual stops promising the TUI fork tool. Check nothing in-process-only remains in the agent's tool set (compare `session::tool_set` with the daemon's policy).
- Ctrl+T task checklist: a `rkat/tasks` local method reads the session task store (`tasks.db`) in the daemon; the client renders it as today.
- History ↑ recall: client-local history file; make it work in attach mode.
- Anything else the manual's attach section lists as a limit, except multi-client steer attribution (engine gap, ask 6, stays documented).
- Quit-confirm and other texts that assume in-process.

#### P2 Lifecycle (done 2026-11-09)
- Endpoint file gains `workspace`, `started_at`; the daemon tracks `detached: bool` and `clients: usize` and reports them in `rkat/status`.
- `/detach` (new command; `/quit` stops meaning detach), `rkat/detach` so the daemon records it.
- `/quit` semantics as above, including the owner notion (the client that started the daemon holds a start token or the daemon records the owner connection id), the other-clients rule, hangup = detach.
- `rkat-tui --list` (and `rkat-tui-mob --list` sharing the code).
- Endpoint-file fallback when confinement would expose the state dir: `$XDG_RUNTIME_DIR/rkat-tui/<key>/`, else refuse with today's message. The temp dir is granted to the sandbox, so never there.
- Tests: `tests/attach_lifecycle.rs` with a scripted daemon: quit-stops, detach-keeps, quit-after-detach-asks, other-client-detaches, drop-without-message-keeps, `--list` rows and stale cleanup.

#### P3 Default flip (done 2026-11-09)
- `AttachMode::IfRunningElseStart` (the existing `Auto`) becomes the default; `--no-attach` escape hatch with the alongside notice.
- `-c`/`--resume` under attach (replace the Batch C refusal with the switch).
- `--attach=DIRECTORY`; `--attach=HOST:PORT` with `--attach-token-file`; the client resolves `!`, `@` mentions, `/export`, `/remember` against the daemon's reported workspace (refuse the file-editing ones when the workspace is not local).
- Manual: the attach section becomes the main text; `ssh -L` recipe.

#### P4 Delete the in-process runner (done 2026-11-10)
- Remove `src/runner/` (keep what the workbench absorbed), `src/fork/`, the in-process paths in `src/bootstrap/`, `--no-attach`, `AttachMode::None`; `src/session/` stays only as far as the daemon uses it.
- Migrate `tests/integrator.rs`, `tests/fork.rs`, `tests/compact.rs`, `tests/mobs.rs`, … onto the daemon rig (`tests/common/attach.rs`), keeping each behaviour assertion; delete `tests/common/mod.rs`'s `Rig`. Record in `docs/evidence.md` which in-process test became which attach test.
- CLAUDE.md layout table, README, manual, mob-split (now the architecture doc), changelog.

#### P5 Several agents per workspace (done 2026-11-10)
- A member per agent in the daemon's mob, named `main-<short random id>` (a bare `main` of an older daemon is kept), `/agents` and `/agents <name>`, a member column in the conversations list, a per-agent attach cursor. Approvals, rules and memory stay per workspace. As built: "The member model" below.
- Decided with the user: always wired (every agent to every other, peer tools, peer messages shown labelled), the persona with the names and the norms, join and leave notes; no flag starts a new agent beside a live daemon, `-c` the most recent, `--resume <ref>` the owner (or a new seat); `/quit` retires the agent, the P2 daemon-stop rule applies to the last client; `/detach` keeps the agent live and wired.

### Interaction with the review batches
- Batch D (running): the workbench is what the one runner uses; it was told not to build two-runner abstractions.
- Batch E: dropping the `mob`/`mcp` features is even more right (the daemon is always needed); the config move is unchanged.
- Batch F: unchanged (the UI is the client).
- Batch G: the test-doubles dedupe between the two rigs is moot (one rig survives P4); the docs work is done after P4 so it describes the end state. The adoption test for the #2102 fix moves onto the daemon rig.

### Upstream follow-up: ask 6 (steer correlation), 2026-11-09 13:46 UTC
The Core maintainer rated it P2 and started an additive fix: an optional `admission { session_id, input_id }` on `MobMemberSendResult` for an unkeyed send a runtime-backed member admitted (absent before admission, for non-runtime or peer-only members, keyed sends and older servers). When it ships in a release, the client keys its steer acknowledgement on `input_id` matched against `boundary_append_applied.input_id` instead of first-in-first-out, falling back to FIFO when `admission` is absent (older daemon). Belongs in P2 or as soon as the release exists; drop ask 6 from the limits then.

## Decision

Decision (2026-11-08): rkat-tui becomes two binaries. A daemon embeds the
Meerkat engine, holds the realm, runs one durable mob member as the
orchestrator and exposes Meerkat's own JSON-RPC methods over loopback TCP
(first decided as a Unix domain socket; changed the same day, see "Wire and
security"). The TUI becomes a client of that endpoint: it attaches, sends
turns, renders the member's event stream, answers approvals, and can detach
while the mob keeps running. The protocol is Meerkat's existing method
router and wire types, not a protocol of our own, so other clients can
attach and the consent and decision methods come from the engine.

## Why

- It is the shape the engine already serves: Meta's interactive runtime runs
  its agents as members; the JSON-RPC host exposes `mob/spawn`,
  `mob/turn_start` and `mob/stream_open`.
- It is where the engine's owners wanted the line (zond/rkat-tui#1): the
  engine decides whether a call runs, records policy, owns fork admission
  and lineage, delegation and compaction; the application owns the coding
  allowlist, the classifier, tool selection and consent presentation.
- The orchestrator as a mob member gets the engine's native `fork_off`,
  `fork_helper` and `delegate`, member recovery, and the durable
  application policy binding, none of which a standalone session can have.
- Detach and reattach, long-running work, and several clients on one mob
  fall out of the process split.

## Architecture as built

```
rkat-tui-mob (daemon, src/daemon/, one per workspace realm)
  bootstrap shared with the TUI (realm, config, factory, confinement,
    project trust, instructions, MCP); escalation records = Decisions
  engine: AgentFactory + SessionRuntime + our MobMcpState
    SpawnMemberCustomizer for every member, re-supplied on resume: gate
      (ToolDispatchAdmission), tools, persona, auth, consequence policy
    agents: mob `tui`, members `main-<id>` (one per agent; an older
      daemon's `main` kept), profile `main`, host root, TurnDriven,
      durable, wired to each other; the first seated by the daemon at
      start (or restored), the others when a client asks for one
    agent mob tools (delegate, mob_*, council) + gated fork_off
  decisions: subscribe_openings -> decision/notice to attached clients;
    decision/decide -> AuthenticatedDecisionEvent as the daemon's principal
  rkat/* local methods (mode, rules, conversations, transcript, usage, trust)
  MethodRouter over loopback TCP after the rkat/attach handshake
    (RpcServer per connection, shared runtime, skip_shutdown_on_eof)
  realm lease + $XDG_STATE_HOME/rkat-tui/<key>/mob.endpoint

rkat-tui (client, src/client/)
  reads the endpoint file, attaches with the token and the agent it wants
    (new, recent, a name, a conversation; starts the daemon if none
    answers); refuses a daemon without decision/notice+decide
  mob/member_send for prompts and steers (detach-safe); rkat/set_model
    for a /model switch, in place (model, provider, auth_binding)
  mob/stream_open with the saved {epoch, seq} cursor; rkat/transcript and
    events/list_since to render a conversation from its start
  mob-wide mob/stream_open per mob for the helpers' follower view
  mob/stop_member_run, mob/force_cancel for Esc
  decision/notice + decision/decide for the approval dialog
  mob/compact_member (fallback session/compact) for /compact
  local: rendering, editor, `!` shell, model picker, history, notes
```

## Wire and security

Transport: loopback TCP (`--listen 127.0.0.1:<port>`, default port 0), not
a Unix socket: it is the loop `rkat-rpc --tcp` already runs, it forwards
over SSH, and it behaves the same on every platform.

Endpoint file: `$XDG_STATE_HOME/rkat-tui/<key>/mob.endpoint` (else under
`~/.local/state`; `<key>` is the first 32 hex digits of the SHA-256 of the
canonical workspace path; directory 0700, file 0600), one JSON line
`{"address", "token", "pid"}`, removed on shutdown. `--endpoint-file`
overrides it, and the TUI passes the path it waits on to a daemon it
starts.

Each connection must first attach (`src/daemon/handshake.rs`); anything
else is answered with a JSON-RPC error and closed before the engine sees
it. The token never travels: the client sends `rkat/attach {"nonce"}`
with a fresh client nonce, the daemon answers `{"challenge": {"nonce",
"proof"}}` with its own nonce and `HMAC-SHA256(token, daemon label ‖
client nonce ‖ 0 ‖ daemon nonce)`, the client checks that proof and only
then sends a second `rkat/attach {"proof"}` keyed the same way under the
client label, answered with `{"attached": true, …}`. A process that took
the port of a daemon that is gone (or that answers in the shutdown
window) cannot produce the daemon's proof, so the client refuses it
(`ClientError::Unproven`) having sent nothing it could use; a daemon of
before the handshake refuses an attach without a token, which the client
reports as `ClientError::DaemonTooOld`. A client of before the handshake
still attaches with `{"token"}`; its token was sent by then either way.

**The token is the whole access control.** A connection that presents it can
do everything a TUI can: answer decisions, switch the permission mode, add
rules. Nothing on a loopback TCP connection proves which program sent it, so
the daemon does not try to tell a TUI from any other holder of the token.
The token must therefore stay out of the agent's reach: the agent's confined
shell is granted the workspace (`.rkat` included; the Linux backend has no
deny lists), the temp dir, the Cargo home's `registry/` and `git/` (the
rest of it read only, entry by entry, its credentials not at all) and the
user cache dir, and loopback is reachable from it, so the file lives in
the user's state directory, which none of these hold. The daemon refuses
to start when its confinement plan grants the file's directory after all
(`DaemonError::EndpointExposed`: a workspace that contains the state
directory, an `XDG_STATE_HOME` in the temp dir, an extra
`--confinement-read`). The agent's file tools (and the classifier's) never
read the endpoint directories either: they are secret roots of the
project root, sealed at tier 0 for reads, searches and writes
(`ProjectRoot::with_secret_roots`). With `--no-confinement` the shell is
the trusted host and can read the file like any other process of the
user.

A client on the daemon's own machine compares the agent shell's
confinement the daemon reports (`rkat/status` `shell`) with what its own
flags give before attaching, and refuses a daemon whose shell may do more
(`ClientError::ShellWider`). The comparison is local only: a daemon
reached on another machine (`--attach=HOST:PORT` to a remote workspace)
is not compared at all, because its grants name that machine's paths
(`Reached::apply` clears the client's expected shell for a remote target,
and `establish` skips `check_shell` without one). Such an attachment
gives no assurance about the remote agent shell's confinement; it is the
remote daemon's command line that decides it.

What the confined shell can still write, until engine ask 8 lands: on
Linux the whole workspace, `.rkat/` and `.git/` included (hooks, the
workspace realm). Nothing rkat-tui reads back from there is trusted on
sight: the workspace realm's `config.toml` never sets the confinement and,
unless its digest is the host trust record's, names no endpoint, key
source, skill source, MCP server or hook; the realm `tui.toml` only denies
unless trusted by digest; `tui_helpers.json` is not read unless it is what
the daemon last wrote; a realm directory git tracks refuses the start.
The engine's own realm files (the sessions database, whose human-authored
messages seed the classifier on resume, the approval and custody stores)
have no such guard, and `.git/hooks` run unconfined by git later: both
wait on the read-only bind.

On the approval side: the classifier sees the action it reviews whole or
does not decide it (over 48 KiB the human is asked, "too large to
review"), and records as the user's words only what was typed: an `@`
mention's file content is a `[context]` line naming the file.

The token grants access, not identity: every decision is recorded as the
daemon's one principal (`[daemon] principal` in the user's `tui.toml`,
default the login name); the daemon is single-person by design.

What is not isolated, by design (one principal per daemon):

- Decision notices (`decision/notice`, `rkat/decision_ended`) go to every
  attached client (`daemon::decisions`); each carries the full approval
  rendering (command lines, diffs) of any agent's request, and the client
  filters by its agent's session (`client::approvals`). No client should
  be assumed blind to another agent's requests.
- The proxy binds a connection's engine member actions to its agent
  (`Local::observe`, `NOT_YOURS`), which keeps a client from steering the
  wrong agent by mistake; a token holder can still attach to any agent by
  name, so the binding is not an access control.
- A peer agent can wake an idle agent and start a turn no client prompted.
  Such a turn is unattended (A6, `approval::attendance`): while no client
  is attached to the agent, a call that needs review (the classifier's,
  or a question to the human) is denied with `DenyReason::Unattended`
  without a classifier request, and the notice is queued for the agent's
  next client (`rkat/missed`); what the mode allows without review runs.
  Attended is positive evidence only: a client bound to the agent now, or
  the call's run (the dispatch context's run id) is one the engine saw
  start after a client's `mob/member_send`/`mob/turn_start` to the
  session (`ApprovalEngine::human_input`), or that input waits and the
  run's start is not seen yet. The peer's text is never recorded as user
  input (only client sends reach `record_user_input`).

### The `rkat/*` local methods

The engine's wire has nothing for an application's permission mode, rule
list, conversation list or trust record, so the daemon answers these
itself before a line reaches the router (`daemon::serve` proxy,
`daemon::local`, `daemon::decisions`); they are the only non-engine
methods on the wire, and `daemon::protocol` types each one for both ends:

| Method | Params | Result |
|---|---|---|
| `rkat/attach` | `{"nonce", "start_token"?, "probe"?, "agent"?: {"choice": "new"\|"recent"\|"agent"\|"conversation", "name"?, "reference"?}}`, then `{"proof"}` (an older client: `{"token", …}` alone) | `{"challenge": {"nonce", "proof"}}`, then `{"attached": true, "agent"?, "seated"?, "build"?}` (the first lines only; `build` is the daemon's version and commit; the start token marks the client that launched the daemon, a probe is not counted as a client and is bound to no agent; no `agent` is `recent`) |
| `rkat/status` | `{}` | `{"workspace", "host"?, "pid", "started_at", "clients", "detached", "owner", "session_id"?, "running", "agent"?, "agents": [{"name", "session_id"?, "running", "clients", "detached"}], "shell"?}` (`shell`: the agent shell's confinement the daemon installed, the engine's `ShellConfinement`, which a client compares with its own flags' before attaching; `session_id` and `running` of the caller's agent, whose `running` is true when its state cannot be read; an agent row's `running` is `null` then; `detached`: a live agent runs without a client) |
| `rkat/detach` | `{}` | `{}` (the caller's agent is marked detached: a later `/quit` asks) |
| `rkat/retire` | `{}` | `{"agent", "session_id"?}` (the caller's agent leaves the mob; a running turn is interrupted; its conversation stays) |
| `rkat/shutdown` | `{"retire"?}` | `{}` (the daemon removes its endpoint file and shuts down; with `retire` every agent retires first) |
| `rkat/get_mode` | `{}` | `{"mode", "paused", "denials"}` |
| `rkat/set_mode` | `{"mode": "default"\|"accept-edits"\|"plan"\|"auto"}` | `{"status", "notice", "warning"?}` |
| `rkat/rules` | `{"op": "list"}`, `{"op": "add", "deny", "tool", "pattern"?}`, `{"op": "remove", "n"}` | `{"ok", "text"}` |
| `rkat/conversations` | `{}` | `{"conversations": [{session_id, last_used, messages, title, current}]}` |
| `rkat/new` | `{}` | `{"session_id", "warning"?}` |
| `rkat/resume` | `{"session_id"}` | `{"session_id", "warning"?}` |
| `rkat/transcript` | `{"session_id"}` | `{"messages": [Message]}` |
| `rkat/usage` | `{"session_id"}` | `{"input_tokens", "output_tokens"}` |
| `rkat/trust` | `{"op": "project"\|"rules"}` | `{"ok", "text"}` |
| `rkat/set_model` | `{"model", "provider"?, "auth_binding"?}` | `{"session_id", "model", "provider"}` (refused while a run is open) |
| `rkat/notices` | `{}` | `{"notices": [{"warn", "text"}]}` (the daemon's start-up notices) |
| `rkat/missed` | `{}` | `{"notices": [{"warn", "text"}]}` (what the caller's agent was denied in unattended turns, each answered once; a client asks on every attach) |
| `rkat/tasks` | `{"session_id"}` | `{"tasks": [Task]}` (the engine's task store) |
| `rkat/checklist_view` | `{"view": "hidden"\|"shown"}` | `{}` (saved as `[ui] checklist`) |
| `rkat/decision_ended` | notification, daemon to client | a decision the client was shown ended |
| `rkat/file_diff` | notification, daemon to client | `{"tool_use_id", "path", "unified", "added", "removed"}`: a change the orchestrator's file tools made |

The mode is the daemon's (one per daemon; it governs the orchestrator and
its helpers), is
recorded in the realm rules file and tells the orchestrator when plan mode
starts or ends. `/rules` writes the realm rules file.
`rkat/new` and `rkat/resume` are refused while the orchestrator's run is
open. An application method namespace in the engine, or the governed
realm's review policy, could replace these methods.

## How each concern works

**Consent.** The daemon runs rkat-tui's approval engine (tiers,
layered rules, the classifier on the same endpoint and model, permission
modes, escalation timeout). A call that needs the human becomes one durable
decision (`daemon::decisions`: kind `rkat-tui.tool-call`, the dialog as its
JSON rendering, the gate as requester, the daemon's principal as approver,
the escalation timeout as deadline). The daemon subscribes to the decision
owner's openings (`subscribe_openings`) at start, before any client lists;
each opening is offered as a `decision/notice` to every attached client
authorized for it, a late client gets the open decisions on connect, a
`Missed` reading re-offers them, and each client is offered a decision once.
A client's `decision/decide` is intercepted by the proxy and parsed closed
(a named `actor` or `approver` is refused); the dialog's answer travels as
the one extension, `answer`, which the proxy strips, and the human's comment
as the decision's reason. The daemon dispatches it as an
`AuthenticatedDecisionEvent` (`MethodRouter::dispatch_with_decision_event`);
it is eligible only for a decision presented on that open connection, and
the first valid one wins. Two authority conditions hold: the approver is a
fixed principal bound to the token by host configuration, never an actor
named in the client's JSON; and notices go only to clients authorized for
the decision, which for one principal is every attached client. Timeouts and
interrupts withdraw the decision; `rkat/decision_ended` closes the other
dialogs. The decisions are the daemon's only record: nothing goes to the
realm's approval store, and decisions a stopped daemon left open are
withdrawn at start. The client never polls
`approval/list`: a daemon whose `initialize` lacks `decision/notice` and
`decision/decide` is refused at attach with "too old; restart it"
(`ClientError::DaemonTooOld`).

**The human's words.** The daemon records every `mob/member_send` and
`mob/turn_start` for the orchestrator as user input for the classifier. An
interrupt (`mob/stop_member_run`, `mob/force_cancel`) stops review, answers
open escalations with Stop and retires the running helpers; a run start
lifts the stop.

**Stream and catch-up.** `mob/turn_start` is request-scoped (its run is
cancelled when the sending connection closes), so prompts and steers go
through `mob/member_send`; a `/model` switch is the daemon's
`rkat/set_model`, the engine's LLM reconfigure on the member's live session
so no run is tied to a connection.
Every member `mob/stream_event` carries its envelope's `seq` and the `epoch`
of that sequence space. The client keeps a mark (session, epoch, sequence)
in `<workspace>/.rkat/tui/attach.cursor` and reattaches with
`mob/stream_open` from that `{epoch, seq}`; an event is shown only past the
mark. A first attach, a cursor of another daemon (the cursor records a
digest of the daemon's token) or without an epoch, or a truncated stream
renders the conversation from its start: the committed messages from
`rkat/transcript`, the session's last sequence and gauge from
`events/list_since`, then the live stream. A reconnect retries with backoff
and, in auto mode, restarts a dead daemon (at most every 30 s).

**Helpers and `fork_off`.** The daemon fills the builder's mob-tools slot
before it spawns the orchestrator, so the orchestrator has the engine's
agent mob tools (`delegate`, `mob_*`, `council`) beside the operator family
(`spawn_member`, …). `fork_off` is allowed to the daemon's members (not at
the depth limit): the child is seated under the source's admission (#2112),
asks the customizer, and is a gated, followed helper of the orchestrator's
session. The client lists the mobs (`mob/list`) and opens a mob-wide
`mob/stream_open {mob_id}` on each (`tui` and every `implicit-<session>`),
mapping events to the UI's `Msg::Scoped`; a helper first seen is
backfilled from `events/list_since`, its model from `events/snapshot`. The
client never calls `mob/fork_helper` or `mob/spawn_helper` itself.
(Before P4 the in-process runner had its own `fork` tool, because its main
session was not a mob member and the engine refuses `fork_off` to one; it
went with that runner.)

**Conversations.** One member identity per agent (`tui/main-<id>`), many
sessions. `/new` retires the member (the engine archives its session) and
spawns it fresh; `/resume` retires it and spawns it with `launch_mode =
resume` on an earlier session of its own. The identity, gate, `rkat/*`
methods and every client's addressing stay; only `current_session_id`
moves, which every client on that agent polls (1 s) and follows. The
daemon keeps the list in `<workspace>/.rkat/tui/conversations.json`, each
entry naming its agent (the engine keeps no list of a member's earlier
sessions). A session belongs to the identity that ran it: the engine
refuses to seat another (`ResumeSessionBoundToAnotherMember`), so a
conversation whose agent quit is seated again under that agent's name.
(A member per conversation was rejected in P4: it would keep every old
conversation seated. P5's member per agent is the set of identities that
rejection describes, built for agents, not conversations.)

### The member model (P5)

`daemon::agents::Agents` keeps the live agents: per agent its clients, a
detached flag (set by `/detach`, by its last client leaving without a
retire, and for an agent the daemon restored), a spare flag (the agent the
daemon seated at start, which the first `new` takes) and when it was last
active (a bind, a prompt, a run start). `rkat/attach` binds each
connection to one agent (`serve::admit`, before the acknowledgement),
seating one for `new` (or for a conversation whose agent quit); every
member-scoped `rkat/*` method acts on the connection's agent, the engine
methods the client calls name it (and the proxy refuses an engine member
action on another agent or its helpers, or a compaction of another
agent's session, with `NOT_YOURS`), and a client follows only its agent's
stream, helpers, diffs, decisions and classifier notes. The spawn
customizer shapes every member whose name `is_agent` accepts as an owner
with its own gate and file tools, and refuses such a name from any spawn
the host did not make. Every agent is wired to every other one (seat,
switch, reseat, restore), its persona names it and the norms, and system
notes (`mob_append_system_context`) give it the roster and tell the others
of a join or a leave. `rkat/retire` (a `/quit` with other clients on other
agents) cancels a running turn, stops its helpers and retires the member;
`rkat/shutdown {"retire": true}` (the last client's stop, or "terminate")
retires every agent before the daemon stops; SIGTERM keeps them for the
next start.

**Policy on the daemon side.** The realm stays ungoverned. The classifier
and tiers run as the orchestrator's `ToolDispatchAdmission`
(`daemon::approvals::RootGate`); helpers keep the helper gate on the same
engine. Durable refusals go through a `ToolConsequencePolicyRegistry` with
one narrow-only provider (`rkat-tui` / `deny-rules`, `daemon::policy`) that
denies what a deny rule matches and file writes into sealed paths; the
orchestrator, its same-mob helpers and child-mob members are bound to it,
and the engine refuses to rebuild a member without the registry. Tool
provenance comes from each session's catalog, read while it is idle and
cached (a helper's after its first run).

When the engine serves mob methods on a governed connection with a
host-installed consent surface, the same classifier moves behind
`OperationReviewer` and the TUI answers `approval/consent_decide`.

**The `!` shell.** It runs in the client, under the confinement plan the
client builds from the same flags and `tui.toml` layers as the daemon's
shell; its output, like `/remember` and `/memory` edits,
reaches the orchestrator with `mob/append_system_context`.

**Compaction.** `/compact` (or `/compact now`) sends `mob/compact_member` (fallback
`session/compact`); a cold session is not woken for it (run a turn
first, see the manual's limits).

## Known limits

`docs/manual.md`, "The workspace daemon", "Limits for now", is the one
list of what the daemon and its client do not do yet.

## Open engine asks

1. Same-identity rewind (lukacf/meerkat#2126): `session/fork_at` is refused
   for a mob-owned session and member forks give the copy a new identity.
   The design (a `mob/rewind_member`) is merged as db8547a532; the
   implementation is not scheduled.
2. The governed connection (#2122): mob methods on a governed connection, a
   governed connection on a shared runtime, and consent scoped beyond one
   pipe. These make the engine's own review contract usable from a
   detachable client (P5).
3. A notification sink that survives `RpcServer::new` per connection;
   `serve_tcp_connection` now exists upstream and is not adopted yet.
4. `delegate` naming the helper's session in its result.
5. Optional: a flag letting a trusted connection's `mob/spawn` request
   `host_root`; the daemon spawns the orchestrator in Rust meanwhile.
6. `mob/member_send` answering the runtime input id it admitted (a keyed
   send answers a delivery id, `WireContinuationReceipt`, not the input
   id), so a client can match it to `boundary_append_applied`'s
   `input_id` and tell its own steer's delivery from another client's: the
   attach runner marks its steers delivered first in, first out. An
   upstream fix is in progress (below).
7. Adopting a session created outside the mob: a way to seat a mob member
   on an existing non-member session, or to write the durable
   `comms_name` (and member binding) when seating one, so a consumer can
   bring sessions created outside the mob (rkat-tui's former in-process
   runner made them) into its mob. Without
   it those sessions are view only in the daemon
   (`docs/evidence.md`, "Sessions created before the daemon are view only").
8. Linux `deny_write` (S1 and S5 of the security review): a read-only
   `<workspace>/.rkat/` and `.git/` inside the writable workspace grant.
   The 0.9.11 Linux backend refuses both ways to say it:
   `LinuxPolicy::compile` refuses any non-empty `deny_read`/`deny_write`,
   and `push_disjoint` refuses a grant nested in another. The ask is
   `deny_write` lowered to read-only re-binds of existing subtrees inside
   a writable grant, installed after their parent (macOS already honours
   `deny_write`). Not opened; the user decides. Meanwhile the realm's
   own files are trusted by digest (`tui.toml`, `config.toml`,
   `tui_helpers.json`; `docs/evidence.md`, "After the security review"
   and "1(e)"); the engine's files in the realm (`sessions.sqlite3`, whose
   human-authored messages seed the classifier on resume, the approval
   and custody stores) are not, and need this bind.

## History

The phase records are the evidence sections in the status table, plus "The
pin moves to upstream `main`" (2026-11-08) and "The pin moves to the 0.9.11
integration" (2026-11-09); the engine is now the crates.io 0.9.11 release
("The engine moves to crates.io 0.9.11", 2026-11-09). Two asks of the first list are done: the stream
cursor (#2114) and pushed consent, delivered through durable decisions
rather than plain approval records (#2115).

Upstream PRs:

- #2100: sandbox pid namespace, read-only procfs, Unix socketpairs.
- #2102: `delegate` follows the member's current identity and model (closed; the fix shipped in 0.9.11 and `attach_helpers::a_helper_delegated_after_a_model_switch_runs_on_the_new_model` pins it).
- #2104: the thinking binding for uncatalogued model ids.
- #2105: host trust over hook source bytes (`HookSourceTrust`).
- #2112: fork children inherit the source's dispatch admission.
- #2113: compaction completion after recovery; `mob/compact_member`.
- #2114: `{epoch, seq}` stream cursor; request ids on refusals.
- #2115: `DurableDecisionService::subscribe_openings` for pushed decisions.
- #2116: session events routed to the turn's connection.
- #2117: `mob/fork_member` as an operator method (rewind deferred).
- #2124: the 0.9.11 release integration, `bffcd8f98a`, released on crates.io as 0.9.11.
- #2126: the same-identity rewind design (merged as db8547a532; implementation not scheduled).
- #2122: the map of the governed-realm gaps (open).
