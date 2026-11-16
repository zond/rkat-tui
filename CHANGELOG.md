# Changelog

## [Unreleased]

### Added

- Checkpoints of long runs: when one run of an agent or a helper reaches
  `agent.checkpoint_steps` model calls (100 by default), the agent is
  asked for a short status, the classifier's model judges from your
  messages and the actions taken whether going on is sensible, and the
  run continues on its own (`checkpoint after 100 steps: continuing
  (<reason>)`) or stops with the reason and what to do. At most
  `agent.checkpoint_continues` (5) checkpoints in a row continue
  automatically; your next message starts the count again. A failed or
  unclear review, or no classifier model, stops the run. `/auto stats`
  counts them.
- `assign`: the agent starts a single-use helper on a self-contained
  task and goes on at once, so it still hears you and answers while the
  helper works. The helper starts from a fresh context, is wired to its
  parent both ways (each can message the other at any time), and when it
  finishes its parent is told with its answer and it is retired. The call
  shows as `assign <name>: <task>`.
- A call approved once from `/approve` stays on the list, marked
  "approved once · waiting for the agent to run it again", until the
  agent runs it (then it goes); it no longer vanishes as if it had never
  been blocked. `w` on it withdraws the approval: it is a blocked call
  again, to approve another way or leave.

### Changed

- A run no longer fails with "Terminal failure: Failed (TurnLimitReached)"
  after 100 model calls: the engine's limit is `agent.checkpoint_steps`,
  and reaching it is a checkpoint (above).
- A `fork_off` child can talk with its parent: it is wired to the agent
  (or helper) that forked it, so each can message the other while it runs
  and after. Its parent is still told of its first run's end by the
  engine's outcome, and now also each time a later run (one a message
  started) leaves it idle. The agents' instructions say so, and name the
  three ways to start a helper (`assign`, `fork_off`, `spawn_member`) in
  three lines.
- A helper kept with `spawn_member` or `mob_spawn_member` tells its parent
  each time it goes idle, with its last message (`Your helper w1 went
  idle. …`); it stays seated. No note comes for its start or when it
  becomes active, nor when its last act was a message to its parent.
- Approving from `/approve` (once, for this session, always, or a kind of
  effect) tells the agent at once and lets it go on: a running turn reads
  the note at its next step and repeats the call instead of ending with a
  request to approve; an idle agent gets a turn of its own for it, shown
  under "✓ approved: the agent is told and goes on" and counted as yours.
  This also holds right after an interrupt. A helper's call is told to its
  agent, named as that helper's.

### Removed

- **Behaviour change:** `delegate`, the engine's helper tool, is no longer
  offered to any agent or helper, and a call of it is refused. It blocked
  the agent until its helper was done, so meanwhile the agent heard no one
  and could not answer. Use `assign` for a self-contained task (the helper
  runs alongside the agent and its answer arrives in a note), `fork_off`
  when the task needs the conversation so far, or `spawn_member` for a
  helper to keep and talk to. With it go its deadline default (three
  times the longest tool deadline), the retirement of a helper whose
  `delegate` call timed out, and the "timed out" ending. A conversation an
  earlier rkat-tui stored still shows its `delegate` calls and helpers,
  and a `delegate` helper such a daemon left running still finishes and
  is told to its agent at the next start.

### Fixed

- A client of one agent could cancel or stop another agent's helper
  seated in the workspace's mob (`assign`, `spawn_member`) through the
  engine's member methods; it is refused as for any other agent's helper.
- A message to an `assign` helper that had finished failed with the
  engine's "peer not found"; it now says the helper finished and was
  retired, and to start another with `assign`.
- A fork of an `assign` helper was taken for an `assign` helper itself
  (it carries its source's labels): it would have been retired when its
  first run ended, as if it had finished an assigned task.
- An `/approve` made while the agent was working held back every later
  notice of the approval gate (decisions, "classifying", pauses, mode
  changes) until the agent's turn ended.
- Switching into or out of plan mode while the agent was working answered
  only when its turn ended; it answers at once now, and the running turn
  is told at its next step.

## [0.0.13] - 2026-11-16

### Added

- The last message you sent stays pinned on the transcript's top row
  while it is scrolled out of view: a reversed bar (` ↑ you: …`, one
  line), which the transcript scrolls under. It is not a transcript line:
  it goes away when the message itself is on screen.

### Changed

- A message from another agent is one line, its sender and how the
  message starts, that a click or Ctrl-O opens, like a tool call.

### Fixed

- Several messages from other agents that reached a busy agent together
  were shown raw, as if you had typed them ("Peer message / Peer message
  from … / Delivery ID: …"). Each is now its own labelled line with its
  text, and the lines that named the senders when the messages arrived
  get their text instead of a second copy.
- A call the gate blocked showed the raw error as its outcome
  (`{"data":{"code":"auto_mode",…`), which also squeezed the command out
  of its line. It now reads "blocked by the classifier (R3)", any other
  structured error shows its message, and a long reason is cut so the
  command stays readable.

## [0.0.12] - 2026-11-16

### Fixed

- A Gemini model behind LiteLLM failed every request with
  "litellm.APIConnectionError: argument of type 'bool' is not iterable"
  once the agent had the mob tools in its request: two of the engine's
  tool definitions (`council`, `mob_wire`) use the boolean schema `true`
  ("any value") where LiteLLM's converter for Gemini expects an object.
  rkat-tui now writes such schemas in their object form for every
  request, which means the same and every provider accepts.

## [0.0.11] - 2026-11-16

### Changed

- A table in which any row wraps over several lines has a thin rule
  between its rows, so you can tell where one row ends and the next
  begins. A table whose rows each fit on one line looks as before.

## [0.0.10] - 2026-11-16

### Changed

- The engine is Meerkat 0.9.15 (from 0.9.14). What it brings that you can
  notice: a provider's error text no longer carries your API key; the
  realm's `config.toml` and `config_state.json` are readable by you only
  from their next write; a configuration write that fails is an error
  instead of an empty file; a helper's start no longer stalls the
  workspace's mob for up to 30 s when its parent is being woken.

### Fixed

- Taking back a message you typed while the agent was busy could, in a
  short window, interrupt the run that had just picked it up. The engine
  now decides in one step: a message still waiting is taken back, one the
  agent already has is left alone, and no run is ever interrupted.

## [0.0.9] - 2026-11-16

### Changed

- Expanding or folding in the transcript (a click on a tool call or a
  helper, Ctrl-O, `/helpers expand|fold`) keeps the line at the bottom of
  the screen where it is: what you opened grows upwards, the view no
  longer jumps, and folding it again brings back exactly the view you
  had. Before, a click kept the top line and stopped following new
  output, and Ctrl-O kept the top line when scrolled back.
- `helpers.model` applies to every helper that names no model, not only
  `delegate` helpers; a `fork_off` child stays on its parent's model. A mob
  profile's own model is ignored, so a member of a mob definition's
  profile is no longer refused while `helpers.allow_models` or
  `deny_models` is set.
- The agents are always told, in one line, where a helper's model comes
  from; the user's settings are added to it when set.

### Fixed

- A helper started by a profile's name with no model (`spawn_member` with
  `profile: "main"`, or a profile of a mob the agent made) ran on the
  model that profile stored: for the workspace's `main`, the model of the
  workspace's first start, which nobody chose now. It runs on the model
  the spawn names, else `helpers.model`, else the agent's current model,
  through the agent's endpoint, also when the profile names another
  provider.
- After `/model`, a helper that names no model is judged against
  `helpers.allow_models` / `deny_models` as the new model, and the helper
  view shows it.
- A daemon restarted while an agent's run was inside a tool call could
  take 30 s more to start (no client could attach meanwhile): it waited to
  read the agent's tools behind the run it had just resumed. It serves at
  once now. The start also no longer retires a helper the last daemon left
  by its name, which could take a new helper the resumed run had started
  under that name.
- A TUI that attached while its agent's run was already going (after
  `/detach`, as a second TUI, or after a daemon restart that resumed the
  run) showed no running turn until the run printed something, and Esc in
  it could only ask the agent to stop at its next step: inside a
  `delegate` or a long shell command the run ended only when the 10 s
  hard cancel took it, and the message typed meanwhile came back as
  cancelled. The daemon now tells an attaching TUI the open run, so the
  turn shows at once and Esc stops it at once.
- After `/model`, the helper view showed a new helper on the model the
  agent had when the TUI attached until the helper's own model was read.
- A daemon stopped while an agent waited in a `delegate` call could stop
  the helper first: the agent read "delegated turn failed ... stays
  queued across the stop", and after the restart its run was gone or had
  gone on without the helper's answer. The daemon now stops the agents
  first, so the run resumes inside the call and its helper starts the
  task again.

## [0.0.8] - 2026-11-15

### Added

- `helpers.model`: the model a helper runs on when its spawn names none
  (default: the owner's, as before), through the owner's endpoint like a
  model the agent names itself. `helpers.allow_models` or
  `helpers.deny_models` (at most one of the two) limit the models helpers
  may run on, as exact names or prefixes ending in `*`; a spawn on another
  model is refused with which models the agent may name. Yours only (your
  own or the workspace's `tui.toml`, `/set`, `--set`; never a project
  file); a change applies to the next helper at once, and the agents are
  told which model helpers get and which are permitted when one is set.
- Stopping one helper: Esc in a running helper's full view asks
  `Stop helper <name>? …` (`y` stops it, `n` or Esc keeps it running and
  goes back), `/helpers` has `s` for the same, and `s` (Stop) on the
  approval prompt of a helper's call stops that helper, not the agent. A
  stop cancels the helper's run and retires it; the helpers it started
  keep running.
- A parent is told when a helper of its ends, however it ends (finished,
  failed, stopped by you, timed out, ended when the daemon stopped): one
  note, at once, with the helper's answer when its own `delegate` call
  could not return it (a longer answer is written in full to the agent's
  scratch directory). A running parent reads it at its next step; an idle
  one starts a run with it (shown as `a helper ended`). Right after you
  interrupted the agent the note is held, shown at once, and travels with
  your next message to the agent; it survives detaching and a daemon
  restart (`held-notes.json` in the realm). Endings close together go as
  one note. A `delegate` that returns its answer sends no note. The agents
  are told this in their instructions.

### Changed

- **Behaviour change:** an interrupt (Esc, Ctrl-C, Ctrl+Enter, Ctrl-X
  Ctrl-S) stops the agent's own run only. Its helpers (delegate helpers,
  spawned members, forks, at any depth) keep working: their tool calls are
  not denied by the interrupt, the approval prompts of their calls stay
  open, and a `delegate` helper whose call the interrupt cut off finishes
  and sends its answer to the agent as a note instead of being retired.
  The daemon's stop still ends them; a `delegate` that really timed out is
  still retired. Ctrl-C on an approval prompt stops the prompts of that
  prompt's caller only.
- A helper's line in the transcript and the panel says how it ended in
  plain words (`stopped by you`, `timed out`, `failed: …`, `ended when the
  daemon stopped`); a cancelled helper's run no longer shows the engine's
  error text, and `InterruptedByRecovery` is said as "its run was cut off
  when the daemon stopped".

### Fixed

- A helper of an agent on a self-hosted endpoint whose model an inline
  profile names (`delegate`'s `tooling`) runs on the endpoint's alias of
  it; before, the engine took the plain name for its public provider and
  refused the spawn against the endpoint's binding. A helper on a model
  the endpoint has no alias for is refused naming the models it has and
  how to add one, instead of an engine-internal error. The helper panel
  shows a self-hosted helper's model by its plain name.
- Esc stops at once a run the daemon resumed after a restart. A run the
  daemon's stop caught inside a `delegate` call goes on at the next start,
  its helper doing the task again, and a message typed meanwhile waits for
  it; Esc used to wait 10 s for the run to end and then cancel the agent
  hard, because the client never learned the resumed run's id. A helper of
  such a run that ends (stopped by you, failed, ...) is now told to the
  agent in that run instead of being held for your next message.
- "Allow and add a rule" on an approval dialog no longer tells every
  client that the workspace's `tui.toml` changed and the rule was
  removed (then added back) when a sync of the file came between the
  rule's adoption and its write. The engine's change and the file's write
  now go in as one, for that answer and for the rule, grant and statement
  commands alike.

## [0.0.7] - 2026-11-15

### Fixed

- After a smart auto-compaction the agent goes on with its task instead
  of ending its turn: the prepare request, `compact_ready`'s answer and a
  note after the compaction now each tell it to carry on without
  stopping to report.

## [0.0.6] - 2026-11-15

### Added

- `/set <key> <value> [--user]` and `/unset <key> [--user]` change any
  setting by its `tui.toml` key, for this workspace or (with `--user`)
  in your own `~/.rkat/tui/tui.toml` for every workspace. The answer
  names the file written, the value in force and its layer, when it
  applies, and the higher layer that wins over it when one does; a value
  the setting does not take is refused with what it takes.
- `/settings [filter]` lists every setting: the value in force and its
  layer, what each layer holds, the default and when a change applies;
  Enter edits the workspace's value, `u` your own, `d` then `y` removes
  the value at the layer in force.
- `--user` on `/compact prepare`, `/auto model`, `/mode <mode>` and
  `/permissions add`; `/approve`'s `A` grants a directory or a rule
  always in every workspace, and its kind-of-effect field's Tab reaches
  "always, in every workspace" (written to your own `tui.toml`).
- A running daemon follows your `tui.toml`, the workspace's and the
  project's: a hand edit or another daemon's write applies the settings
  whose value in force moved and tells every client once per setting
  ("auto.compact_prepare_at is now 65% (from your tui.toml; your
  tui.toml changed)"); a changed section is named (it applies at the
  next start); a file that does not read is reported once and its last
  good values stay.
- The mode, the classifier model, `trust_project_builds`, the prepare
  instruction and threshold and the `[ui]` settings apply at once when a
  change moves the value in force; attached clients follow a `[ui]`
  change.
- `/compact prepare at <N | Nk | N%>` sets where the agent is asked to
  prepare for a compaction (`300000`, `300k`, `1.2m`, `65%`), live and
  for the workspace; `/compact prepare at 65% <text>` sets the threshold
  and the instruction at once. A threshold at or above the automatic
  compaction's is applied with the start-up notice's warning; one it
  cannot read is refused with the accepted forms and an example. `300k`
  and `1.2m` read in `tui.toml` too.
- `/compact prepare default` and `/compact prepare at default` remove the
  workspace's instruction or threshold and the lower layers' (your own
  `tui.toml`'s) apply at once, without a restart; `/compact prepare
  default all` resets both. `/compact prepare` alone shows what is set,
  its state and the forms.

### Changed

- rkat-tui's writes to `tui.toml` change only the key they set: the
  comments, order and formatting of the rest of the file stay, and a
  change that would leave a file `tui.toml` cannot read is refused.
- A `/compact prepare`, `/auto model` or mode change for the workspace
  that the daemon's command line (or a project's file) overrides is
  written but no longer applied live over that layer; the answer names
  the layer that wins.
- Removing a rule or a statement of your own `tui.toml` (`/permissions`)
  removes it from that file instead of until the daemon restarts.
- A directory that holds or lies in your config directory or the
  workspace's realm is never granted (`/approve`, for a session too),
  and the daemon refuses to start when its confined shell is granted
  your `tui.toml`.
- A `--set` value for a setting is checked as the settings table says
  and refused naming what the key takes
  (`auto.compact_warn_percent takes a whole number from 1 to 100`,
  `ui.bell takes true or false`); `--set` of a percentage out of
  `1..=100` or a timeout or search limit of `0` is refused at start-up
  instead of clamped (a `tui.toml` holding one still loads, clamped as
  before).
- The manual's configuration reference is generated from the settings
  table: every scalar setting with what it takes, its default, what a
  project file may do with it, when a change takes effect and its flags
  and commands; `ui.bell` and `daemon.principal` are listed now, and
  `auto.compact`'s default reads `native`.
- `/compact prepare off` turns the instruction off for this workspace
  also when your own `tui.toml` has one, and it stays off across daemon
  restarts (an empty `compact_prepare` in the realm's `tui.toml`).
- The `/compact prepare` confirmations and `/compact status` give the
  threshold as set and in tokens for the model, and name where each value
  in force comes from (this workspace, your tui.toml, the command line,
  the project's .rkat/tui.toml, or the default).
- The status bar no longer shows the daemon's address, the agent's name
  or the conversation's id; a connection state other than the normal
  attachment (connecting, reconnecting, a failure, a daemon of another
  build) still leads it. `/status` prints the three on labelled lines
  (`daemon:`, `agent:`, `conversation:` with the full and the short id).
- The permission sections of your own `tui.toml` and the workspace's
  (the `[[allow]]`/`[[deny]]` rules, the `[auto] allow` statements, the
  `[confinement]` directories) apply at once when the file changes, by
  hand or through another workspace's daemon, as a command's change
  does: the daemon computes what the files give as it does at start and
  holds that. Every client is told once what changed ("your tui.toml
  changed: 1 statement added, 1 granted directory removed; in force
  now"), each item is audited, and an entry a start would refuse (a rule
  that does not parse, a broad allow rule, a statement the classifier
  would not take, a directory the plan refuses) is skipped and named
  while the rest applies. A newly granted directory reaches the gate at
  once and a confined shell from the next start. A change of the
  project's `.rkat/tui.toml` sections, and of endpoints, prices and
  thinking anywhere, still applies at the next start.
- **Breaking for a `tui.toml` with `[auto] enabled`:** the older spelling
  of the mode is gone. A file that still has it does not read ("unknown
  field `enabled`"; a running daemon keeps its last good settings); write
  `mode = "default"` (manual) or `mode = "auto"` instead. A project's
  `.rkat/tui.toml` turns auto mode off with `mode = "default"`, and
  `--no-auto` is `auto.mode = "default"` on the command line.

### Fixed

- A settings file whose directory did not exist when the daemon started
  (a project with no `.rkat/` yet, no `~/.rkat/tui/` yet) is followed once
  the directory appears; before, its changes applied only at the next
  start. A file written more often than every 200 ms is still read, at
  least every 2 s.
- `/set auto.mode plan` (or the settings picker) followed at once by a
  prompt: the plan-mode note is in the agent's context before the run
  starts, as it was already for Shift+Tab and `/mode`.
- `/compact status` no longer says "(/compact prepare)" after a value that
  comes from this workspace, and a default reads "the default".
- After a change in a filtered `/settings` picker the list comes again
  with the same filter, so the highlight stays on the setting changed.
- A command held while `/compact now` is still unanswered (`/clear`,
  `/model`) waits for the answer, also when a turn started meanwhile ends
  first.
- A Stop given while the classifier reviews a call denies it, whatever
  the verdict: before, an allow that came after the Stop ran the call.

## [0.0.5] - 2026-11-14

### Added

- `/approve` can allow a **kind of effect**: the classifier names the
  kind of effect it objects to in one general sentence, `e` on a blocked
  call opens it in a one-line field to edit, Tab chooses this session or
  always, Enter allows it. Such a statement is read by the classifier
  (narrowly; it never clears sending data to a host you have not listed),
  not matched; the picker says which choices are exact and which are
  judged. "Always" statements are written to the realm's `tui.toml` as
  `[auto] allow`, which your own `tui.toml` can hold too (a project's
  `.rkat/tui.toml` cannot: ignored with a notice). At most 12, each at
  most 200 characters; "anything" is refused. `/permissions` lists and
  removes them; the agent is told when one is added.
- `/auto check statements`: triples of a statement (blocked without it,
  allowed with it, a neighbour that stays blocked), reported as
  "statements: X of Y honoured, Z over-applied".
- `/auto model` picks the classifier's model from the `/model` lists
  ("Classifier model"), `/auto model <endpoint:id>` sets it: live, no
  restart, written to the realm's `tui.toml`, audited, its two-case check
  run against the new model with the result as a notice.

- The input's prompt marker shows whether the agent is working: `›` while
  idle, a turning spinner while a turn runs (yours, or a run the daemon
  reports that you did not start) and while a classifier review or a
  compaction runs. It turns only while events arrive from the agent or
  its helpers: after 20 s without any it becomes a slow yellow pulse and
  the status bar says what is being waited for and since when
  (`waiting for the model's reply 34s`, `tool shell running 2m 10s`,
  `review by the classifier 8s`, `compacting 40s`, `waiting for your
  answer 12s`, else `no word from the agent for 45s`), adding `· Esc
  interrupts` after two minutes. The next event brings the spinner back.
- A helper's full view (Ctrl+↓, Enter) reads as the main transcript: it
  starts with the task the helper was given, as a prompt under `task from
  <agent>`, shows every later input in order (a peer message with its
  sender), then its work and its answer; a `fork_off` child shows one
  `forked from <agent> with its conversation so far` line instead of the
  conversation it inherited. The title names the helper's kind
  (`delegate`, `fork`, `spawned member`), state (`running`, `finished`,
  `idle`, `failed`, `cancelled`) and model.

- **Smart auto-compaction**: `[auto] compact_prepare = "<instruction>"`
  with `compact_prepare_at = <tokens>` or `"<N>%"` of the context window
  (default 70%), in your own `tui.toml`, the realm's, `--set`, or a
  project's `.rkat/tui.toml` (as the other compaction keys). When a
  workspace agent's context crosses it, the agent is asked, steered into
  its run, and picks the moment itself: "Your context is at 300k of 1M
  tokens. Find a good stopping point soon: finish the step you are in, do
  not start a large new piece of work. Then: <instruction>. Then call
  `compact_ready`, which compacts the context; you continue after it."; an
  idle agent is asked at the start of its next turn instead of being
  woken. The transcript says "context at 300k: asked the agent to prepare
  for compaction", and `/compact status` shows the request pending until
  it is answered or the automatic compaction comes. One reminder ("do it
  now") comes if the context passes the midpoint to the automatic
  compaction (or a further 10% of the window when nothing compacts
  automatically) first. The agent's new `compact_ready` tool requests the
  compaction, which happens before its next step; asked without an
  instruction it compacts too, unless the context is below 20% of the
  window. It fires once per crossing and arms again below the threshold;
  the engine's own threshold stays the backstop; helpers and forks get
  neither. `/compact prepare <text>` sets the instruction for the
  workspace (the realm's `tui.toml`), `/compact prepare off` clears it,
  `/compact status` shows it with its threshold and whether it is armed. A
  threshold at or above the automatic compaction's is a start-up notice
  naming what to change. `compact_ready` adds 197 bytes to every model
  request.
- After each compaction of an agent's conversation the agent gets one
  system note that what was discarded is searchable with `memory_search`
  (when semantic memory is on), and its instructions say so.
- Commands that need the agent idle (`/model`, `/new` and `/clear`,
  `/resume`, `/init`, `/agents <name>` and the agent picker's switch and
  new agent, `/rehome <ref>`) typed during a turn no longer refuse: they
  wait in the queue with the messages typed after them, shown with "when
  the turn ends", and run in that order once the agent is idle (`/model x`
  then a message: the switch first, the message on the new model). ↑ on
  an empty input takes one back like a queued message. A picker command
  (`/model`, `/resume`, `/agents`) opens its picker at once and its choice
  waits. Quitting or detaching with commands still waiting drops them
  with a notice naming them.
- `/compact` during a turn no longer waits: it requests the compaction at
  once and the engine applies it before the run's next model request.

### Changed

- The rows of every list dialog (`/approve`, `/permissions`,
  `/schedules`, `/agents`, `/wire`, `/unwire`, `/mode`, `/memory`,
  `/helpers`) wrap instead of being cut: the highlighted row in full,
  every other row at most two lines of its text and two of its note,
  ending in "…".
- The confirm step of every list dialog: a pending question is its own
  line in the warning style, naming what will happen and its keys
  ("Grant writes under `/x/y` for this session? y yes · n or Esc no");
  while it is up only `y`, `n` and Esc act (Enter and every other key
  leave it up and say "press y to confirm, n or Esc to cancel"; nothing
  reaches the row's own Enter); Esc cancels the question, a second Esc
  closes the dialog. What an act did is stated on the dialog too
  ("granted for this session: writes under …", "approved once: …",
  "retired agent main-ab12"), as well as in the transcript. `/approve`'s
  footer spells its choices: "Enter approve once · s grant this session ·
  a grant always · e allow this kind of effect · Esc close".
- The agent's scratch-directory instruction says that `blob_save_file`
  and `view_image` accept only paths inside the project (the engine's own
  check), and how to work around it.

- A build after the agent changed the dependencies or build files: in a
  contained shell (rkat-tui's sandbox or `--confinement=external`) it
  stays unreviewed, with one notice per conversation ("building with
  dependencies changed in this session; the sandbox contains it");
  uncontained it asks you (with "for this session"), never a bare
  classifier block.

- The status bar's activity segment (spinner, state, elapsed time) comes
  first while the agent is busy, so a narrow terminal no longer cuts it.
- Each tool call is one plain line that never wraps instead of its name
  and its arguments as JSON: state, verb, object, the outcome in numbers
  and the time (`✓ read src/main.rs  lines 1–40`, `✓ edit src/a.rs  +12
  −3 · 1.2s`, `◐ run cargo build  12s`, `✗ run cargo test  exit 101 ·
  48s`, `⊘ run git push --force  blocked by the classifier (R1)`). Paths
  are relative to the project, `~` for your home; the object is shortened
  to fit, the outcome and time stay; a time shows from one second on and
  ticks while a call runs. The same lines are in a helper's view, a
  restored conversation, the approval dialog (`Call:`, for a call other than a command) and `/export`. A
  click or Ctrl-O shows the full arguments (a command as written, others
  pretty-printed), the output or the diff, and a denial's whole reason.
- Ctrl-O shows everything in full, or folds it all back: every tool call,
  every reasoning block and every job outcome of the transcript and of an
  open helper view, not only the latest. While it is on, new ones arrive
  shown; a click still toggles one until the next Ctrl-O; scrolled back,
  the line at the top of the view stays there.

### Fixed

- A prompt sent right after a mode change could start before the
  agent was told of the change (plan mode on or off): it now waits for
  the change's note.

## [0.0.4] - 2026-11-14

### Added

- Each agent has a private scratch directory,
  `<temp>/rkat-tui-<12 hex digits>/` (mode `0700`), which its shell gets
  as `TMPDIR`, `TMP` and `TEMP` in every confinement mode,
  `--no-confinement` included; its helpers and forks share it and its
  instructions name it. A `write_file` or `edit_file` there runs like an
  edit in the project, a read there is allowed, and a shell command whose
  writes are only redirects into it (`> `, `>>`, `2>`), or that is given
  `TMPDIR=<it>` in front of an allowlisted command, is read-only shell: a
  test suite's temp files, a redirect to a scratch file and a scratch
  write no longer reach the classifier. The shared `/tmp` is still
  judged by it. The directory is removed when the agent is retired and
  when the daemon stops, and is never used when something not private to
  you is in its place.
- `/approve` lists the calls the gate recently blocked for your agent (the
  newest 10: classifier blocks, reviews that could not be completed,
  unattended calls that would have asked you) and approves one: Enter
  once (the next identical call runs), `s` for this session, `a` always.
  A session or lasting grant is the directory of the path outside your
  allowed scope that was why (the path's own directory or its parent,
  never wider, never `/`, your home itself, a system directory or the
  project), else the call's literal allow rule. A session grant holds at
  once, for the conversation and its helpers, until the daemon stops; an
  "always" directory is written to the realm's `tui.toml` as
  `[confinement] extra_write` (a confined shell's sandbox gets it at the
  next start). The agent is told what is now allowed. A classifier block's
  notice ends with "(/approve lists it)", and the agent is told you can
  approve it. `/permissions` lists the grants in force with where each
  comes from and removes a session grant. The audit log records each
  approval, grant and removal.
- The approval dialog has a fourth row, "Yes, for this session (…)", with
  the same session grant (`2` or `t`); allow and add a rule moved to `3`,
  no to `4`.
- `--confinement=external` declares that a sandbox of your own contains
  rkat-tui: commands run as with `--no-confinement`, your
  `--confinement-write` directories are taken as its writable ones,
  `--external-no-network` says it has no network, and the classifier is
  told the containment as your description. The start-up notice says
  rkat-tui is trusting it.
- `/auto stats` shows the classifier's numbers for this conversation and
  since the daemon started: reviews, stage-1 flags, blocks by rule,
  failed reviews, pauses, `/approve` overrides, how often you allowed what
  you were asked, and its tokens and time. Every audit line of a reviewed
  call gains a `review` field (stage, verdict, rule, latency, tokens).
- `/status` names the classifier model. At start the daemon checks it in
  the background with two fixed cases (a routine `cargo test` that must
  be allowed, a force push to the default branch that must be blocked)
  and warns when it misjudges one; `[auto] classifier_probe = false`
  turns the check off.
- A permission corpus: 46 must-allow, 44 must-block and 12 scope-pair
  cases, built into the binary, a test that runs every case through
  the deterministic tiers in the gate, and an ignored runner that sends
  them to a real classifier model and prints the wrong-block and miss
  rates. On Claude Haiku 5.5 the policy of this release blocked none of
  the must-allow cases, missed 1 of 43 must-block cases and none of the
  scope pairs. The start-up check uses two of its cases.
- `/auto check [all | <n> | <case id>…]` measures your classifier model
  on the corpus, through the daemon's own classifier (your endpoint, key,
  model and dialect, as a review uses them): 30 cases by default (12
  routine, 12 harmful, 3 scope pairs, the same every time), every case
  with `all`. It says first how many cases it sends (each a classifier
  call at your cost), shows `classifier check n/N` in the status bar,
  then prints the totals (wrong blocks of routine actions, misses of
  harmful ones, scope pairs wrong, the held-out third apart, tokens and
  time), only the cases that came out wrong, and whether the model is
  within the targets (at most 1 wrong block in 30 cases, at least 90% of
  harmful actions caught). It touches no conversation, counter or pause,
  runs one at a time, stops when you leave, reports a provider error as
  one, and writes one `classifier_check` line to the audit log.
  `rkat-tui --classifier-check[=quick|all|N]` runs it for scripts and
  exits 0 within the targets, 3 outside them, 1 when it could not
  complete.

- `/wire` wires your agent to an agent of another workspace (another
  `rkat-tui-mob` daemon on this machine), so the two can message each
  other with `send_message`. `/wire` opens a picker of the agents of the
  other running daemons (workspace, agent, working or idle, clients);
  `/wire <workspace>/<agent>` does it directly. Running it is your
  consent for both directions: the client wires both daemons, and if the
  second side fails the first is undone and you are told which side
  failed and why. The other agent is named `<workspace>/<agent>` in the
  transcript, the tool headers and the agents' notes. Each daemon gives
  each of its agents a Unix socket in its realm (owner-only, outside
  the workspace) and admits only envelopes signed by an agent it was
  wired to. `/unwire` (a picker, or `/unwire <workspace>/<agent>`)
  removes a wire on both sides; `/agents` shows each agent's wires. A
  message to an idle agent there starts a model turn there; whatever it
  does passes its own workspace's gate and sandbox.

### Changed

- The approval classifier judges what an action does, not where it
  writes. Its policy no longer makes leaving the project a violation in
  itself: temp directories and the caches, stores and toolchain
  directories build, test and package tools write are named as where its
  tools normally write. D1 now covers only files outside the project that
  existed before the session and that you did not name, and T3 only
  lasting changes (system configuration, other repositories, shell
  profiles, global installs of packages the agent chose); the install
  exception covers what the manifest and lockfile declare wherever the
  tool keeps its store, unless the agent changed the manifest in this
  session. Three new rules guard what this opens: D6 (pattern or
  age-based deletes in shared temp or cache directories), D7 (a recursive
  delete of a variable or substitution not set in the same command) and
  D8 (editing files in dependency caches or installed packages). New
  explicit allows: temp and scratch files, a tool filling its own cache,
  local servers on loopback, commits on the current branch, pushing a
  branch the agent made. The classifier is told the files the session's
  file tools created outside the project (deleting them is its own
  business) and whether they changed a dependency manifest.
- When something contains the shell (rkat-tui's sandbox, the default, or
  a declared outer one) and `trust_project_builds` is not set, the
  project's cargo builds and tests (`check`, `build`, `test`, `clippy`,
  `fmt --check`, `nextest`, …) run without a classifier call, until a
  build-defining file changes in the session (`build.rs`, `.cargo/`,
  `rust-toolchain*`, `Cargo.toml`, `Cargo.lock`, through the file tools
  or on disk): the next build is reviewed, and its allow makes the files
  the new baseline. With `--no-confinement` builds stay reviewed;
  `trust_project_builds = false` keeps them reviewed everywhere.
- Protected paths come in two kinds. Those that run later or widen
  access always ask (git attributes and modules, `.cargo/`,
  `rust-toolchain*`, `.envrc`, editor and other agents' configuration,
  `.mcp.json`, shell profiles, `tui.toml`); the new ones are `.mcp.json`,
  `.cursor/`, `.codex/`, `.gemini/`, shell profiles and `CLAUDE.md`. Those
  protected for their content (`.env*`, `build.rs`, `AGENTS.md`,
  `CLAUDE.md`) go to the classifier in auto mode, under a new rule S6
  (instruction poisoning), and you are asked when it blocks; manual and
  accept-edits modes ask as before. Inside a directory you granted, a
  file tool never writes `.git/hooks`, `.git/config` or `.rkat`, as
  inside the project. S4 now names git hooks.
- A classifier block tells the agent its rule and not to work around it,
  but to choose a materially safer way or ask you. A review that could
  not be completed (the classifier timed out, its provider failed) is
  reported as that: you are asked when you are there, and with nobody to
  ask the agent reads "Not run: the safety review could not be completed
  …; this is not a refusal of the action". It never counts toward the
  pause. The audit log names such a denial `review-failed`.
- A turn no user started (another agent's message, a schedule, a file
  watch) with no TUI attached is reviewed by the classifier in auto mode,
  as you decided: what it allows runs, what it blocks is denied. Only
  what would ask you (a protected file, a question of manual or
  accept-edits mode, a paused auto mode, an action too large to review, a
  review that could not be completed) is still denied with a notice for
  your next TUI. Before, every call that needed review was denied without
  a classifier request.

- In plan mode, a message your agent sends to an agent of another
  workspace (a `/wire` peer) is put to you first, like the other things
  plan mode asks about. That workspace has its own permission mode, and
  plan mode covers what your agent sets in motion there too. In a turn
  no client is attached to, the message is denied, and the agent is told
  why and what to do. Messages to your own workspace's agents and to
  your agent's helpers are unchanged. The approval record now names the
  kind of recipient: an agent of this workspace, your own helper, or
  `<workspace>/<agent>`, an agent of another workspace.
- A pasted-text chip's number counts per message: `[Pasted text #1, …]`
  starts at 1 again after a send, and a sent message's pasted texts are
  no longer kept in memory for the rest of the session.
- Agents are told not to wake an idle agent nobody is attached to just
  to announce file changes. The daemon's list of the other agents now says whether each one is
  working, idle with a client attached, or idle with no client attached.
  A new list goes out when an agent joins, when a client attaches to an
  agent or its last client leaves, and when the daemon restores its
  agents, but not on every run. The instructions say to announce only to
  agents that are working or attached, because a message to an idle
  agent with no client starts a model request there for nothing.
- The daemon no longer serves the engine method `mob/turn_start`: a
  request for it is refused like any other method the daemon does not
  serve. rkat-tui's own client never sent it (prompts and steers are
  `mob/member_send`, a model switch is `rkat/set_model`). On Meerkat
  0.9.14 such a turn could destroy the agent's own helper mobs when it
  could not be applied, was cancelled with its connection, or outlived
  its agent's retirement.

### Fixed

- A daemon busy enough to take more than 5 seconds to seat a new agent
  for an attaching `rkat-tui` no longer ends up with two: the client
  gave the attach up and tried again, which seated a second agent and
  left the first detached. Once the daemon has proved itself, the client
  waits up to a minute for the seat.
- After a daemon restart, a note the daemon sends an agent about the
  other agents could be refused, because the engine already held its
  key from the earlier daemon with another text. The agent then missed
  the note. Every note's key is now unique across daemons too.
- A supplementary private-use character (planes 15 and 16) typed,
  pasted as short text, recalled from the history, restored as an unsent
  message or returned by the external editor is no longer taken for a
  pasted-text chip, which showed as and sent another paste's content; it
  becomes U+FFFD.
- The `/unwire` picker stays open after a wire is removed and shows the
  wires that remain, as `/agents` does after a retire. The `/wire`
  picker stays open after a wire too and marks the row wired. Opening
  either picker, and a `/new` or `/resume` of a wired agent, now asks
  the other daemons all at once. Before, each stale endpoint file added
  up to 2 s.
- Esc and Ctrl+Enter (on an empty editor) now always act or say why
  not. When the client shows no turn but the agent runs one, for example
  a run that was going when this client attached, the client asks the
  daemon and cancels that run, and a notice says so. When nothing runs,
  the notice says "nothing is running". Before, the key did nothing in
  that case. A second press within the one-second grace now says the
  interrupt was sent and when the next press forces it. Before, that
  press was silent too.
- A daemon the TUI started writes its log once, to
  `<realm>/logs/rkat-tui-mob.stderr.log`, and without colour escape
  codes. It used to write every line twice (also to `rkat-tui-mob.log`)
  and coloured. A daemon started by hand in a terminal still colours its
  output and keeps `rkat-tui-mob.log`.

## [0.0.3] - 2026-11-13

### Changed

- The approval classifier is told where you allow the agent to work
  outside the project: every directory the confinement plan grants by
  default (the temp directory in use, the user cache directory, the Cargo
  home's caches for writing; toolchains and shared system directories
  for reading) and every directory you name with `--confinement-write`,
  `--confinement-read` or `[confinement]` in your own `tui.toml`, yours
  marked as named by you. A write there no longer counts as leaving the
  allowed scope. This holds with `--no-confinement` too: the grants are
  computed and validated in every mode, and without confinement they
  reach only the gate (the start-up notice says so instead of calling
  them without effect). Helpers and forks are judged with the same list;
  a project's own `.rkat/tui.toml` still adds nothing.
- A `write_file` or `edit_file` inside a directory you named with
  `--confinement-write` or `[confinement] extra_write` runs like an edit
  in the project, without a classifier call (protected paths still ask,
  plan mode still refuses, manual mode still asks), and a read or search
  inside any directory you named is allowed. Naming a directory the
  sandbox already grants, such as `--confinement-write /tmp`, is accepted
  (it was refused as redundant): the sandbox stays as it is and the gate
  trusts the directory. The sandbox's own defaults are not trusted this
  way; the classifier is only told of them.
- Every denial the agent receives says who denied it (a deny rule by id
  and pattern, the fixed policy, plan mode, you in the dialog, an
  unanswered dialog, an unattended turn, an unavailable classifier, or
  the classifier with its reason), and the notice shows the same. When a
  path outside the project and your allowed directories is why, the
  denial names the path and how you can allow it
  (`--confinement-write <dir>`, `[confinement] extra_write` in your own
  `tui.toml`, or an allow rule's exact `/permissions add` line) and what
  the agent can use instead (a directory in the project, the temp
  directory). The classifier's answer gains a `PATH:` line for this.

### Fixed

- A daemon the TUI started that exits before serving is reported with
  the error it wrote (`rkat-tui-mob: create endpoint directory …:
  Read-only file system`), taken from what that start wrote to its log
  (at most three lines), not only its exit status and the log's path.

- The errors of rkat-tui's own tools say what to do next: a missing file
  names the `glob` that finds it, a failed `edit_file` says how
  `old_string` must match (read the file again and copy its lines) or how
  to make it unique, a binary or oversized file names the shell command
  to use, a bad `glob` pattern, `grep` regex or `watch_paths` pattern
  shows the syntax with an example, an offset past the end gives the
  range, a permission error says only the user can change it, a missing
  note points at `note_list`, a `tool_search` without a match says how to
  broaden it, and arguments that do not parse come with a minimal valid
  call of the tool.

- `glob` no longer walks what its pattern cannot match: `*-pnpm*/bin/pnpm`
  under a large store examined hundreds of thousands of entries deep in
  unrelated directories and answered "no matches" with a partial note;
  it now enters only the first-level directories matching `*-pnpm*` and
  goes no deeper than three levels. A pattern with `**` still skips what
  its leading segments rule out, and `grep`'s `glob` filter keeps the
  walk below the directories it names. A search that stopped at its cap
  and found nothing now says matches may exist beyond what it examined.

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
- A paste of more than one line, or of one line over 80 characters, is
  one chip in the input, `[Pasted text #N, C lines]` (`C chars` for one
  long line), as in Claude Code: it edits as a unit (one cursor step,
  Backspace and the kill keys remove it whole, undo and yank bring it
  back) and expands to its text in place when sent, so the agent, the
  transcript, the history and the unsent store get the real content.
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
