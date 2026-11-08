# rkat-tui as a client of a running mob

## Status (2026-11-08, P1–P4, P4b)

P1 is in: `rkat-tui-mob` (experimental, `src/bin/rkat-tui-mob.rs`,
`src/daemon/`) bootstraps the workspace realm exactly as the TUI does,
composes the engine's JSON-RPC `SessionRuntime` with our own
`MobMcpState`, seats the orchestrator (mob `tui`, member `main`,
turn-driven, durable, host root) or restores it, and serves the engine's
whole `MethodRouter` (`initialize`, `mob/*`, `turn/*`, `events/*`,
`approval/*`, …) over loopback TCP to any number of clients; a client
leaving does not stop the runtime or a running turn.

Transport: loopback TCP (`--listen 127.0.0.1:<port>`, default port 0),
not a Unix socket: it is the loop `rkat-rpc --tcp` already runs, it
forwards over SSH, and it behaves the same on every platform. Any local
process can reach a loopback port, so each connection must first send
`rkat/attach` with the per-daemon token from the endpoint file
`$XDG_STATE_HOME/rkat-tui/<key>/mob.endpoint` (else under
`~/.local/state`; `<key>` is the first 32 hex digits of the SHA-256 of the
canonical workspace path; directory 0700, file 0600: `{"address", "token",
"pid"}`, one JSON line, removed on shutdown; `--endpoint-file` overrides
it, and the TUI passes the path it waits on to a daemon it starts);
anything else is answered with a JSON-RPC error and closed before the
engine sees it (`src/daemon/handshake.rs`, the only non-engine thing on
the wire).

**The token is the whole access control.** A connection that presents it
can do everything a TUI can: answer `approval/decide`, switch the
permission mode with `rkat/set_mode`, add rules with `rkat/rules`. Nothing
on a loopback TCP connection proves which program sent it, so the daemon
does not try to tell a TUI from any other holder of the token. The token
therefore must stay out of the agent's reach: the agent's confined shell is
granted the workspace (`.rkat` included; the Linux backend has no deny
lists), the temp dir, the Cargo home and the user cache dir, and loopback
is reachable from it, so the file lives in the user's state directory, which
none of these hold, and the daemon refuses to start when its confinement
plan grants the file's directory after all (`DaemonError::EndpointExposed`:
a workspace that contains the state directory, an `XDG_STATE_HOME` in the
temp dir, an extra `--confinement-read`). With `--no-confinement` the shell
is the trusted host and can read the file like any other process of the
user; nothing is kept from it then. (Until the review fixes the file was
`<workspace>/.rkat/tui/mob.endpoint`, which the confined shell could read:
`docs/evidence.md`, "Review fixes for P1–P4". The daemon and the client
still look there: a daemon of that version that answers keeps the
workspace until `/daemon stop`; a stale file is deleted.)

Known limits of P1:

- (Lifted in P3, below: the orchestrator and its helpers are gated, and
  escalations are answered over `approval/*`.)
- `mob/turn_start` is request-scoped: its run is cancelled when the
  connection that sent it closes. A client that may detach sends
  `mob/member_send` (answered at once; the run is the member's). P2's
  client uses it for prompts.
- `mob/stream_open` has no cursor; a reattaching client catches up with
  `events/list_since` on the member's session (works, see
  `docs/evidence.md`).
- No start-on-demand, no daemonising (`--foreground` only), and running
  the daemon and the in-process TUI on the same workspace at once is not
  supported (they share the realm's mob store).

P2 is in (experimental): `rkat-tui --attach` runs the TUI against the
daemon (`src/client/`). What works: prompts and streaming (`mob/member_send`
+ `mob/stream_open`), steers, Esc (`mob/stop_member_run` for the run the
stream reported, `mob/force_cancel` on a second Esc after 1 s or when no run
was reported), `/model <provider>:<id>` (the next prompt goes through
`mob/turn_start` with `model`/`provider`; the session keeps the model, also
across a daemon restart), the context gauge and token counts from
`turn_completed`, detach (`/quit`), reattach with catch-up, reconnect with
backoff after the daemon restarts, start-on-demand (`--attach=auto`), a
status-bar segment and `/daemon stop`. The attach runner serves the UI's
`Effect`/`Msg` contract itself rather than sitting under the in-process
runner: that runner is built on the in-process `Runtime` (approval desk,
helper host, `fork`, model lists, the user shell's confinement plan).

Catch-up rule: the live stream's envelope `seq` and the replay's
`cursor.sequence` are the same durable per-session sequence (the
notification's own `sequence` is a per-stream counter and is not used). The
client opens the live stream, then pages `events/list_since` from its saved
cursor, and shows an event only when its sequence is above the high-water
mark, which then moves to it; the mark is saved in
`<workspace>/.rkat/tui/attach.cursor`. A run found in progress is shown as
running (`Msg::RemoteTurn`).

Deferred after P2: `/new`, `/resume`, `/rewind`, helpers' follower
view, `fork` (P4); `/compact`, `/remember`, `/memory` and the model picker
(each answers "not available in attach mode yet"); `/model` on a custom
endpoint (needs the auth binding); a reconnect never restarts a dead
daemon; `attach = true` in `tui.toml` (P4).

P3 is in: approvals on the daemon side.

- The daemon's approval engine is the in-process one, built by the shared
  bootstrap (tiers, layered rules, the classifier on the same endpoint and
  model, permission modes, escalation timeout, the ledger). The orchestrator
  gets it as its `ToolDispatchAdmission` (`daemon::approvals::RootGate`, the
  in-process `ApprovalGate` resolved to the orchestrator's session through
  the helper registry); its helpers keep the helper gate on the same engine.
- A call that needs the human is the escalation record the ledger already
  writes in the realm's approval store (`runtime.approval_service()` is the
  persistence bundle's one issuer, the same the ledger uses), with the
  dialog's content as its `request_body`. The daemon's `Desk` waits on the
  record (polls `get` every 100 ms) and hands the engine the decision a
  client recorded with `approval/decide`; the human's comment, "allow + add
  rule" and "deny and stop" travel in the decision's `provenance`
  (`{"rkat_tui": {"answer", "comment"}}`); a plain approve or deny from any
  other client is allow once or deny with its reason. The engine's own
  timeout, serialisation and re-checks are unchanged; when it settles the
  record a client already decided, the ledger confirms instead of failing.
- The client lists `approval/list` (status pending, requester the TUI's
  gate, owner the orchestrator's session) on attach, on every
  `tool_call_requested`, and every 250 ms while a turn runs or a dialog is
  open, shows the in-process dialog, and decides. Any attached client may
  answer; the first decision wins, the others' dialogs close on their next
  list with a notice, and a late decision is refused by the engine and
  dropped.
- The human's words reach the classifier: the daemon reads every
  `mob/member_send` and `mob/turn_start` for the orchestrator on its way to
  the router and records the content as user input (the in-process session
  records it at admission). An interrupt (`mob/stop_member_run`,
  `mob/force_cancel`) stops review for the session and answers its open
  escalations with Stop; a run start (followed with
  `subscribe_agent_events`) lifts it, as in-process.
- Tool provenance: the RPC runtime exposes no published tool definitions,
  only the command read, which waits behind a running turn, so the daemon
  reads each session's catalog while it is idle (at start and after every
  run) and caches it; until the first read only the TUI's own file and note
  tools are known (everything else is judged a foreign tool). A helper's
  catalog is read after its first run (P4b); until then it is judged by the
  TUI's tools and the natives of the catalogs read so far.
- Durable refusals: the daemon installs a `ToolConsequencePolicyRegistry`
  with one provider (`rkat-tui` / `deny-rules`, `daemon::policy`) and binds
  the orchestrator, its same-mob helpers (customizer) and child-mob members
  (`with_child_application_tool_policy`) to it. The policy is narrow-only:
  it denies a call a deny rule matches or a file write into a sealed path
  and allows the rest. Observed: a daemon started without the registry
  starts, and the member's first delivery fails its restore with "a
  governed application tool policy requires the host policy registry".
- `[thinking]`: the profile carries the start-up model's provider params;
  a rebuild re-applies the setting of the session's model
  (`resume_provider_params`); a `mob/turn_start` model switch gets the new
  model's params (or a clear) added by the daemon unless it names its own.
- The user's `!` shell runs in the client, built as in-process (the
  confinement plan from the same flags and `tui.toml` layers); its output
  goes to the orchestrator with `mob/append_system_context`.

#### The `rkat/*` local methods

The engine's wire has no field or method for an application's permission
mode or rule list, so the daemon answers three methods itself, before a
line reaches the router (`daemon::serve::proxy`, `daemon::local`); with
`rkat/attach` they are the only non-engine methods:

| Method | Params | Result |
|---|---|---|
| `rkat/get_mode` | `{}` | `{"mode", "paused", "denials": [consecutive, max, total, max]}` |
| `rkat/set_mode` | `{"mode": "default"\|"accept-edits"\|"plan"\|"auto"}` | `{"status", "notice", "warning"?}` |
| `rkat/rules` | `{"op": "list"}`, `{"op": "add", "deny", "tool", "pattern"?}`, `{"op": "remove", "n"}` | `{"ok", "text"}` |

The mode is the daemon's (one per daemon; it governs the orchestrator and
its helpers, as the in-process mode governs a session and its helpers), is
recorded in the realm rules file and tells the orchestrator when plan mode
starts or ends. `/rules` runs on the daemon's engine, which writes the realm
rules file as the in-process `/rules` does (the brief's "client writes the
file, then `rkat/reload_rules`" was not built: two writers of one file and a
reload that re-runs the trust checks, for no gain). A future engine
extension (an application method namespace, or the governed realm's review
policy) could replace them.

Deferred after P3 (done in P4 or P4b unless named below): helpers'
follower view, `fork`, `/new`, `/resume`, `/rewind`, `/compact`; a push
notification for pending approvals (engine ask 2); approval decision events
(the client reads the counters with `rkat/get_mode`, not the per-call
notices).

P4 is in: helpers, conversations and detach in attach mode.

- Helpers: the orchestrator's build mounts the engine's agent mob tools
  (`delegate`, `mob_*`, `council`) beside the operator family
  (`spawn_member`, …): the daemon fills the builder's mob-tools slot
  (`set_mob_tools`) before it spawns the orchestrator (`MethodRouter::new`
  filled it only on the first connection, after the spawn, which is why P3
  saw no `delegate`). `fork_off` stays denied. The TUI's own `fork` tool is
  not offered (it is built on the in-process service); the engine's
  `fork_helper` is the host method `mob/fork_helper`, not an agent tool, so
  there is nothing to alias. The daemon runs the in-process helper follower
  (`mobs::follow`) for its bookkeeping (running set, helper sessions for
  nesting), and an interrupt (`mob/stop_member_run`, `mob/force_cancel` of
  the orchestrator) also force-cancels and retires the orchestrator's
  running helpers, as in-process.
- Follower view: the client lists the mobs (`mob/list`) on attach and while
  a tool that seats helpers runs, opens a mob-wide `mob/stream_open
  {mob_id}` on each (`tui` and every `implicit-<session>`), maps each
  `AttributedEvent` to the in-process `Msg::Scoped` and the running set to
  `StatusUpdate::Helpers` (`client::helpers`). A helper first seen has its
  session's durable events read (`events/list_since`) and its live events
  held until then (the router subscribes a late member from its live
  events on); its model comes from `events/snapshot`. Cost is the UI's own
  sum over `turn_completed`, kept per conversation in the cost sidecar.
- Conversations: one member identity (`tui/main`), many sessions. `/new`
  retires the member (the engine archives its session) and spawns it again
  fresh (host root, turn-driven); `/resume` retires it and spawns it with
  `launch_mode = resume` on an earlier session, which the engine revives.
  The identity, gate, `rkat/*` methods and every client's addressing stay;
  only `current_session_id` moves, which every client polls (1 s) and
  follows (stream reopened, transcript from `rkat/transcript`, mark at the
  session's last sequence). Chosen over a member per conversation
  (`main-2`, …): that would make the root a set of identities through the
  customizer, the gate, the watcher and the local methods, keep every old
  conversation seated, and gain nothing `/resume` needs. The daemon keeps
  the list in `<workspace>/.rkat/tui/conversations.json` (the engine keeps
  no list of a member's earlier sessions).
- `/rewind` is not offered: `session/fork_at` is refused for a mob-owned
  session ("mob-owned session transcripts cannot be edited through the
  generic session surface"), and a member fork (`MemberLaunchMode::Fork`,
  `mob/fork_helper`) takes `ForkContext::FullHistory` or `LastMessages
  {count}` rendered as text into a fresh session: no boundary. Ask 7 below.
- Model picker and `/model`: the client's endpoint registry (the `tui.toml`
  layers, keyless built-ins added) and lister; a switch carries
  `auth_binding` (`set` for a configured endpoint, `clear` for a built-in)
  on `mob/turn_start`; the daemon resolves `[thinking]` by the binding's
  endpoint name.
- `/compact` reports the policy (the client reads the `tui.toml` layers);
  `/compact now` is refused with the reason (no compact-now for a member).
  `/remember` and `/memory` edit the files locally and send
  `mob/append_system_context`.
- Detach: `attach = true` in a user `tui.toml` selects attach mode
  (`bootstrap::attach_mode`); a reconnect starts a daemon that died (stale
  endpoint file; auto mode; at most every 30 s); the daemon is started
  through `setsid(1)` when it is on `PATH` (in place: a child of the TUI is
  no group leader), else in its own process group.
- New `rkat/*` methods: `rkat/conversations {}` → `{"conversations":
  [{session_id, last_used, messages, title, current}]}`, `rkat/new {}` and
  `rkat/resume {session_id}` → `{"session_id", "warning"?}`,
  `rkat/transcript {session_id}` → `{"messages": [Message]}`.

P4b is in: the polish P4 left.

- Token counter and gauge: the daemon answers `rkat/usage {session_id}` →
  `{"input_tokens", "output_tokens"}` from the session service's `read`
  (`SessionView::billing`; the wire's `events/snapshot` and `session/read`
  carry no usage). The client seeds the counter from it on attach, on
  reconnect and after `/new`/`/resume`, and the gauge after a switch from
  the last `turn_completed` in the replay it reads anyway.
- `/trust` and `/rules trust`: `rkat/trust {"op": "project"|"rules"}`
  on the daemon's trust record (`ProjectInputs::trust_all`,
  `ApprovalEngine::trust_realm_rules`), answered `{"ok", "text"}`.
  Trusted hooks run from the agent's next build; its MCP servers stay the
  ones in its mob's definition (made when the mob was created).
- Helper catalogs: the helper registry's updates (dropped before) feed
  `daemon::approvals::watch_helpers`, which reads a helper's tool
  definitions after its first run and caches them per session. A
  `spawn_member` helper is judged by its own catalog from its second run;
  a `delegate` helper retires after its one run, so the fallback stays.

What remains: P5 (governed realm, asks 3); `/rewind`, `/compact now` and
the TUI's `fork` in attach mode; MCP servers added or trusted after the
daemon's mob was created. Engine asks
outstanding (no PRs yet): 1 stream cursor, 2 approval notice, 5 a
notification sink that survives `RpcServer::new` per connection (map risk
(c)), 6 fork-seat admission (fork children ungated; our reproducer PR is
pending), 7 a member fork at a message boundary (or `session/fork_at` for a
member's session), 8 compact-now for a member, 9 `delegate` naming the
helper's session in its result (the client finds helpers by listing mobs).

Decision (2026-11-08): rkat-tui becomes two binaries. A daemon embeds the
Meerkat engine, holds the realm, runs one durable mob member as the
orchestrator and exposes Meerkat's own JSON-RPC methods over loopback TCP
(first decided as a Unix domain socket; changed the same day, see Status).
The TUI becomes a client of that endpoint: it attaches, sends turns,
renders the member's event stream, answers approvals, and can detach while
the mob keeps running. The protocol is Meerkat's existing method router and
wire types, not a protocol of our own, so other clients can attach and the
consent and decision methods come from the engine.

The facts below were read from upstream `main` on 2026-11-08 (file:line
references in the session's `rpc-map.md`); the engine references are to
`crates/meerkat-rpc`, `crates/meerkat-mob` and `crates/meerkat-mob-mcp`.

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

## Architecture

```
rkat-tui-mob (daemon, one per workspace realm)
  engine: AgentFactory + SessionRuntime + our MobMcpState
    SpawnMemberCustomizer: approval gate (ToolDispatchAdmission), file and
      shell tools (external_tools), instructions, persona, auth binding,
      application policy (consequence registry) for every member incl. the
      orchestrator; re-supplied on resume
    orchestrator: SpawnMemberSpec::host_root("main", identity), TurnDriven,
      spawned by the daemon in Rust at start (or restored)
  MethodRouter served per connection over loopback TCP after an
    `rkat/attach` token handshake (newline-delimited JSON; RpcServer per
    connection, shared runtime, skip_shutdown_on_eof; the loop copies
    serve_tcp_with_options_until)
  realm lease + $XDG_STATE_HOME/rkat-tui/<key>/mob.endpoint (address,
    token, pid; outside every path the agent's shell is granted);
    start-on-demand by the TUI

rkat-tui (client)
  reads the endpoint file, connects and attaches with the token (starts
    the daemon if none answers)
  mob/member_send for prompts (detach-safe; mob/turn_start's run dies with
    its connection), mob/turn_start only for model/provider/auth_binding
    overrides (/model)
  mob/stream_open for live events + events/list_since for catch-up
  mob/stop_member_run, mob/force_cancel for Esc
  approval/list + approval/decide for the approval dialog (pending requests
    are discovered on ToolCallRequested until the engine pushes them)
  mob/fork_helper, mob/spawn_helper for the agent's helpers (via the member)
  everything local stays local: rendering, editor, the user's `!` shell,
    history, key reference
```

### What the engine provides and we reuse

`MethodRouter::new(runtime, config_store, sink)` and `dispatch`;
`RpcServer::new(reader, writer, runtime, config_store).run()` over any
`AsyncBufRead`/`AsyncWrite`; `SessionRuntime::new_with_config_store`,
`set_mob_state`; our own `MobMcpState` with
`with_child_spawn_member_customizer`,
`with_tool_consequence_policy_registry`,
`with_child_application_tool_policy`; `SpawnMemberSpec::host_root`;
`mob/turn_start` with identity overrides (an in-place `/model`);
`mob/stream_open`; `events/list_since`; `approval/{request,list,get,decide}`;
`tool_grants/*`; `decision/*`; `mob/fork_helper`, `mob/spawn_helper`,
`mob/lifecycle {resume}`; `MobHandle::subscribe_agent_events_from` (Rust,
cursor replay).

### Policy on the daemon side

The realm stays ungoverned for now. The engine's tiered review and human
consent route run only in a governed realm, and over RPC a governed realm
serves no mob methods and binds consent to one open pipe, so it cannot host
a detachable client yet (asks 1 to 3 below). Until that changes:

- The classifier and tier policy run in the daemon as the member's
  `ToolDispatchAdmission`, installed by the customizer (the pattern
  Meerkat Connect uses for terminal consent). A call that needs a human
  raises an `ApprovalService` request; the TUI lists and decides it over
  `approval/*`. This gate is process-local and not recorded with the
  session, which the engine's own map names as a known gap (planned:
  approval-backed scoped grants).
- Durable refusals go through the application policy binding
  (`ToolConsequencePolicyRegistry`, narrow-only), which the engine records
  with the member and refuses to rebuild without.
- Gated tools (`grant_gated_tools`) and `tool_grants/*` carry the
  "allow for this session" decisions the engine already persists.

When the engine serves mob methods on a governed connection with a
host-installed consent surface, the same classifier moves behind
`OperationReviewer` and the TUI answers `approval/consent_decide`.

### Engine asks (small, in order of need)

1. `mob/stream_open` takes an optional cursor (`{epoch, seq}` or `earliest`)
   and emits the envelope's epoch and seq, so a reattaching client resumes
   without gaps; the Rust `subscribe_agent_events_from` already does it.
2. A notification for a pending plain approval (`approval/notice`) on
   ungoverned connections, so the client need not poll `approval/list`.
3. Longer term: mob methods on a governed connection, a governed connection
   attachable to a shared runtime, and a host-installable consent surface
   whose scope outlives one pipe. These make the engine's own review
   contract usable from a detachable client.
4. Optional: a flag letting a trusted connection's `mob/spawn` request
   `host_root`; the daemon spawns the orchestrator in Rust meanwhile.

## Upstream dispositions (2026-11-08)

The maintainers answered the asks on the test-only PRs (zond/rkat-tui#1,
lukacf/meerkat#2113, #2114, #2115, #2117). What they decided, and what it
changes here:

- **Compaction (#2113).** The lost `CompactionCompleted` after a recovery is a
  defect, fixed for 0.9.11 by forwarding queued events before post-commit
  reconciliation returns. The per-build threshold stays caller-scoped: the
  daemon keeps re-supplying it on rebuild and recovery, as it does. A typed
  compact-now arrives as `session/compact` plus a mob-member variant, so
  `/compact now` in attach mode waits for that release.
- **Stream resume (#2114).** `mob/stream_open` gets an `{epoch, seq}` cursor
  and each event carries its epoch; a stale epoch replays from the earliest
  event. The client's replay-plus-live catch-up can then become one stream.
  The request-id loss on parameter refusals is fixed with it.
- **Approval push (#2115).** Plain approval records stay pull-only. Push goes
  through durable decisions: `decision/notice` when a decision opens. The
  daemon's consent should therefore move from `approval/*` to `decision/*`,
  which also gives the durable record the ownership line asks for. Open
  Answer from the maintainers (same day): the daemon, as the trusted host
  that authenticates each TCP connection, wraps a client's `decision/decide`
  as an `AuthenticatedDecisionEvent` and calls
  `MethodRouter::dispatch_with_decision_event` itself; push comes from an
  in-process subscription to the decision service's open events, which the
  daemon forwards to attached clients. No per-connection TCP operator; plain
  `rkat-rpc --tcp` gets no decision notices by design. Subscription names
  and the implementation are pending. Two authority conditions the daemon
  must keep: the shared token grants access, not identity, so the approver
  on an `AuthenticatedDecisionEvent` is a fixed principal bound to the
  token by host configuration (the daemon is single-person by design), never
  an actor named in the client's JSON; and open-decision notices go only to
  clients authorized for that decision, which for one principal is every
  attached client, and for a future multi-person daemon means one
  authenticated principal per connection and per-decision filtering.
- **Forks and rewind (#2117).** `mob/fork_member` becomes an operator method
  that gives the child a new identity, after the admission fix of #2112.
  Same-identity rewind is deferred to a design note. Resuming onto another
  member's session gets a typed refusal. For `/rewind` in attach mode this
  means the rewound conversation would live under a new member identity, so
  the conversations model (one identity, many sessions) needs to follow a
  chosen member rather than always `main`.
- **Fix order for the behaviour reports:** #2112 (fork children inherit the
  admission), #2116 (session events to the turn's connection), #2102
  (delegate follows the current identity), plus the `HookSource` constructor.

None of this is merged or released yet; the plan tracks 0.9.11.

The three gaps behind the governed-realm direction (P5) are mapped in
lukacf/meerkat#2122: a governed connection serves no mob methods, a
governed connection owns its runtime, and consent is scoped to the open
pipe.

## Phases

Each phase keeps the current in-process TUI working and ships with tests.

- **P1 daemon skeleton (done).** `src/bin/rkat-tui-mob.rs` plus `src/daemon/`:
  reuse `bootstrap` (realm, config, factory, confinement, project trust,
  instructions, MCP) to build the factory; `SessionRuntime` + our
  `MobMcpState`; the customizer generalised from `mobs::HelperCustomizer`
  to treat the host-root orchestrator as the owner; spawn or restore the
  orchestrator; serve the router over loopback TCP with a shared runtime;
  lease, endpoint file, clean shutdown on SIGTERM, `--foreground`. Tests: start
  the daemon in-process in a temp workspace, talk JSON-RPC over TCP
  (`initialize`, `mob/members`, `mob/turn_start` with a scripted LLM,
  `mob/stream_open`), two connections at once, reconnect after a client
  drops.
- **P2 client mode (done, experimental).** `rkat-tui --attach` (later the default): a
  `session` implementation over the daemon connection (turn, interrupt, events,
  model switch, resume via `mob/lifecycle`), the runner unchanged above
  it; catch-up with `events/list_since`; start-on-demand. Tests: the
  scripted-LLM runner tests run against the daemon.
- **P3 approvals over the daemon (done).** Admission on the daemon side raising
  approvals; the TUI's dialog over `approval/*`; timeouts and escalation
  carried over; consequence registry for durable refusals; `tool_grants`
  for session allows. Tests: the approval routing tests against the
  daemon.
- **P4 helpers, forks, detach.** `delegate`/`fork_helper` from the
  orchestrator, followers in the TUI from the mob-wide stream, cost from
  `TurnCompleted`, compaction events, detach and reattach with replay,
  a second client.
- **P5 governed realm**, once asks 3 land.
