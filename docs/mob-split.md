# rkat-tui as a client of a running mob

## Status (2026-11-08)

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
`<workspace>/.rkat/tui/mob.endpoint` (mode 0600: `{"address", "token",
"pid"}`, one JSON line, removed on shutdown); anything else is answered
with a JSON-RPC error and closed before the engine sees it
(`src/daemon/handshake.rs`, the only non-engine thing on the wire).

Known limits of P1:

- No approval gate on the orchestrator's own calls yet (the customizer's
  `RootMember::admission` is the P3 hook); its helpers keep the TUI's
  helper gate, but nothing in the daemon answers an escalation or records
  user intent, so their reviewed calls are refused.
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
  realm lease + <workspace>/.rkat/tui/mob.endpoint (address, token, pid);
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
- **P2 client mode.** `rkat-tui --attach` (later the default): a
  `session` implementation over the daemon connection (turn, interrupt, events,
  model switch, resume via `mob/lifecycle`), the runner unchanged above
  it; catch-up with `events/list_since`; start-on-demand. Tests: the
  scripted-LLM runner tests run against the daemon.
- **P3 approvals over the daemon.** Admission on the daemon side raising
  approvals; the TUI's dialog over `approval/*`; timeouts and escalation
  carried over; consequence registry for durable refusals; `tool_grants`
  for session allows. Tests: the approval routing tests against the
  daemon.
- **P4 helpers, forks, detach.** `delegate`/`fork_helper` from the
  orchestrator, followers in the TUI from the mob-wide stream, cost from
  `TurnCompleted`, compaction events, detach and reattach with replay,
  a second client.
- **P5 governed realm**, once asks 3 land.
