# rkat-tui as a client of a running mob

## Status (2026-11-09)

Built and on `main`; the record of each phase, with its probes and test
names, is in `docs/evidence.md`:

| Phase | Date | `docs/evidence.md` section |
|---|---|---|
| P1 the `rkat-tui-mob` daemon | 2026-11-08 | "P1: the `rkat-tui-mob` daemon" |
| P2 `rkat-tui --attach` | 2026-11-08 | "P2: `rkat-tui --attach`" |
| P3 approvals on the daemon side | 2026-11-08 | "P3: approvals on the daemon side" |
| P4 helpers, conversations, detach | 2026-11-08 | "P4: helpers, conversations and detach in attach mode" |
| Review fixes for P1–P4 | 2026-11-08 | "Review fixes for P1–P4" |
| P4b attach-mode polish | 2026-11-08 | "P4b: attach-mode polish" |
| P3b and the 0.9.11 adoption | 2026-11-09 | "P3b and the 0.9.11 adoption" |

Attach mode is experimental; `--no-attach` is the default. Open:
same-identity `/rewind` (lukacf/meerkat#2126); P5, the governed realm
(#2122); MCP servers added or trusted after the
daemon's mob was created.

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
    orchestrator: mob `tui`, member `main`, host root, TurnDriven, durable;
      spawned by the daemon in Rust at start (or restored)
    agent mob tools (delegate, mob_*, council) + gated fork_off
  decisions: subscribe_openings -> decision/notice to attached clients;
    decision/decide -> AuthenticatedDecisionEvent as the daemon's principal
  rkat/* local methods (mode, rules, conversations, transcript, usage, trust)
  MethodRouter over loopback TCP after the rkat/attach handshake
    (RpcServer per connection, shared runtime, skip_shutdown_on_eof)
  realm lease + $XDG_STATE_HOME/rkat-tui/<key>/mob.endpoint

rkat-tui --attach (client, src/client/)
  reads the endpoint file, attaches with the token (starts the daemon if
    none answers); refuses a daemon without decision/notice+decide
  mob/member_send for prompts and steers (detach-safe); mob/turn_start
    only for a /model switch (model, provider, auth_binding)
  mob/stream_open with the saved {epoch, seq} cursor; rkat/transcript and
    events/list_since to render a conversation from its start
  mob-wide mob/stream_open per mob for the helpers' follower view
  mob/stop_member_run, mob/force_cancel for Esc
  decision/notice + decision/decide for the approval dialog
  mob/compact_member (fallback session/compact) for /compact now
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

Each connection must first send `rkat/attach {"token"}`; anything else is
answered with a JSON-RPC error and closed before the engine sees it
(`src/daemon/handshake.rs`).

**The token is the whole access control.** A connection that presents it can
do everything a TUI can: answer decisions, switch the permission mode, add
rules. Nothing on a loopback TCP connection proves which program sent it, so
the daemon does not try to tell a TUI from any other holder of the token.
The token must therefore stay out of the agent's reach: the agent's confined
shell is granted the workspace (`.rkat` included; the Linux backend has no
deny lists), the temp dir, the Cargo home and the user cache dir, and
loopback is reachable from it, so the file lives in the user's state
directory, which none of these hold. The daemon refuses to start when its
confinement plan grants the file's directory after all
(`DaemonError::EndpointExposed`: a workspace that contains the state
directory, an `XDG_STATE_HOME` in the temp dir, an extra
`--confinement-read`). With `--no-confinement` the shell is the trusted host
and can read the file like any other process of the user.

The token grants access, not identity: every decision is recorded as the
daemon's one principal (`[daemon] principal` in the user's `tui.toml`,
default the login name); the daemon is single-person by design.

### The `rkat/*` local methods

The engine's wire has nothing for an application's permission mode, rule
list, conversation list or trust record, so the daemon answers these
itself before a line reaches the router (`daemon::serve` proxy,
`daemon::local`, `daemon::decisions`); they are the only non-engine
methods on the wire:

| Method | Params | Result |
|---|---|---|
| `rkat/attach` | `{"token"}` | `{"attached": true}` (first line only) |
| `rkat/get_mode` | `{}` | `{"mode", "paused", "denials"}` |
| `rkat/set_mode` | `{"mode": "default"\|"accept-edits"\|"plan"\|"auto"}` | `{"status", "notice", "warning"?}` |
| `rkat/rules` | `{"op": "list"}`, `{"op": "add", "deny", "tool", "pattern"?}`, `{"op": "remove", "n"}` | `{"ok", "text"}` |
| `rkat/conversations` | `{}` | `{"conversations": [{session_id, last_used, messages, title, current}]}` |
| `rkat/new` | `{}` | `{"session_id", "warning"?}` |
| `rkat/resume` | `{"session_id"}` | `{"session_id", "warning"?}` |
| `rkat/transcript` | `{"session_id"}` | `{"messages": [Message]}` |
| `rkat/usage` | `{"session_id"}` | `{"input_tokens", "output_tokens"}` |
| `rkat/trust` | `{"op": "project"\|"rules"}` | `{"ok", "text"}` |
| `rkat/decision_ended` | notification, daemon to client | a decision the client was shown ended |

The mode is the daemon's (one per daemon; it governs the orchestrator and
its helpers, as the in-process mode governs a session and its helpers), is
recorded in the realm rules file and tells the orchestrator when plan mode
starts or ends. `/rules` writes the realm rules file as in-process.
`rkat/new` and `rkat/resume` are refused while the orchestrator's run is
open. An application method namespace in the engine, or the governed
realm's review policy, could replace these methods.

## How each concern works

**Consent.** The daemon's approval engine is the in-process one (tiers,
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
withdrawn at start. The in-process TUI keeps no record beyond the dialog
itself. The client never polls
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
through `mob/member_send`; only a `/model` switch uses `mob/turn_start`.
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
mapping events to the in-process `Msg::Scoped`; a helper first seen is
backfilled from `events/list_since`, its model from `events/snapshot`. The
client never calls `mob/fork_helper` or `mob/spawn_helper` itself.
In-process, `fork_off` stays unavailable because the main in-process session
is not a mob member; the TUI's own `fork` tool is the in-process fork and is
not offered in attach mode.

**Conversations.** One member identity (`tui/main`), many sessions. `/new`
retires the member (the engine archives its session) and spawns it fresh;
`/resume` retires it and spawns it with `launch_mode = resume` on an earlier
session. The identity, gate, `rkat/*` methods and every client's addressing
stay; only `current_session_id` moves, which every client polls (1 s) and
follows. The daemon keeps the list in
`<workspace>/.rkat/tui/conversations.json` (the engine keeps no list of a
member's earlier sessions). A member per conversation (`main-2`, …) was
rejected: it would make the root a set of identities through the customizer,
the gate, the watcher and the local methods, and keep every old conversation
seated.

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

**The `!` shell.** It runs in the client, confined as in-process (same flags
and `tui.toml` layers); its output, like `/remember` and `/memory` edits,
reaches the orchestrator with `mob/append_system_context`.

**Compaction.** `/compact now` works in both modes: attach mode sends
`mob/compact_member` (fallback `session/compact`); in-process it calls
`MeerkatMachine::request_compaction` without a rebuild. The in-process
`/model` no longer rebuilds the session for helpers either (#2102).

## Known limits

`docs/manual.md`, "Attach mode (experimental)", is the one list of what
attach mode does not do yet.

## Open engine asks

1. Same-identity rewind (lukacf/meerkat#2126): `session/fork_at` is refused
   for a mob-owned session and member forks give the copy a new identity;
   the design note recommends a `mob/rewind_member`.
2. The governed connection (#2122): mob methods on a governed connection, a
   governed connection on a shared runtime, and consent scoped beyond one
   pipe. These make the engine's own review contract usable from a
   detachable client (P5).
3. A notification sink that survives `RpcServer::new` per connection;
   `serve_tcp_connection` now exists upstream and is not adopted yet.
4. `delegate` naming the helper's session in its result.
5. Optional: a flag letting a trusted connection's `mob/spawn` request
   `host_root`; the daemon spawns the orchestrator in Rust meanwhile.

## History

The phase records are the evidence sections in the status table, plus "The
pin moves to upstream `main`" (2026-11-08) and "The pin moves to the 0.9.11
integration" (2026-11-09); the engine is now the crates.io 0.9.11 release
("The engine moves to crates.io 0.9.11", 2026-11-09). Two asks of the first list are done: the stream
cursor (#2114) and pushed consent, delivered through durable decisions
rather than plain approval records (#2115).

Upstream PRs:

- #2100: sandbox pid namespace, read-only procfs, Unix socketpairs.
- #2102: `delegate` follows the member's current identity and model.
- #2104: the thinking binding for uncatalogued model ids.
- #2105: host trust over hook source bytes (`HookSourceTrust`).
- #2112: fork children inherit the source's dispatch admission.
- #2113: compaction completion after recovery; `mob/compact_member`.
- #2114: `{epoch, seq}` stream cursor; request ids on refusals.
- #2115: `DurableDecisionService::subscribe_openings` for pushed decisions.
- #2116: session events routed to the turn's connection.
- #2117: `mob/fork_member` as an operator method (rewind deferred).
- #2124: the 0.9.11 release integration, `bffcd8f98a`, released on crates.io as 0.9.11.
- #2126: the same-identity rewind design note (open).
- #2122: the map of the governed-realm gaps (open).
