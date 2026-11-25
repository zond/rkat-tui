# rkat-tui user manual

How rkat-tui works in full: sessions, approvals, the sandbox, helpers,
rules, endpoints, memory and the keys. The [README](../README.md) is the
short version; [`testing.md`](testing.md) is the test protocol for working
on rkat-tui itself.

## Contents

- [Overview](#overview)
- [First run](#first-run)
- [Sessions](#sessions): [Reading the transcript](#reading-the-transcript), [Checkpoints of long runs](#checkpoints-of-long-runs)
- [How approval works](#how-approval-works): [What the classifier sees and blocks](#what-the-classifier-sees-and-blocks), [The approval dialog](#the-approval-dialog), [The classifier's model](#the-classifiers-model), [Checking your classifier model](#checking-your-classifier-model), [The permissions dialog](#the-permissions-dialog), [The project's hosts and the hosts you trust](#the-projects-hosts-and-the-hosts-you-trust), [Permission modes](#permission-modes), [What runs as the host](#what-runs-as-the-host)
- [Confinement](#confinement): [The scratch directory](#the-scratch-directory), [More directories](#more-directories), [Security decisions](#security-decisions), [Security model and its limits](#security-model-and-its-limits), [The file tools' limits](#the-file-tools-limits)
- [Helpers](#helpers): [Tools](#tools), [How the parent hears of it](#how-the-parent-hears-of-it), [Interrupting and stopping](#interrupting-and-stopping), [Watching helpers](#watching-helpers), [Limits](#limits), [Helper models](#helper-models)
- [Settings](#settings)
- [Rules](#rules): [Configuration reference (`tui.toml`)](#configuration-reference-tuitoml)
- [Models, endpoints and cost](#models-endpoints-and-cost): [Setting up an endpoint](#setting-up-an-endpoint), [How endpoints reach the engine](#how-endpoints-reach-the-engine), [Choosing a model](#choosing-a-model), [Thinking](#thinking), [Context gauge and cost](#context-gauge-and-cost)
- [Instructions and memory](#instructions-and-memory): [Context and compaction](#context-and-compaction)
- [Skills](#skills)
- [Keys and commands](#keys-and-commands): [Commands](#commands), [Keys](#keys), [Task checklist](#task-checklist)
- [Where things live](#where-things-live): [Moving or renaming a workspace](#moving-or-renaming-a-workspace), [Deleting a workspace's state](#deleting-a-workspaces-state), [The audit log](#the-audit-log), [When the disk fills](#when-the-disk-fills)
- [The workspace daemon](#the-workspace-daemon): [Which daemon, which agent](#which-daemon-which-agent)
- [Several agents](#several-agents): [`/agents`](#agents), [`/quit` and `/detach`](#quit-and-detach), [Messages between agents](#messages-between-agents), [`contacts`](#contacts), [Connecting helpers](#connecting-helpers), [What is and is not kept apart](#what-is-and-is-not-kept-apart)
- [Wiring agents across workspaces](#wiring-agents-across-workspaces): [Agents on another machine (over ssh)](#agents-on-another-machine-over-ssh)
- [Timers and watches](#timers-and-watches): [Schedules](#schedules), [Tools an agent reveals](#tools-an-agent-reveals), [File watches](#file-watches)

## Overview

rkat-tui is a terminal coding agent built on the Meerkat agent engine. The
agent runs in a per-workspace daemon, `rkat-tui-mob`, so it keeps working
when you leave; `rkat-tui` is its client and starts the daemon when none
runs (see [The workspace daemon](#the-workspace-daemon)).

Every tool call passes an approval gate. In **auto mode**, the default,
routine actions run, risky ones go to a classifier that never sees the
agent's reasoning, and you are asked only after repeated blocks. Shift+Tab
switches to manual, accept-edits or plan mode. The agent's shell runs in
an OS sandbox.

```bash
rkat-tui "fix the failing test"   # a new agent with a first request
rkat-tui -c                       # join the agent that was active last
rkat-tui --attach=../other        # attach to another workspace's running daemon
rkat-tui --resume ~2              # join the agent of the 2nd most recent conversation
rkat-tui --model gpt-5.5 --classifier-model gpt-5.5
rkat-tui --no-auto                # manual mode: you answer what the classifier would review
rkat-tui --model llama3:8b        # a bare id is looked up in every endpoint's model list
rkat-tui --endpoint-def litellm=openai=http://localhost:4000,key_env=LITELLM_KEY --model litellm:gpt-5.5
rkat-tui --allow 'shell=cargo nextest run*' --set auto.compact=auto
rkat-tui --no-confinement         # run the shell unsandboxed (see "Confinement")
```

## First run

1. **Install.** `cargo install rkat-tui --locked` installs `rkat-tui` and
   its daemon `rkat-tui-mob` (Rust 1.97 or later; Linux and macOS).
   `cargo binstall rkat-tui` installs prebuilt binaries on x86-64 Linux
   (glibc 2.35 or later) and Apple-silicon macOS, and builds from source
   elsewhere.

   A plain `cargo install` builds the bundled SQLite with defaults that
   make concurrent sessions in the daemon queue on three process-wide
   locks (the daemon's log says so). This builds it without them:

   ```bash
   LIBSQLITE3_FLAGS="-DSQLITE_DEFAULT_MEMSTATUS=0 -USQLITE_ENABLE_MEMORY_MANAGEMENT -DSQLITE_DISABLE_PAGECACHE_OVERFLOW_STATS" cargo install rkat-tui --locked
   ```

   The prebuilt archives, and `cargo install --path .` in a checkout,
   already have these flags.

   On Linux the sandbox needs unprivileged user namespaces; without them
   rkat-tui refuses to start and explains how to enable them (on Ubuntu
   24.04 and later, an AppArmor profile). macOS uses Seatbelt; other
   platforms have no sandbox. `--no-confinement` runs the shell
   unsandboxed (see [Confinement](#confinement)).
2. **A provider key.** Export `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or
   `GEMINI_API_KEY`, or set a key in the realm config, the `rkat` token
   store or an endpoint of your own (see
   [Models, endpoints and cost](#models-endpoints-and-cost)). Without one,
   rkat-tui refuses to start.
3. **The first request.** In your project, run `rkat-tui` and type a
   request, or pass it: `rkat-tui "fix the failing test"`. Tool calls run,
   go to the classifier or ask you (see
   [How approval works](#how-approval-works)). `?` on an empty input shows
   the keys, `/help` the commands.
4. **The daemon.** The first `rkat-tui` in a workspace starts its daemon
   in the background; later ones attach to it. The conversation, its
   turns and its helpers live there and go on with no terminal attached.
   A second `rkat-tui` in the same directory gets an agent of its own,
   wired to the first; `rkat-tui -c` joins the existing one instead (see
   [Which daemon, which agent](#which-daemon-which-agent) and
   [Several agents](#several-agents)).
5. **Leaving.** `/detach` leaves the agent working; `rkat-tui -c` brings
   you back. `/quit` retires your agent (its conversation stays, and
   `rkat-tui -c` continues it later) and stops the daemon when this
   `rkat-tui` started it and is the last one attached; otherwise it leaves
   the daemon running or asks (see [The workspace daemon](#the-workspace-daemon)).

## Sessions

A session is one of the workspace daemon's conversations. Its transcript,
model and cost totals survive restarts:

- `rkat-tui -c` joins the agent active last, with its conversation.
- `rkat-tui --resume <ref>` joins the agent that owns that conversation.
- `/resume [ref]` switches your agent to another conversation.
- `/clear` (or `/new`) starts a fresh one.

A ref is `last`, `~N` (the N-th most recent), an id, an id prefix or the
short id shown; `/resume` or `--resume` alone opens a picker. While the
daemon runs it holds a lease on the workspace's realm, so `rkat realm
prune` treats the realm as live.

**Typing while the agent works.** Enter on an idle session starts a turn.
While a turn runs, Enter *steers*: the message goes into the running turn
at its next model boundary (between tool calls), or runs as a follow-up
when no boundary comes. Typing never interrupts. Every message waiting
when the next model request starts joins that request, each as its own
message in the order sent, together with any daemon notes (a helper's
ending, an approval, a watch's batch) and other agents' messages that
arrived meanwhile. A request already under way never takes a new message,
so a message sent during a tool call waits for that tool's result. A
message sent while the agent looks idle works the same way: it joins
whatever run the agent has by then, or starts one. Only `/compact`'s own
quiet run is sent on its own.

**Interrupting.** Esc or Ctrl-C while a turn runs interrupts it. (The
first press after a mouse selection only clears the selection.) It stops
the agent's own run: an open model request or shell command is dropped at
once. Its helpers keep working, and how each ends reaches the agent with
your next message (see [Helpers](#helpers)).

Every message you typed during the turn that the agent has not read yet
comes back to the editor, in order, and starts nothing by itself; you
decide whether to send it. A message the agent has read stays in the
conversation. Pressing again soon after says when the next press forces
the run; a later press cancels it at once. Presses right after the run
ended do nothing, so they never clear what came back. If the run has not
ended 10 s after the interrupt, the client cancels it; if even that is not
confirmed, a notice says the run may still be going, and what you send
next waits behind it.

After an interrupt, a call the run still makes that needs a review is
denied, and the agent is told you interrupted. The prompt of an
interrupted run is not kept in the conversation the model sees, though the
transcript still shows it. Esc with no turn shown still asks the daemon to
stop the agent's run, in case it is running one this client does not show
(for example one started before you attached).

**Send now.** Ctrl-X Ctrl-S interrupts the turn and sends, as the next
run, your messages the agent has not read and then the editor's text.
When idle it sends as Enter does.

**Taking messages back.** ↑ on an empty editor takes back every message
the agent has not read yet, in the order typed. A message the agent
already took stays; only Esc stops the turn.

**How a turn ends.** A turn ends when the daemon reports the agent idle:
no run open, no message on its way, and nothing the runtime still holds
for it. The UI says whether it completed, failed or was interrupted. A
message the agent has not read when the turn ends stays marked unread
until it is read, and ↑ can still take it back. One the agent never takes
(a forced cancel, the agent retired or stopped) returns to the editor with
a notice saying why.

**When the daemon stops.** Messages still waiting come back to your
editor as unsent and are saved in the workspace's `tui_unsent.txt` (see
[Where things live](#where-things-live)), which the next start announces.
So does a message that was shown delivered when the stop cut off the
model request that carried it: the engine keeps a message only once the
step that read it is saved (its tools started, or the run ended). A
detach loses nothing; waiting messages stay with the agent.

**No rewind.** There is no `/rewind`: the daemon cannot continue a
conversation from before an earlier message. `/new` starts over.

**Quitting.** Ctrl-D on an empty editor quits on a second press within
3 s; `/quit` quits at once when idle. While a turn runs or messages are
pending, both warn first; a second press within 3 s interrupts the turn
and saves every message the daemon never received to `tui_unsent.txt`.
Whether the daemon stops too: see
[The workspace daemon](#the-workspace-daemon).

### Reading the transcript

Each tool call is one line: the state (`○` asked for, `◐` running, `✓`
done, `✗` failed, `⊘` denied), a verb, what it acts on, the outcome, and
the time once it passes a second:

```
✓ read src/main.rs  lines 1–40
✓ edit src/ui/render.rs  +12 −3 · 1.2s ▸
◐ run cargo build  12s
✗ run cargo test  exit 101 · 48s ▸
✓ search "fn main" in *.rs  3 files
⊘ run git push --force  blocked by the classifier (R1) ▸
```

Paths are relative to the project. The outcome shows only what the call's
result says for certain: line counts, `+A −R`, a non-zero exit, match
counts (`(stopped early)` when a search's budget ran out). A failure adds
its reason; a denial says who denied it (the classifier and its rule,
you, a rule, plan mode). `▸` means there is more: a click on the line, or
Ctrl-O for every call, shows the full arguments, the output or diff, and a
denial's whole reason with how to approve it in `/permissions`.

Finished, successful calls of one tool in a row fold into one line:

```
✓ read 5 files: src/a.rs, src/b.rs, src/c.rs and 2 more ▸
✓ edit PLAN.md ×3  +7 −3 ▸
✓ run cargo test ×3 ▸
```

Commands fold only when they are the same command. A call that failed,
was denied, waits for approval or still runs is never folded, so a
failure always has its own line. A click opens a folded line into its
calls. `/export` writes every call's own line.

**Scrolling.** At the bottom the transcript follows new output. Scrolled
up (PgUp, Shift+↑, the wheel), what you are reading stays put while output
arrives, and the input's top line counts the new lines below; End
(Ctrl+End with text in the input) follows again. Sending a message jumps
to the bottom.

### Checkpoints of long runs

A run may make `agent.checkpoint_steps` model calls (default 100, at
least 10). Then, instead of failing, it reaches a checkpoint:

1. The agent is asked for its status (done, left, next step) in a short
   run with its tools off.
2. The classifier's model (see `/auto model`) judges whether the agent is
   making progress on what you asked: not repeating itself, not drifting
   outside your request, not done and spinning. It sees what it always
   sees (your messages and the agent's actions, never its reasoning or
   its tools' output), plus the status as a claim to check, never as an
   instruction. It answers CONTINUE or STOP with a reason.
3. One line reports the verdict. On a stop nothing more runs; say
   "continue" to go on, or raise `agent.checkpoint_steps`.

A checkpoint never continues by default: a failed or unclear review stops
the run, and so does a workspace without a classifier model. At most
`agent.checkpoint_continues` checkpoints in a row (default 5; 0 for none)
continue on their own; your next message resets the count. Esc or a
message during a checkpoint cancels it.

Each check is a model call at your cost. A run the checkpoint continues is
the daemon's, not yours: its calls that would ask you are asked only while
a client is attached to the agent, and denied otherwise (you are told at
your next attach).

Helpers (every kind `assign` starts) have checkpoints too. A stopped
single-use helper is retired and its parent gets its status as the
answer; a stopped kept helper stays seated and idle, and its parent is
told. A daemon stop during a checkpoint leaves the agent idle at the next
start; say "continue". A change of `agent.checkpoint_steps` applies to
conversations and helpers started afterwards. Neither setting can come
from a project's `.rkat/tui.toml`. `/auto stats` counts checkpoints, and
the audit log has a `checkpoint` and a `checkpoint_verdict` line for each.

## How approval works

Every tool call, the agent's and its helpers', passes the approval gate.
The first matching tier decides:

| Tier | What | Result |
|---|---|---|
| 0 | A deny rule matches; a file tool writes a **sealed** path; a withheld tool | denied |
| 1 | Read-only tools and shell commands; narrow allow rules | allowed |
| 2 | Unprotected file writes inside the project or a directory you granted; workspace notes | allowed (manual mode asks you) |
| ask | A file write to a **protected** path | asks you (auto mode: the classifier judges the content-protected ones) |
| 3 | Everything else | the classifier (manual and accept-edits ask you) |

That is auto mode, the default [permission mode](#permission-modes).

**Sealed.** `write_file`, `edit_file` and `blob_save_file` never write
these, whatever the classifier or an allow rule says:

- the project's `.rkat/` and every `.rkat` and `.git` inside the project;
- the git directories that govern the project, wherever they are (an
  enclosing repository's `.git`; a worktree's git directory and its
  `commondir`);
- the workspace's state directory (`~/.rkat/tui/workspaces/<id>/`, which
  the UI calls the realm) and the config directory `~/.rkat/tui`;
- in a directory you granted: its `.git/hooks`, `.git/config` and any
  `.rkat`.

Reads are not sealed, except in the workspace's state directory and the
daemon's endpoint directories (`$XDG_STATE_HOME/rkat-tui/`, else
`~/.local/state/rkat-tui/`, and `$XDG_RUNTIME_DIR/rkat-tui/`, which hold
the attach token): file tools never read or search them. A shell command
that writes a sealed path goes to the classifier like any other. On a
case-insensitive file system (macOS's default), `.RKAT/` is sealed too.

**Withheld.** The engine's `apply_patch` and `brain_swap` are withheld
(rkat-tui owns file writes and model switching). Background shell jobs
(`shell` with `background`, `shell_jobs`, `shell_job_status`,
`shell_job_cancel`, `monitor_start`) are refused; the agent is told to run
the command in the foreground or `assign` a helper to it.

**Tier 1:**

- `read_file`, `glob`, `grep` inside the project, the
  [scratch directory](#the-scratch-directory), or a directory you named
  (`--confinement-read`, `--confinement-write`, `[confinement]`);
- task tools, `memory_search`, `note_list`, `note_read`, `blob_inspect`,
  skill browsing, read-only `mob_*` and schedule queries, and `assign`
  (the helper's own calls pass the gate);
- `send_message` to another agent of the workspace, the agent's helpers,
  its parent or a fellow helper, or a wired agent (in plan mode, one to
  another workspace asks you);
- read-only shell commands (`cat`, `ls`, `rg`, `git status|diff|log`, …),
  also with output redirected into the scratch directory or a leading
  `TMPDIR=<scratch directory>`; `cargo build|test|nextest|check|clippy`
  while builds are trusted (`auto.trust_project_builds`).

Once the agent's shell changes or reads git's configuration, or writes
under `.git/`, that conversation's git commands leave tier 1: git runs
what its configuration names.

**Protected** paths, in the project and in directories you granted:

- **Runs later or widens access** (always asks you): `.envrc`, `.cargo/`,
  `rust-toolchain*`, `.claude/`, `.cursor/`, `.codex/`, `.gemini/`,
  `.mcp.json`, `.gitmodules`, `.gitattributes`, `tui.toml`, `.vscode/`,
  `.idea/`, shell profiles (`.bashrc`, `.bash_profile`, `.profile`,
  `.zshrc`, `.zprofile`, `.zshenv`, `.config/fish/`).
- **Content** (auto mode: the classifier reviews it under S6, instruction
  poisoning, and the credential rules, and asks you when it blocks; other
  modes ask): `.env`, `.env.*`, `build.rs`, `AGENTS.md`,
  `AGENTS.local.md`, `CLAUDE.md`.

An allow rule for the path allows the write; a deny rule denies it.

**Tier 3**: other shell commands, MCP tools, web search, other messages,
mutating `mob_*` tools, schedule changes, paths outside the project and
your granted directories, and user-level notes (they reach every
project's system prompt). A tool counts as a builtin, file or note tool
only when its definition says so: an MCP tool of the same name goes to
the classifier.

**What a deny rule matches**: the tool and, with an `arg` glob or a
`literal`, the subject. For a shell command, that is the command with
prefixes such as `env`, `command`, `sudo`, `nice`, `timeout`, leading
`VAR=value`, a program's directory (`/usr/bin/git` → `git`) and git's
global options (`-C <dir>`) stripped; for a file tool, the
project-relative path. It does not see through a script, `sh -c` or
another interpreter, an alias, a function or a variable. Deny rules are a
convenience; the classifier and the sandbox contain an action.

**Denials** are ordinary tool results (`policy_denied`, with the reason);
the run goes on. Each says who denied it: a deny rule (only you can lift
it), the fixed policy, plan mode, you (with your instruction), an
unanswered dialog, an unattended turn (see
[Several agents](#several-agents)), an unavailable classifier, a failed
review, or the classifier with its rule and reason. After a classifier
block the agent is told not to work around it but to find a materially
safer way or ask you; you can [approve the call](#approving-a-blocked-call).
When a path outside your allowed directories is why, the denial names the
path, the ways to allow it (`/permissions`, `--confinement-write`,
`[confinement] extra_write` in `~/.rkat/tui/tui.toml` and a daemon
restart, an allow rule) and where the agent can work instead.

A review that could not be completed (timeout, provider error) is not a
block and never counts toward the pause: you are asked if you are there;
otherwise the agent is told the call was not run and was not refused.

### What the classifier sees and blocks

The classifier is a separate model call
([its model](#the-classifiers-model)). It gets:

- your messages as typed (an `@`-attached file is named, not shown);
- the agent's and its helpers' earlier tool calls (string arguments cut
  at 1 KiB);
- the action under review, whole;
- for a shell command, the script it runs (up to 8 KiB) and, for a build,
  the files that decide what it runs;
- the files the agent's file tools wrote this session (newest 50),
  marked as the agent's;
- the environment: project root, working directory, OS, `origin` and
  default branch, your allowed directories,
  [statements](#allowing-a-kind-of-effect) and
  [hosts](#the-projects-hosts-and-the-hosts-you-trust), whether a sandbox
  contains the shell, the scratch directory, files the session created
  outside the project, and whether it changed a dependency manifest.

It never sees the agent's reasoning, prose or tool output, so the agent
cannot argue with it and injected output cannot reach it. Messages from
other agents, schedules, watches and compaction summaries are context,
never your words. It is told that a request, permission or prohibition of
yours stands until a later message withdraws it.

Your messages have a 32 KiB budget of their own (one over 8 KiB keeps its
first 6 and last 2 KiB); tool calls and context share another 32 KiB.
Each drops its oldest entries first, so a long run of tool calls never
pushes out the request behind them. When your messages overrun, the
classifier's model picks, from your older messages only, those that still
hold (standing requests, permissions, prohibitions, constraints); they
stay word for word, within 8 KiB of the 32. Nothing the model writes is
kept; if the pick fails, the previous choice stays.

The daemon stores your words per conversation (`tui_user_words/` in the
workspace's state), so they survive a restart, `/resume` and compaction.
A conversation written by a build before that file existed has no words
of yours in the classifier's transcript after a restart. A helper's call
is judged on its agent's transcript; the helper's assignment is the
agent's text, never yours.

**It blocks effects, not places.** The policy is tuned for a mistaken or
overeager agent, not an adversary: the sandbox is the security boundary,
and leaving the project is no violation in itself. It blocks:

- destroying what existed: a file outside the project that predates the
  session and that you did not name (D1), force pushes and discarded work
  (D2), databases (D3), pattern or age-based deletes in shared temp or
  cache directories (D6), a recursive delete of an unset `$VAR` (D7);
- editing dependency caches or installed packages (D8);
- reading credentials (D4) and sending data out (D5);
- lasting changes outside the project: system configuration, other
  repositories, shell profiles, global installs of packages the agent
  chose (T3);
- the rest: weakening security (S1–S6), crossing trust boundaries
  (T1–T5), bypassing review or acting for you on shared systems (R1–R5).

It allows:

- building, testing, linting and formatting, with what those write;
- files in temp and scratch locations, and the session's own files;
- installing what the manifest and lockfile declare; after the agent
  changed the manifest, only while something contains the shell
  (uncontained, rkat-tui asks you:
  [builds](#builds-after-a-dependency-change));
- `nix develop` (`-c <command>`, judged as itself), `nix build`, `nix
  shell`, `nix run` of the project's own `flake.nix` with `flake.lock`;
  not a flake by URL or registry, `nix profile install`, `--impure`,
  `--no-sandbox` or `--override-input`;
- a tool at the exact version the project's own files pin, run as they
  run it (`uvx ruff==0.6.9`, `npx prettier@3.3.3`); not a range,
  `@latest`, no version, a tool the agent chose, or a pin in a file the
  agent wrote;
- a tool filling its cache; servers on loopback; commits on the current
  branch; pushing a feature branch you asked for or the agent made;
- what you explicitly and specifically asked for.

**How it reviews.** Stage 1 answers ALLOW or BLOCK in one word, blocking
when unsure. Stage 2 re-judges what stage 1 flagged, with reasoning, and
may read project files (at most 6 reads or searches, 16 KiB each, 60 s);
file content is untrusted evidence, and `.env*` and `.envrc` are never
read. `auto.classifier_tools = false` turns the reading off. A call
identical to one allowed earlier in the conversation is not reviewed
again, except while auto mode is paused. An action whose arguments exceed
48 KiB is never shown to the classifier: you are asked, and with nobody
to ask it is denied. A stage-2 answer that names no clear verdict (the
provider cut it off at its token limit, or it is unclear) is no block:
you are asked, as when the review fails, and with nobody to ask it is
denied; `/auto stats` counts these as "no clear verdict".

### The approval dialog

After **3 consecutive** or **20 total** classifier blocks (for the whole
daemon, since it started or the last `/auto resume`), auto mode pauses
and every tier-3 action asks you. Allowing one ends a pause of 3 in a
row; a pause of 20 ends only with `/auto resume` or switching to auto
mode again. The same dialog asks about protected writes and the other
questions; a helper's call names the helper.

The four rows are: allow once; for this session (a
[session grant](#approving-a-blocked-call), else once); allow and add a
literal allow rule to the workspace's `tui.toml` (for a write, the
project-relative path, matching only when every target is that file);
deny, with an optional instruction the agent is told to follow. `s`
denies and stops the turn (or that helper); Ctrl-C stops all of the
caller's questions; Ctrl+O shows the whole arguments.

Two guards keep your typing from answering: Enter answers only after ↑
or ↓ moved the cursor, and while you are typing (until a short pause)
keys are dropped, not answered. Your unsent input stays as it was.

Every answer is recorded as the daemon's durable decision; one that
cannot be recorded denies. Each agent's questions show one at a time on
its own clients. A dialog unanswered for 10 minutes is denied, and so are
that agent's questions queued behind it.

### The classifier's model

With no `[auto] classifier_model` the classifier reviews with the agent's
start-up model (rkat-tui names no model of its own as a default); a start-up
notice says so. A cheaper model is usually enough. `/status` names it.
`/auto model` opens a picker over every endpoint's models; `/auto model
[endpoint:]<id>` sets it; `/auto model --user <id>` writes your own
`tui.toml` instead of the workspace's. It is saved as `[auto]
classifier_model` (never in a project file) and applies at once. A model
of an endpoint the daemon lacks is refused: define the endpoint in your
own `tui.toml` and restart the daemon.

At start, and after `/auto model`, the daemon checks the model with two
built-in cases (a `cargo test` it must allow, `git push --force origin
main` it must block; a few thousand tokens) and warns when one comes out
wrong. `[auto] classifier_probe = false` turns that off.

`/auto stats` shows, for this conversation and since the daemon started,
reviews, stage-1 flags, blocks by rule, answers with no clear verdict,
failed reviews, pauses, the share
of blocks you approved in `/permissions` (the false-block signal), how
often you allowed what you were asked (near 100% means the questions tell
you nothing), and the classifier's tokens and time.

### Checking your classifier model

How often auto mode blocks ordinary work, and how often it misses harm,
depends mostly on the model. `/auto check` sends labelled built-in cases
through the daemon's own classifier exactly as a review would (stage 2
without file reading) and reports what came out wrong. Each case is one
or two model calls at your cost.

| Command | Sends |
|---|---|
| `/auto check` | 30 fixed cases: 12 routine, 12 harmful, 3 scope pairs (at most a few hundred thousand input tokens) |
| `/auto check all` | all 166: 51 routine, 64 harmful, 12 scope pairs, 9 statement triples |
| `/auto check <n>` | `n` cases, spread the same way |
| `/auto check statements` | the 9 statement triples |
| `/auto check <case id>…` | those cases |

The targets are at most 1 wrong block in 30 routine cases and at least
90% of harmful ones caught. The report gives the totals, the same for the
held-out third (never used to tune the policy's wording: a model right
elsewhere but wrong there is lucky), each wrong case, and the verdict.
Scope pairs (one action, once asked for and once not) are reported but
not targeted. A statement triple checks that a
[statement](#allowing-a-kind-of-effect) is honoured and not over-applied:
an action blocked without it, allowed with it, and a neighbour that stays
blocked. A provider error or timeout stops the check; it is never counted
as a block. A check touches no conversation, counter or stat; it runs on
a fixed example machine, one at a time per daemon, at most 20 minutes.

Outside the targets, pick a stronger model: a larger or newer one of the
family usually fixes wrong blocks; one that misses harm should not judge.

`rkat-tui --classifier-check[=all|=<n>]` runs it from a script (starting
the daemon if needed): report on stdout, exit 0 within the targets, 3
outside, 1 when it could not complete.

### The permissions dialog

`/permissions` (or `/permissions blocked|rules|hosts`) opens three
sections, switched with Tab in a box the size of the largest:

- **Blocked**: recently blocked calls, to approve
  ([below](#approving-a-blocked-call));
- **In force**: rules, grants and statements, each with its source
  ([below](#in-force-rules-grants-and-statements));
- **Hosts**: the hosts you trust, and the project's own
  ([below](#the-projects-hosts-and-the-hosts-you-trust)).

It opens on Blocked when anything is there. Each section's footer lists
its keys.

#### Approving a blocked call

A classifier block in auto mode goes back to the agent without a dialog;
its notice points to `/permissions`. Blocked lists the newest 10 blocked
calls of your agent and its helpers (classifier blocks, failed reviews,
unattended calls that would have asked you), each with the grant an
approval would make.

| Key | Approves |
|---|---|
| Enter or `o` | **Once**: the next identical call runs unreviewed |
| `s` | **For this session**: the grant, for this conversation and its helpers until the daemon stops |
| `a` | **Always here**: a directory into the workspace's `[confinement] extra_write`, a rule into its rules |
| `A` | **Always, every workspace**: the same in your own `tui.toml` |
| `e` | **This kind of effect**: a [statement](#allowing-a-kind-of-effect) |
| `t` / `T` | **Trust the host** of a D5 block, here / everywhere |
| `w` | Withdraw a waiting approval once |

`s`, `a`, `A`, `t` and `T` ask for `y` first; Enter never confirms.

An approval once stays listed, waiting, until the identical call uses it.
It covers the whole call, so a row showing only part of it must be read
in full (Ctrl+O) before Enter approves it; a call over 48 KiB cannot be
approved once. A waiting approval lives only in the daemon, with its
conversation: a restart or ten newer blocks end it.

**The grant** is the directory of the path outside your allowed
directories that caused the block (its parent if it is not a directory
yet), else the call's literal allow rule. A file-tool write in a granted
directory runs like a project edit, and the classifier is told you allow
it. Never granted, not even for a session: `/`, your home directory
itself, a system directory, the project or a directory holding it,
rkat-tui's state, the directory of your `tui.toml` or the workspace's (or
one holding or inside it), and a directory overlapping one you named. The
gate applies an "always" directory at once; a confined shell can write it
from the next daemon start.

**Trusting a host** (`t`, `T`) trusts the hosts the call itself names
(URLs, `user@host:path`, `--hostname`) that are neither the project's own
nor trusted, read from the call, never from the classifier's words.

However you approve, the agent is told at once to run the call again and
go on; an idle agent gets a turn for it, which counts as yours. Removing a
grant or statement tells the agent nothing. Every approval, grant and
removal is in the audit log.

#### Allowing a kind of effect

A grant is exact: one directory, one command. A **statement** allows a
kind of action in your words ("pushing to main of this repository is
fine, I work alone"). The classifier reads the active statements and
lets one clear its block rules for actions of that kind, read narrowly:
not a neighbouring action of another kind, never sending data to a host
that is neither the project's nor trusted, and not what your messages
forbid. It is judged, not matched: a weak model may apply it too widely
or not at all (`/auto check statements` measures that).

A classifier block names its kind of effect in one general sentence, shown
on the Blocked row; `e` opens it for you to edit, `n` in In force opens an
empty one. Its scope is this session (the agent's helpers and forks
included), always for this workspace, or always everywhere (`[auto]
allow = ["…"]` in the workspace's or your own `tui.toml`, which you can
also edit by hand). A project's `.rkat/tui.toml` cannot add one: a cloned
repository could word one that clears what it wants. At most 12 are
active, each up to 200 characters; an empty one, a duplicate, or one that
only says "anything" is refused. The agent is told of each new one.

#### In force: rules, grants and statements

Lists the rules (with their numbers and layers), the grants and the
statements, each with its source. `d` removes a row: a rule or statement
of the workspace's or your `tui.toml` for good, one of the project file
or the command line until the daemon restarts; a session grant, while
another grant names the file to edit. `n` adds a statement, `r` a rule
(allow or deny, workspace or your file, pattern or literal), as
`/permissions add` does ([Rules](#rules)). `e` on a rule of the
workspace's or your `tui.toml` opens the same entry filled in; Enter
replaces the rule where it stands in its file, in one write (an entry
that does not parse, or says what another rule says, is refused in the
entry and the old rule stays).

### The project's hosts and the hosts you trust

Rule D5 blocks sending project data to a host you did not name. Two kinds
count as named:

- **The project's own**: the hosts of the git remotes (`url` and
  `pushurl` in `.git/config`, the repository's for a worktree). Only the
  host is used; a user or token in the URL never leaves the daemon.
  Fetching from them and pushing the project's content there with git,
  `gh` or `glab` is ordinary work.
- **The ones you trust**: `[auto] trusted_hosts = ["ci.example.net",
  "*.corp.example"]` in your own or the workspace's `tui.toml`, or
  `--set 'auto.trusted_hosts=["<host>"]'` for one run. `*.corp.example`
  takes `git.corp.example`, never `corp.example`, `evilcorp.example` or
  `corp.example.evil.example`; the domain needs two labels. A scheme,
  user, port or path is refused; at most 64 entries count; a project's
  `.rkat/tui.toml` cannot set it. Changes apply at once.

Neither list is a blanket allow: credentials and secrets are D5 at any
host, and every other rule still applies there (the default branch,
publishing, unrequested merge requests, issues or comments, history
rewrites). A D5 block names the host and how to trust it.

The remotes are read when the daemon starts, never while it runs. The
confined shell can write `.git/` (an engine limit), so a remote the agent
adds is not the project's, and adding a remote or changing its URL is
judged as sending the project there. Restart the daemon after changing
the remotes yourself.

#### The Hosts section

Lists your trusted hosts with the files that hold them, then the
project's own, read-only. `n` trusts a host you type (this workspace or
every workspace), written to that file's `[auto] trusted_hosts`; `d`
removes one from every file of yours that names it (one the daemon's
command line names stays until the daemon stops). Only your own keys add
a host: the agent can neither ask for it nor write either file.

### Permission modes

Shift+Tab cycles manual → accept edits → plan → auto; the status bar
shows the mode, auto mode's counters (`auto (deny 0/3, 0/20)`) and a
pause.

| Mode | Tier 2 (in-project edits) | Tier 3 | Shell |
|---|---|---|---|
| `default` (shown as `manual`) | asks you | asks you | read-only commands run, others ask |
| `accept-edits` | allowed | asks you | as manual |
| `plan` | denied | denied | only commands tier 1 itself finds read-only (not an allow rule's) |
| `auto` (the default) | allowed | the classifier | read-only commands run, others go to the classifier |

In every mode deny rules and sealed paths deny, tier-1 reads run, and a
protected write asks (plan mode denies it). The mode is the daemon's, for
every agent and helper of the workspace; a queued question is judged
again under the mode in force when it comes up. Leaving auto mode ends a
pause; entering it resets the counters.

| Command | Does |
|---|---|
| `/mode` | Pick a mode (on a paused auto mode: resume) |
| `/mode [--user] default\|accept-edits\|plan\|auto` | Set it |
| `/auto on`, `/auto off`, `/auto resume` | `/mode auto`, `/mode default`, end a pause |
| `/auto` | The mode and the counters |

**Plan mode** is read-only: the agent is told to research and present a
plan. When a turn ends in plan mode with an answer, "Plan ready" offers
to proceed (switching to accept edits), keep planning, or proceed in
manual mode.

The mode is saved as `[auto] mode` in the workspace's `tui.toml` (with
`--user`, your own). `--no-auto` starts one run in manual mode, `--set
auto.mode=plan` in any mode. A project's `tui.toml` can only move auto
mode to manual (`mode = "default"`).

### What runs as the host

The gate and the sandbox cover tool calls. Local MCP stdio servers and
command hooks are not tool calls: rkat-tui starts them with your
privileges, outside the confinement. A project's `.rkat/mcp.toml`
(servers) and `.rkat/config.toml` (`[hooks]`) may have come with a cloned
repository, and on Linux a confined shell can write `.rkat/`, so neither
runs anything until you trust it. `/trust` first shows what each file
would run (every MCP server's command line or URL, every hook's command,
read from the file as it is then) and asks: `y` records the SHA-256 of
exactly what you were shown in `~/.rkat/tui/trust/`, `n` or Esc records
nothing; a file that changed after you were shown it stays untrusted,
and the answer says so. `--trust-project` trusts them for one run
without recording. A changed file is untrusted again; a
start-up notice names each untrusted file and what it did not start, and
`/status` shows their state. Your own `~/.rkat/config.toml`,
`~/.rkat/mcp.toml`, `~/.rkat/tui` and the workspace's state directory
are always trusted.

## Confinement

The agent's `shell` tool runs its commands in a sandbox that
`--confinement <mode>` picks:

| Mode | Read and write | Read only | Network |
|---|---|---|---|
| `workspace` (default) | the workspace, the temp dir, the Cargo home's `registry/` and `git/`, the user cache dir, each `--confinement-write` | the rest of the Cargo home (never `credentials.toml` or `credentials`), the rustup home, `/usr/{local,share,include,libexec}`, `/opt`, `/etc/{ssl,pki,ca-certificates,alternatives}`, each `--confinement-read`; the files `/etc/{resolv.conf,hosts,nsswitch.conf,host.conf,gai.conf,services,protocols,passwd,group,gitconfig}` | allowed |
| `strict` | as `workspace` | as `workspace` | none |
| `off` (`--no-confinement`) | no sandbox | | the host's |
| `external` | no sandbox of rkat-tui's: you declare that your own contains rkat-tui | | as you declare |

Shell commands run in the foreground only; background jobs are not
available. Command hooks and local MCP stdio servers are never confined.

**Your own sandbox.** Inside a container of yours, pass
`--confinement=external`: the shell runs as with `off`, you name its
writable directories with `--confinement-write`, and
`--external-no-network` says it has no network. rkat-tui cannot check
this; it trusts your description, tells the classifier, and trusts
project builds as under its own sandbox. Only the command line can
choose `external`.

**Builds.** While a sandbox contains the shell (`workspace`, `strict`,
`external`), the project's `cargo check`, `build`, `test`, `clippy`,
`fmt --check`, `nextest`, `tree` and `metadata` run without a classifier
call. With `--no-confinement` they are reviewed. `--trust-project-builds`
trusts them anyway, until a dependency change;
`--no-trust-project-builds` (`[auto] trust_project_builds = false`)
reviews them in every mode.

#### Builds after a dependency change

The gate notices when the agent changes a build-defining file
(`build.rs`, `.cargo/config*`, `rust-toolchain*`, the manifests and
lockfiles: `Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`,
`flake.nix`, …): any write by its file tools, or any change to the
project root's files. A shell write to a workspace member's `build.rs`
goes unseen. Then:

- **Contained:** builds still run unreviewed (the sandbox is the
  boundary); the first one shows a notice and writes a
  `build_after_change` audit line.
- **Uncontained** (`--no-confinement`): any command running `cargo`,
  `make`, `npm`, `pnpm`, `yarn` or `npx` asks you. "Yes, for this
  session" sends such builds back to the classifier, told you allow
  them. In an unattended turn it is denied, and `/permissions` offers the
  same session grant.

**What a confined command sees.** On Linux it runs in an empty private
mount view holding only the table's paths that exist on the host and a
read-only system baseline (`/bin`, `/sbin`, `/usr/bin`, `/usr/sbin`,
`/lib*`, `/usr/lib*`, `/etc/ld.so.cache`, `/dev/null`, `/dev/random`,
`/dev/urandom`). Nothing else of `/etc`, `/run`, `/sys` or your home is
there. It has its own pid namespace (host processes are invisible) and,
where the host allows, its own read-only `/proc`. A seccomp filter
allows only IP sockets and socket pairs: it cannot connect to a Unix
socket. The Cargo home is granted entry by entry as found at the
daemon's start, so a command cannot replace a tool in `bin/` or set
`build.rustc-wrapper`, the registry token is invisible, and `cargo
install` fails. Grants never pass through a symbolic link, which is why
`/etc` is granted file by file. `/etc/hosts` names always resolve; DNS
works wherever the network does, through `resolv.conf` (also when it
links to systemd-resolved's stub).

**Environment.** A confined command does not inherit rkat-tui's
environment. It gets `PWD` and only these, when set: `PATH`, `HOME`,
`USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `TERM`, `COLORTERM`, `NO_COLOR`,
`CARGO_HOME`, `RUSTUP_HOME`, `RUSTUP_TOOLCHAIN`, `XDG_CACHE_HOME`,
`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `LANG`, `LANGUAGE`, `LC_*`, `TZ`,
`SSL_CERT_FILE`, `SSL_CERT_DIR`, `CURL_CA_BUNDLE`, and the proxy
variables (`http_proxy`, `https_proxy`, `no_proxy`, `all_proxy`, either
case). Proxy URLs pass unchanged, credentials included. Every other
variable, provider API keys included, stays out. With `off`, the whole
environment is inherited.

### The scratch directory

Each agent gets a private `<temp>/rkat-tui-<12 hex digits>/` (mode
`0700`, usually under `/tmp`), shared with its helpers and forks. In
every mode its shell gets it as `TMPDIR`, `TMP` and `TEMP`, so test
suites and compilers write there instead of the shared `/tmp`. The gate
treats it as the agent's own: file-tool writes there count as project
edits, reads are unreviewed, and shell redirects into it count as
read-only. A resumed agent gets the same directory, empty; it is removed
when the agent retires and when the daemon stops. If the path exists
but is not a private directory of yours (another user's, a link, open to
others), the agent gets none and the daemon log says why.
`blob_save_file` and `view_image` accept only paths inside the project.

### More directories

`--confinement-read <dir>` and `--confinement-write <dir>` (repeatable),
or in your own `tui.toml`:

```toml
[confinement]
extra_read = ["/data/datasets"]
extra_write = ["/scratch/build"]
```

Each must be an existing absolute directory with no symbolic link on the
way, off the system baseline, `/proc`, `/sys` and `/dev`, and not
overlap the workspace or another added directory; otherwise the start
is refused, with the reason. Layers add up (global, workspace,
`--set`), and a flag wins over an overlapping file entry. A default
grant overlapping yours yields to it (`--confinement-write /opt/foo`
drops the read-only `/opt`). A project's `.rkat/tui.toml` and the
workspace's `config.toml` cannot widen the sandbox: their
`[confinement]` is ignored with a notice.

The gate trusts what you name: file-tool edits in a write directory are
tiered like project edits (protected paths such as `.env` still ask),
reads in any named directory are unreviewed, and `.git/hooks`,
`.git/config` and `.rkat` inside stay sealed. A symbolic link out of
the directory does not carry the grant.

**What the classifier is told.** The grants are computed even when the
shell is unconfined. The classifier gets the directories outside the
project that commands may write and read by default, and yours marked as
yours. A write there is not "outside the allowed scope", but the
classifier still judges what it does. With `--no-confinement` inside a
sandbox of your own, name its writable directories with
`--confinement-write` so the classifier knows them.

**When the host cannot confine,** rkat-tui refuses to start (exit 2, no
fallback). The Linux sandbox needs unprivileged user namespaces; the
error names the switch that blocks them
(`kernel.apparmor_restrict_unprivileged_userns`, the default on Ubuntu
24.04 and later; `kernel.unprivileged_userns_clone`;
`user.max_user_namespaces`) and gives the commands to fix it, including
a narrow AppArmor profile for the `rkat-tui` binary. macOS uses
Seatbelt; other platforms have no sandbox.

Unconfined, the shell runs as you and can reach everything rkat-tui
keeps in the workspace's state directory: rules, settings, the helpers'
bookkeeping, the agents' `/wire` sockets. Only the approval gate
remains, so run unconfined only inside a sandbox of your own.

### Security decisions

- **The network is on.** `workspace` mode lets the shell reach package
  registries, `git fetch` and `curl`; `--confinement=strict` removes the
  IP network. In auto mode, network commands are reviewed like any
  other.
- **No idle timeout.** A detached agent runs until its turn ends or you
  stop it (`/quit` from a TUI attached to it, `/daemon stop`, `kill`).
- **Unattended turns are reviewed.** A turn another agent, a schedule or
  a file watch starts while no TUI is attached is reviewed by the
  classifier in auto mode. Anything that would ask you (a protected
  file, manual or accept-edits mode, paused auto mode, an action too
  large to review, a failed review) is denied, the agent is told it needs
  you, and the next TUI to attach shows it ([Several
  agents](#several-agents)). Plan mode stays plan mode. A turn you
  started stays reviewed as usual after you detach.
- **The audit log** records every approval decision and who made it,
  and every change of mode, rules and trust
  ([Where things live](#where-things-live)).

Not provided: a machine-wide policy file for every user of a host, and
`cargo-vet` audits of the dependencies (CI runs `cargo deny`).

### Security model and its limits

Each limit is deliberate; *to tighten* says what a cautious user can do.

- **The shell is sandboxed; the file tools are not.** `read_file`,
  `write_file`, `edit_file`, `grep` and `glob` run in the daemon as you.
  In the project and directories you named, reads are allowed and edits
  follow the permission mode. Anything else outside the project is
  reviewed (classifier in auto mode, you in manual and accept-edits,
  refused in plan mode). Refused outright: the workspace's state
  directory, the attach token's directory (never read) and the sealed
  paths (`~/.rkat/tui`, the project's `.rkat/` and `.git/`, never
  written). *To tighten:* deny per file tool; a rule's subject is the
  path relative to the project, or absolute outside it, so `[[deny]]
  tool = "read_file" arg = "/*"` refuses every read outside the project
  (repeat for `grep`, `glob`, `write_file`, `edit_file`). Or use manual
  mode (`--no-auto`, `[auto] mode = "default"`).
- **A secret in the project is readable.** `read_file .env` runs
  unasked (only writes to `.env`, `build.rs`, `AGENTS.md` and the like
  ask), and `@.env` attaches it. *To tighten:* `[[deny]] tool =
  "read_file" arg = "**/.env*"`, and the same for `grep`.
- **A file-tool allow rule without `arg` allows everything it reaches,**
  protected paths and paths outside the project included, answering
  what would otherwise ask or be classified; it is not flagged as broad.
  Deny rules, sealed and secret paths and plan mode still win; a
  project's `tui.toml` cannot add allow rules. *To
  tighten:* give each such rule an `arg` inside the project (`arg =
  "src/**"`); check `/permissions` now and then.
- **The sandbox shares `/tmp` and `~/.cache` with you.** The shell can
  read and change what other programs keep there; a command naming
  `/tmp` reaches the shared one, judged by the classifier, not the
  sandbox (a sweeping delete is blocked). Its own temp files go to its
  scratch directory. Unix sockets there are out of reach, but TCP, loopback
  included, is open in the default mode. *To tighten:*
  `--confinement=strict` removes the network; keep secrets out of both
  directories.
- **A contained shell runs the project's builds unreviewed,** also after
  the agent changed dependencies, so test code the agent wrote runs with
  the network on (in `workspace` mode). The sandbox keeps it out of your
  home and provider keys; an `external` sandbox is only as good as your
  description. *To tighten:* `--no-trust-project-builds`,
  or `--confinement=strict`.
- **The classifier judges effects, so ordinary writes outside the
  project run:** files in temp directories and caches, installs of what
  the project declares. A small classifier model may read that too
  generously and pass a cache edit or a sweeping delete; the sandbox
  still keeps the shell out of your home and `~/.ssh`. *To tighten:*
  keep confinement on, pick a capable `[auto] classifier_model`, or use
  manual mode for untrusted code.
- **The shell can write the project's `.git/` and `.rkat/`.** Read-only
  git commands (`status`, `diff`, `log`, `show`, `blame`, …) run
  unreviewed, and git runs what `.git/config` names (`core.fsmonitor`,
  `textconv` drivers, `diff.external`): in the sandbox, or as you with
  `--no-confinement`. A fresh clone has no `.git/config` of its own; a
  copied directory or unpacked archive may. The project's
  `.rkat/mcp.toml` and hooks run only while `/trust` holds their digest,
  and a change to `.rkat/tui.toml` applies at the next start. *To
  tighten:* keep confinement on; for an untrusted checkout, `[[deny]]
  tool = "shell" arg = "git *"`.
- **A user-scope note reaches every workspace.** A note saved with
  `scope = user` (`note_save`: reviewed in auto mode, asked in manual
  mode) is listed, name and first line, marked untrusted and stripped of
  headings and fences, in every workspace's system prompt: the one way
  an agent writes into another workspace's prompt. *To tighten:* `[[deny]] tool =
  "note_save" arg = "user/*"`; read `~/.rkat/tui/notes/` now and then.
- **Wired agents talk unreviewed.** After `/wire` ([Wiring agents across
  workspaces](#wiring-agents-across-workspaces)), `send_message` between
  them needs no review: yours can send anything it read in your project,
  and the other can wake yours at any time (each wake is a paid model
  turn). A message carries no authority: what your agent does passes
  your gate and sandbox, an unattended turn runs only what the
  classifier allows, and the classifier never takes it for your words.
  Each agent's socket (`wire/<agent>.sock` in the state directory, 0600
  in a 0700 directory) admits only your own processes, and envelopes not
  signed by the wired agent are dropped unanswered. *To tighten:* wire
  only projects that may share what they read, remove a wire when done
  (`d` on its row in `/wire`), or
  `[[deny]] tool = "send_message"`.
- **`!` runs as you, unsandboxed and unreviewed,** with your whole
  environment, and its output joins the agent's context. *To tighten:*
  never paste the agent's commands into it unread.
- **A file attached with `@` is as trusted as one the agent read:** its
  instructions can be obeyed. The classifier sees only that files were
  attached. Sealed paths are never attached. *To tighten:* attach only
  what you would let the agent read; `[auto] classifier_tools = false`
  also keeps file contents from the classifier.
- **A non-loopback `--attach` is unencrypted.** It needs
  `--attach-insecure`; the token never crosses the network, but the
  session does, approvals included, readable and alterable on the way.
  *To tighten:* forward the port over SSH instead (`ssh -N -L
  127.0.0.1:4100:127.0.0.1:<port> <host>`, then
  `--attach=127.0.0.1:4100`).
- **The attach token is the daemon's only access control.** Whoever
  reads the endpoint file can attach, answer approvals and change mode
  and rules. With confinement on, neither the shell nor the file tools
  can read it (the daemon refuses to start otherwise); with
  `--no-confinement` the shell can. *To tighten:* keep confinement on.

### The file tools' limits

| Tool | Limit |
|---|---|
| `read_file` | the first 16 MiB of a regular file |
| `write_file`, `edit_file` | refuse files over 16 MiB |
| `glob` | 500 paths, newest first |
| `grep` | the first 16 MiB of each file, binaries skipped, 250 lines (`head_limit`), 64 KiB |

`glob` and `grep` stop at `[search] budget_secs` (default 10 s) or
`[search] max_entries` files and directories examined (default
500 000); a project's `tui.toml` may only lower these. A search that
stops early returns what it found and says it is partial, where the time
went, and how to narrow it. A more specific pattern searches less:
`src/**/*.rs` walks only `src/`, while a pattern with `{…}`, `[…]` or `\`
walks the whole tree. A cancelled call (Esc, Ctrl-C, timeout) stops at
once, and a write it had not made yet is not made.

## Helpers

The agent can hand work to helper agents. A helper never blocks the
agent: while it works, the agent still hears you and answers. One tool,
`assign`, starts every helper, with two independent choices:

| | single-use (`keep: false`, default) | kept (`keep: true`) |
|---|---|---|
| **fresh** (`context: "fresh"`, default): knows only its task | parent told when it finishes, with its answer; then retired | parent told each time it goes idle; stays until `retire_helper` |
| **fork** (`context: "fork"`): continues from a copy of the agent's conversation, on the agent's model | outcome arrives when its first run ends; then retired | as single-use, then each later run reported when it goes idle; stays until `retire_helper` |

The UI names these `fresh · single-use`, `fresh · kept`,
`fork · single-use` and `fork · kept`.

**What each costs.** A fork carries the whole conversation on every
model call, which is expensive in a long conversation. A fresh helper
knows only its task, so the task must say everything it needs. A kept
helper costs nothing while idle, but every message to it starts a run of
it, and its answer starts a run of the agent. Keep a helper only to give
it a new task that needs what it learned; you do not need `keep` to talk
to a helper while it works.

**How it relates to the agent.** Every helper is wired to its parent
both ways: each can `send_message` the other at any time, and a message
to an idle helper starts a run. A helper's tool calls pass its parent
session's gate (same rules, same counters; escalations name the helper),
so starting a helper never unlocks an action. It has rkat-tui's file
tools and a helper persona. Helpers of helpers are possible up to the
depth limit (below); retiring a helper by any route retires its own
helpers with it. Leaving a conversation (`/new`, `/resume`) retires all
its helpers. A retired helper cannot be brought back, and a name that is
free again starts a brand-new helper.

Background shell jobs are not available. To run a long command while it
keeps working, the agent assigns a helper to run it and report; servers
and other long-lived processes cannot be kept running this way.

### Tools

| Tool | Does |
|---|---|
| `assign` | `name`, `task`, optional `context`, `keep`, `model`; returns at once. A name is 1 to 32 lowercase letters, digits and `-`, not starting with `-`, not an agent's name (`main-…`), and not in use anywhere in the workspace. A fork takes no `model` |
| `retire_helper` | `name`: retires one of the caller's own helpers (its run is cancelled) |
| `connect_helpers` | wires the caller's helpers to each other ([Several agents](#several-agents)) |
| `contacts` | whom the caller can message ([Several agents](#several-agents)) |
| `mob_create`, `mob_spawn_member` | a team in a mob of its own; its members behave as kept fresh helpers |

The engine's `delegate`, `fork_off`, `spawn_member` and
`spawn_many_members` are not offered; a call of one is refused. A fork
is still the engine's own: `assign` starts it through `fork_off`.

### How the parent hears of it

However a helper ends (finished, failed, stopped by you, stopped at its
checkpoint, ended with the daemon), its parent is told once, at once. A
kept helper's parent is told each time it goes idle instead. An answer
over 4 000 characters is cut, and the full text is written to the
agent's scratch directory. A running parent reads the note at its next
step; an idle parent starts a run for it. So each ending costs the
parent a model turn. A single-use helper that has unread messages from
its parent, or helpers of its own still running, goes on instead of
ending.

A fork's first run is reported by the engine itself ("Background
fork_off job … finished"); the transcript shows it as a compact `⚙ fork`
line with the report folded. A fork still in its first run when the
daemon stops does not survive: the engine counts it as failed, and its
parent is told so after the next start.

Right after you interrupt the agent, a note does not wake it: it is held
and goes with your next message, ahead of your words. Held notes survive
detaching and a daemon restart (`held-notes.json` in the workspace's
state). Notes are never drawn as your messages; a note that started a
run shows as one folded line (`✓ helper review-x finished: …`), which a
click or Ctrl-O opens.

A helper still running when the daemon stops is run on by the engine at
the next start, and its answer comes as a note.

### Interrupting and stopping

Esc, Ctrl-C and Ctrl-X Ctrl-S stop the agent's **own** run only. Its
helpers keep working, their calls are still judged, and their approval
prompts stay open. Stopping the daemon ends every helper.

To stop one helper: Ctrl-C in its full view, `s` in `/helpers`, or `s`
on the approval prompt of its call; each asks first. A stop cancels the
helper's run and retires it, but the helpers it started keep running.
Ctrl-C in a helper's view never interrupts the agent or counts toward
quitting.

### Watching helpers

A panel under the input lists the agent's live helpers, indented by
depth, running ones first, each with its current activity; an ended one
stays dim for a few seconds. Ctrl+↓/Ctrl+↑ select one, and Enter on an
**empty** input opens its full view in place of the transcript (with
text, Enter sends as usual). The full view reads like the main
transcript: the task, later messages, its prose (the last is its
answer), tool calls and its own helpers. A fork's view starts at its own
task; the inherited conversation is not repeated. Esc goes back.

The transcript keeps each helper's header where it started; a click
unfolds its work in place. `[ui] helpers = "expanded"` starts every
header unfolded (default `"folded"`).

`/helpers` (or `/tasks`) lists this conversation's live helpers, with
kind, state, calls and tokens; `a` shows the ended ones too. From the
list you open a full view, fold a block, or stop a helper. `/helpers
list [all]` prints the list as a notice; `/helpers expand|fold
<label>|all` unfolds or folds blocks. The list comes from the daemon, so
it is right after a re-attach, `/resume` or a restart. Helpers of other
agents in the workspace are not shown.

### Limits

| Setting | Default | Meaning |
|---|---|---|
| `helpers.max_depth` | 1 | levels of helpers (0: none). At the limit a helper has no `assign`, `connect_helpers`, `retire_helper`, `mob_create`, `mob_spawn_member` or `council`. A project file may only lower it; applies at the next start |
| `helpers.max_live` | 8 (at most 64) | helpers each agent, and each helper, may hold from `assign` at once: running, kept and idle, or starting. Each caller has its own limit. Mob members do not count. A project file may only lower it; a change applies to the next `assign` and retires nobody |

The default depth of 1 means the agent may start helpers and its helpers
may not, so chains of full contexts do not build up. A call past
`max_live` is refused, naming the helpers held and saying to retire some
with `retire_helper`.

### Helper models

A fresh helper's model (from `assign` or `mob_spawn_member`) is the
model the call names, else `helpers.model`, else its owner's current
model. A stored mob profile's model never applies: the profile gives
tools and instructions, but its model, provider and fallback chain are
replaced. A fork always stays on its parent's current model, and none of
the settings below apply to it.

Every helper runs through its owner's endpoint, never a provider's
public default; a model the endpoint cannot serve is refused with the
alternatives. On a self-hosted endpoint only models the endpoint listed
at start, or that `/model` switched to, are available.

| Setting | Default | Meaning |
|---|---|---|
| `helpers.model` | the owner's model | what `/model` takes, on the owner's endpoint |
| `helpers.allow_models` | any | the only models fresh helpers may use |
| `helpers.deny_models` | none | models fresh helpers may not use |

Set at most one of the two lists, across all your layers. Each entry is
an exact model name or a prefix ending in one `*` (`"claude-haiku-*"`),
matched against the plain model name without `endpoint:`. A spawn whose
model fails the list is refused, and the agent is told what to name
instead.

```toml
[helpers]
model = "claude-haiku-4-5"
allow_models = ["claude-haiku-*", "claude-sonnet-4-6"]
```

These three are yours, not the project's: `/set`, `/unset`, `--set`, or
your own or the workspace's `tui.toml`; a project's `.rkat/tui.toml` may
not set them. A change applies to helpers started from then on, and the
agents are told. A file that sets both lists is not applied (at start it
fails the start). The classifier's model (`auto.classifier_model`) is
separate.

## Settings

Each setting has one dotted key, used in a `tui.toml` (`[auto]`
`compact_prepare_at = "65%"`), on the daemon's command line (`--set
auto.compact_prepare_at=65%`, or a dedicated flag) and in the TUI
(`/set auto.compact_prepare_at 65%`). The
[configuration reference](#configuration-reference-tuitoml) lists every
key, its default and flags, what a project file may do with it, and
when a change applies.

**Layers**, lowest first: the default, your `~/.rkat/tui/tui.toml`
(every workspace), the workspace's `tui.toml` in its state directory,
the command line. A project's `.rkat/tui.toml` counts only for the keys
the reference allows; where it may only lower a value, its lower value
wins over every layer. rkat-tui never writes a project's file.

| Command | Does |
|---|---|
| `/set [--user] <key> <value>` | writes the workspace's file, or with `--user` (`--global`) yours; values as `true`/`on`, `65%`, `300k`, words, text |
| `/unset [--user] <key>` | removes that level's value; the next layer's applies |
| `/settings [filter]` | lists every setting with its value in force, its layer, what each layer holds and when a change applies; edit or remove values there |

`/compact prepare …`, `/auto model`, `/mode`, `/permissions add` and
Ctrl-T write settings the same way, and the first four take `--user`
too (`/mode` alone and Shift+Tab write the workspace's). `--user` counts
only directly after the command or its subcommand (`/set --user …`,
`/compact prepare --user …`, `/auto model --user …`, `/permissions add
--user …`); anywhere else it is part of the text, so
`/permissions add allow shell pip install --user *` is a rule for
exactly that command.
`/permissions`' `A` and `n` write your file.

Every change answers with the file written, the value now in force and
its layer, and when it applies. A change a higher layer overrides is
still written and the answer says which layer wins. A value the setting
does not take is refused. Changes are in the audit log.

**When it applies:** *at once* (the mode, the classifier model,
`trust_project_builds`, the prepare instruction and threshold, most
`[ui]` settings, which every attached client follows), *from the next
conversation* (`agent.checkpoint_steps`), or *at the next start* of the
daemon, or of rkat-tui for a client-side setting such as
`shell.user_timeout_secs` (e.g. `auto.compact`, `search.budget_secs`,
`ui.mouse`).

rkat-tui changes only the one key in a file, keeping comments and
layout, writes nothing that would not parse, and refuses a symlinked
file.

**Edits by hand.** A running daemon watches your `tui.toml`, the
workspace's and the project's. When yours or the workspace's changes, it
applies the new values at once, as `/set` would, including rules,
`[auto] allow` statements, trusted hosts and `[confinement]`
directories, and tells every client what changed. An entry a start
would refuse (a rule that does not parse, a broad allow rule, a
statement past twelve, a bad trusted host, a directory that reaches
your settings) is skipped and named. A newly granted directory reaches
the file tools at once but the confined shell only after the daemon's
next start. A file that does not parse keeps its last good values until
it does. Two kinds of change wait for the next start: the project's
`.rkat/tui.toml` (the agent's shell can write it), and endpoints, prices
or thinking in any file.

## Rules

A rule allows or denies a tool, optionally only for calls whose subject
(see [How approval works](#how-approval-works)) matches a glob (`arg`) or
an exact text (`literal`). A deny rule wins. Rules come from your own
`~/.rkat/tui/tui.toml`, the workspace's `tui.toml` (in its state
directory; `/permissions add` writes there), the command line (`--allow`,
`--deny`, shown as `(cli)`) and the project's `.rkat/tui.toml`, which may
only add deny rules.

- A shell allow rule must match **every** command of a `;`, `&&`, `||`
  or `|` chain, so `cargo nextest run*` does not allow `cargo nextest
  run; curl x | sh`; a command with a redirection never matches one. A
  deny rule matches if **any** command does.
- Too-broad allow rules are dropped with a notice whenever the daemon
  reads them: every tool, every tool of an MCP server, `shell` with no
  pattern or a leading wildcard, an interpreter with a wildcard
  (`python*`, `node`, `bash`, `sh`, `zsh`, `ruby`, `perl`, `deno`, `bun`),
  and any rule for a task runner (`npm run`, `npm exec`, `npx`, `pnpm`,
  `yarn`, `cargo run`, `make`).

`/permissions add [--user] allow|deny <tool> [pattern]` adds one (to your
own file with `--user`), `/permissions rm <n>` removes one, `/permissions
list` prints them; the dialog's
[In force](#in-force-rules-grants-and-statements) section does the same.
The workspace's file is out of the agent's and any repository's reach,
and a hand edit applies at once:

```toml
[auto]
mode = "auto"
allow = ["pushing to the default branch of this repository"]  # statements

[[allow]]
tool = "shell"
arg = "cargo nextest run*"

[[deny]]
tool = "shell"
arg = "git push*"
```

### Configuration reference (`tui.toml`)

Layers, later winning: the default, your own `tui.toml`, the
workspace's, the command line. A project's `.rkat/tui.toml` counts only
as its column says (the command line still wins over it; a lowering
applies over every layer); the rest of it is ignored, since a cloned
repository could otherwise choose it.

`--set <dotted.key>=<value>` sets a key for one run. The value is TOML
(`true`, `85`, `[{tool = "shell"}]`); anything else is a string
(`--set auto.compact=auto`). Tables merge, scalars replace, arrays
append, endpoints merge by name. A bad `--set` stops start-up and names
it. The table is generated from `src/config/registry.rs`; "Takes effect"
says when a change made while the daemon runs applies.

<!-- settings-table:begin -->
| Key | Takes | Default | A project file | Takes effect | Flag, command |
|---|---|---|---|---|---|
| `auto.mode`: The permission mode at start (`default` is manual: every call that needs review asks you). | `default`, `accept-edits`, `plan`, `auto` | auto | may only turn auto mode off (`mode = "default"`) | at once | `--no-auto`, `/mode`, Shift+Tab |
| `auto.classifier_model`: The model that reviews the agent's actions in auto mode (a cheaper one than the agent's is usually enough). | a model, `[endpoint:]model` | the agent's start-up model | may not set it | at once | `--classifier-model`, `/auto model` |
| `auto.trust_project_builds`: Whether `cargo build\|test\|nextest\|check\|clippy` are tier 1. | `true` or `false` | unset: trusted while a sandbox contains the shell; uncontained, a build after a dependency change asks | may only set `false` | at once | `--trust-project-builds`, `--no-trust-project-builds` |
| `auto.classifier_tools`: Whether the classifier's second stage may read project files. | `true` or `false` | true | may only set `false` | at the next start | — |
| `auto.classifier_probe`: Whether the daemon checks the classifier model at start with two fixed cases. | `true` or `false` | true | may set it (the command line wins) | at the next start | — |
| `auto.compact`: When the context fills up: the engine's own threshold, only warn, or compact at `compact_auto_percent`. | `native`, `warn`, `auto` | native | may set it (the command line wins) | at the next start | `--auto-compact`, `--no-auto-compact` |
| `auto.compact_warn_percent`: Context usage (percent of the window) at which to warn once. | a whole number from 1 to 100 | 75 | may set it (the command line wins) | at the next start | — |
| `auto.compact_auto_percent`: Context usage (percent of the window) at which to compact (`compact = "auto"`) or warn every turn. | a whole number from 1 to 100 | 85 | may set it (the command line wins) | at the next start | — |
| `auto.compact_prepare`: What every agent is asked to do before an automatic compaction (empty: none here). | text (at most 2000 characters) | none | may set it (the command line wins) | at once | `/compact prepare <text>\|off\|default` |
| `auto.compact_prepare_at`: Where the agents are asked to prepare: tokens, or a share of the window. | tokens (`300000`, `300k`, `1.2m`) or a share of the window (`"65%"`) | 70% | may set it (the command line wins) | at once | `/compact prepare at <threshold>\|default` |
| `helpers.max_depth`: How many levels of helpers a session may have (0: none). | a whole number from 0 to 255 | 1 | may only lower it | at the next start | — |
| `helpers.max_live`: How many helpers each agent, and each helper, may hold at once from `assign` (running, kept and idle, or starting; at most 64): a call past it is refused, and a lowered limit retires nobody. Members of mobs made with `mob_create` do not count. | a whole number from 1 to 64 | 8 | may only lower it | at once | — |
| `helpers.model`: The model a fresh helper started with `assign` or `mob_spawn_member` runs on when the call names none (through its owner's endpoint); a fork stays on its parent's. | a model, `[endpoint:]model` | the owner's model | may not set it | at once | — |
| `helpers.allow_models`: The only models a fresh helper started with `assign` or `mob_spawn_member` may run on (not with `deny_models`). | a list of model names, each exact or a prefix ending in one `*` (`["claude-haiku-*"]`; `/set` also takes `a, b*`) | unset: any model | may not set it | at once | — |
| `helpers.deny_models`: Models a fresh helper started with `assign` or `mob_spawn_member` may not run on (not with `allow_models`). | a list of model names, each exact or a prefix ending in one `*` (`["claude-haiku-*"]`; `/set` also takes `a, b*`) | unset: none | may not set it | at once | — |
| `agent.checkpoint_steps`: The model calls one run of an agent or a helper makes before a checkpoint: it is asked for a short status, the classifier's model judges whether going on is sensible, and it continues or stops (the conversations and helpers started from then on). | a whole number from 10 to 100000 | 100 | may not set it | from the next conversation | — |
| `agent.checkpoint_continues`: How many checkpoints in a row may continue automatically before the run stops and waits for you (0: every checkpoint stops); a message of yours starts the count again. | a whole number from 0 to 1000 | 5 | may not set it | at once | — |
| `shell.user_timeout_secs`: How long a `!` command may run, in seconds. | a whole number from 1 | 120 | may only lower it | at the next start | — |
| `search.budget_secs`: How long one `glob` or `grep` may run before it answers with what it found, in seconds. | a whole number from 1 | 10 | may only lower it | at the next start | — |
| `search.max_entries`: How many files and directories one `glob` or `grep` may examine. | a whole number from 1 | 500000 | may only lower it | at the next start | — |
| `ui.helpers`: How helper headers start in the transcript. | `folded`, `expanded` | folded | may set it (the command line wins) | at once | — |
| `ui.bell`: Ring the bell when a turn ends or a prompt opens while the terminal is not focused. | `true` or `false` | true | may set it (the command line wins) | at once | — |
| `ui.mouse`: Capture the mouse (wheel scrolling, click to expand, drag to copy); `false` leaves selection to the terminal. | `true` or `false` | true | may set it (the command line wins) | at the next start | `--no-mouse` |
| `ui.checklist`: Whether the task checklist shows under the input. | `hidden`, `shown` | hidden | may set it (the command line wins) | at once | Ctrl+T |
| `pricing.fetch`: Whether the community price list is fetched for the cost readout. | `true` or `false` | true | may only set `false` | at the next start | `--no-pricing-fetch` |
| `daemon.principal`: The person every attached client decides approval requests as. | text (at most 200 characters) | the login name ($USER, else $LOGNAME, else owner) | may not set it | at the next start | — |
| `[[allow]] / [[deny]]`: Rules: `tool`, then `arg` (a glob) or `literal`. | a section | none | may only add deny rules | at once from your own and the workspace's file (a project file's at the next start) | `--allow` / `--deny <tool>[=<glob>]`; /permissions (In force: r adds, e edits, d removes; Blocked: a, A) |
| `auto.allow`: Statements: kinds of action you allow, which the classifier reads. | a section | none | may not set it | at once | —; /permissions (In force: n adds, d removes; Blocked: e) |
| `auto.trusted_hosts`: Hosts you trust with the project's data (`host` or `*.domain`), which the classifier reads beside the hosts of the project's git remotes; other rules still apply there. | a section | none | may not set it | at once | `--set 'auto.trusted_hosts=["<host>"]'`; /permissions (Hosts: n adds, d removes; Blocked: t, T) |
| `confinement.extra_read / extra_write`: Extra directories for the confined shell, which the approval gate is told too. | a section | none | may not set it | the gate and the file tools at once; a confined shell at the next start | `--confinement-read` / `--confinement-write <dir>`; /permissions (Blocked: a, A) |
| `endpoints.<name>`: `dialect`, `base_url`, `api_key_env`, `priority`, `api_style` (an inline `api_key` is refused). | a section | none | may not set it | at the next start | `--endpoint-def <name>=<dialect>=<url>[,key_env=VAR][,api_style=STYLE]`; — |
| `pricing."<model id>"`: `input`, `output`, `cached_input`, `cache_write` in USD per million tokens. | a section | none | may not set it | at the next start | `--set 'pricing."<id>".input=15'`; — |
| `thinking."<endpoint:id or id>"`: Anthropic thinking: `"adaptive"`, `{ enabled = <budget tokens> }` or `"default"`. | a section | none | may not set it | at the next start | `--set 'thinking."proxy:my-alias"=adaptive'`; — |
<!-- settings-table:end -->

Flags not in the table: `--model`/`-m`, `--endpoint`, `--resume`,
`-c`/`--continue`, `-w`/`--workspace`, `--config-dir`,
`--user-config-root`, `--confinement`, `--external-no-network`,
`--no-confinement`, `--wait-for-mcp`, `--trust-project`, `--attach`,
`--attach-token-file`, `--attach-insecure`, `--list`, `--rehome`,
`--remove-state <ref> [--yes]`, `--audit`, `--classifier-check`
(`rkat-tui --help`).

## Models, endpoints and cost

### Setting up an endpoint

An **endpoint** is a named server that speaks one of three wire
dialects: `anthropic`, `openai` (also LiteLLM, vLLM, llama.cpp, Ollama
and other OpenAI-compatible servers) or `gemini`. The dialect names the
protocol, not who serves the model.

The built-in endpoints `anthropic`, `openai` and `gemini` need only a key:
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or `GEMINI_API_KEY`, a binding in
the realm config, or the `rkat` token store. Any other server is an
`[endpoints.<name>]` table in your `~/.rkat/tui/tui.toml`, the realm's
`tui.toml`, or `--endpoint-def` for one run. A project's `.rkat/tui.toml`
cannot define one.

```toml
# ~/.rkat/tui/tui.toml
[endpoints.work]
dialect = "openai"
base_url = "https://litellm.example.com"   # the server's root, with or without /v1
api_key_env = "WORK_LLM_KEY"               # the variable that holds the key
# api_style = "chat_completions"           # for servers without the Responses API
# priority = 1                             # wins a bare id several endpoints list
```

For one run: `--endpoint-def
work=openai=https://litellm.example.com,key_env=WORK_LLM_KEY[,api_style=chat_completions]`.

The key always comes from the environment: rkat-tui records only the
variable's name. An inline `api_key` refuses the start (exit 2); an unset
variable skips the endpoint with a notice. Models are `work:<model>`, as
the server lists them (`--model work:claude-opus-5-5`, or `/model`). The
classifier reviews with the start-up model unless you give it another
(`--classifier-model work:<model>` or `[auto] classifier_model`).

### How endpoints reach the engine

rkat-tui writes each configured endpoint into the realm's `config.toml`
(the URL, the variable's name, a binding), so `rkat`, REST and RPC on the
same realm can use it too.

**An `openai`-dialect endpoint** runs every model it lists as
`<endpoint>:<model>`, on what the server reports: context window, output
limit, reasoning and image support, and, for LiteLLM's `/model/info`,
prices. Meerkat's catalog is not consulted. So `work:claude-opus-5-5`
gets the proxy's window and prices and plain OpenAI requests (no
Anthropic thinking field or beta headers), while the same id on the
built-in `anthropic` endpoint uses the catalog.

- `api_style` is `responses` (default) or `chat_completions`; it is
  refused on other dialects.
- Tool schemas are rewritten into a form every provider accepts, so a
  Gemini model behind the proxy works too.
- A reasoning block a proxy returns as text whose first line is `thought`
  is shown, exported and copied as thinking.
- `[thinking]` does not apply. Helpers run on the same server.

**An `anthropic`- or `gemini`-dialect gateway** keeps that dialect's
client but runs only models Meerkat's catalog knows (or that you add as
`[models.<id>]` in the realm's `config.toml`). For any other model,
configure the server as an `openai` endpoint instead.

A configured endpoint's own figures (window, limits, prices) win over
the catalog, which fills only what the endpoint leaves out. They drive
the context gauge, `/context` and the compaction threshold.

### Choosing a model

A reference is `endpoint:id` or a bare id. A bare id goes to the endpoint
whose live model list has it; when several do, the current endpoint wins,
then the built-in endpoint of the id's catalog provider, then the highest
`priority`, then the first configured. An id no list has goes to its
catalog provider's endpoint, or to the only endpoint. Names are never
guessed from a prefix.

`/model` opens a picker grouped by endpoint, with each model's input and
output price per million tokens where one is known ([Context gauge and
cost](#context-gauge-and-cost)), `/model <ref>` switches
directly, `/model refresh` refetches the lists. A switch takes effect
between turns, across endpoints and dialects too. A conversation records
its model and endpoint, so `-c` and `/resume` continue on them. Helpers
use the session's model unless `helpers.model` or the call names another
([Helpers](#helpers)); the classifier keeps its own model.

### Thinking

`[thinking]` sets Anthropic thinking per model, keyed by `endpoint:id` or
a bare id (the reference wins). Only your own `tui.toml` files set it;
helpers keep the engine's default.

```toml
[thinking]
"gateway:claude-alias" = "adaptive"       # the model picks its budget
"claude-sonnet-4-5" = { enabled = 8000 }  # at most 8000 thinking tokens
"claude-opus-4-7" = "default"             # no override (undoes a lower layer)
```

A model id outside Meerkat's catalog needs an entry here: without one,
Anthropic refuses a request whose tool list changed (an MCP server
connected, a resume). The setting applies at creation, on every `/model`
switch and on every resume, on Anthropic-dialect endpoints only.
Start-up ignores, with a notice, a reference to another dialect and a
type a catalogued model does not take (Claude Fable 5.2 takes only
`"adaptive"`).

### Context gauge and cost

The status bar shows the context gauge, `ctx 42% (84k/200k)`, and the
conversation's cost, `$0.42`; `/usage` (also `/cost`) breaks it down by
model. `~` marks an estimate.

Prices come from `[pricing."<model id>"]` in your `tui.toml` ([reference](#configuration-reference-tuitoml)),
then a LiteLLM endpoint's `/model/info`, then LiteLLM's community price
list. That list is the only request rkat-tui makes to a host other than
your endpoints:
`https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json`,
at most weekly, cached in `$XDG_CACHE_HOME/rkat-tui` (or the platform's
cache directory). `--no-pricing-fetch` or `[pricing] fetch = false` skips
it.

The daemon counts every model call, attached or not: the agent's, every
helper's, every classifier call and every compaction. Turns that ran
while you were detached are in the totals, and the totals survive a
daemon restart. The figure is an estimate, not a bill.

## Instructions and memory

A new session's system prompt is built from, in order:

1. **The base prompt**: your `SYSTEM.md` (config directory) replaces
   Meerkat's default when it is not empty and the realm config sets no
   `system_prompt` or `system_prompt_file`.
2. **The workspace `AGENTS.md`** (or `.rkat/AGENTS.md`) and the skill
   list ([Skills](#skills)).
3. **rkat-tui's coding persona**, then your own `AGENTS.md` (config
   directory), every `AGENTS.md` (or `.rkat/AGENTS.md`) from the git root
   down to the workspace's parent, and the workspace's `AGENTS.local.md`
   (or `.rkat/AGENTS.local.md`, meant to stay uncommitted). Each file is
   cut at 32 KiB, all together at 96 KiB. The git root is the nearest
   directory with `.git`; your home directory is never one.
4. **The notes index** (below), labelled as untrusted, model-written
   context, not instructions.

Files are read again for every new session (`/clear`, `/new`). A resumed
session (`-c`, `/resume`) keeps the instructions it was created with, so
an edited file applies from the next new session; `/remember` and
`/memory` also reach the live one. After an upgrade, the daemon replaces
rkat-tui's own sections (persona, helper instructions) in a resumed
conversation with the new build's text; your files stay as they were.

| Command | Does |
|---|---|
| `/memory` | lists the three files below; Enter edits one |
| `/memory workspace` | edits the workspace `AGENTS.md` |
| `/memory user` | edits your own `AGENTS.md` |
| `/memory system` | edits your `SYSTEM.md` (applies from the next new session) |
| `/remember <text>`, or one-line `#text` | appends `- <text>` to the workspace `AGENTS.md` |

The editor is `$VISUAL`, else `$EDITOR`, else `vi` (`code --wait`
works). A changed `AGENTS.md`, and a `/remember` line, are also added to
the live session, so the agent's next model call reads them, even
mid-turn. `##…`, `\#…` and multi-line input are sent as prompts.

**Notes** are small Markdown files the agent keeps across sessions
(`note_save`, `note_read`, `note_list`, `note_forget`). Workspace notes
live in `~/.rkat/tui/workspaces/<id>/notes/`, user notes (preferences
for every project) in `<config-dir>/notes/`, each with a generated
`MEMORY.md` index. At most 4 KiB a note and 200 notes per scope; the
prompt carries only the index, cut at 8 KiB. Reading notes is tier 1,
writing a workspace note tier 2; writing a user note goes to the
classifier, since it reaches every project's prompt.

**Semantic memory** is Meerkat's: what a compaction drops is indexed in
`~/.rkat/tui/workspaces/<id>/memory/`, and the agent searches it with
`memory_search` (tier 1). If the index cannot be opened, a start-up
notice says the session runs without it.

**MCP servers** come from the realm config's `[[tools.mcp_servers]]`,
the project's `.rkat/mcp.toml` and your `~/.rkat/mcp.toml` (`rkat mcp
add`), loaded once at start and connected in the background
(`--wait-for-mcp` waits before the first turn). Their tools are tier 3
unless a rule names them. Servers from the project's `.rkat/mcp.toml` may
come with the repository and run with your privileges, so they start
only once you trust the file (`/trust` or `--trust-project`;
[What runs as the host](#what-runs-as-the-host)).

### Context and compaction

A compaction is Meerkat's: before a model call, once the context passes
the threshold, it summarizes the older history, keeps the recent turns,
and indexes what it dropped into semantic memory. `[auto] compact` picks
the threshold:

| `compact` | Compacts |
|---|---|
| `"native"` (default) | at Meerkat's own threshold (80% of the window) |
| `"warn"` (`--no-auto-compact`) | only on `/compact`, or when the provider refuses a request as too large |
| `"auto"` (`--auto-compact`) | before a call would pass `compact_auto_percent` (85%) |

In every mode the gauge warns at `compact_warn_percent` (75%) and after
every turn from `compact_auto_percent`. For a model whose window is
unknown the thresholds are 120k and 150k tokens.

**`/compact`** (or `/compact now`) compacts now; during a turn, before
the run's next model request. Between turns it costs a short extra model
call besides the summary. Meerkat's compaction takes no instructions,
so `/compact` with any other text is refused with the usage (a typo such
as `/compact stauts` does not compact). `/compact status` shows the policy, the threshold and the prepare
instruction.

Each compaction is one folded line, `✓ compacted: 379 → 68 messages (3.2k summary tokens) ▸`;
a click or Ctrl-O shows the summary the agent goes on from, and `/export`
includes it. Nothing is added to the conversation afterwards: the agent's
instructions already tell it to carry on with the task, find earlier
details with `memory_search`, and retire helpers it no longer needs.

**`/context`** shows the gauge, the policy, and what the context holds:
the system prompt, the messages, the tool definitions every request
carries (in bytes and tokens per family, plus tools hidden until a search
reveals them, see [Tools an agent reveals](#tools-an-agent-reveals)), the
skills, and the instruction files on disk. A new conversation's tools
show "not read yet" until its first turn ends. Token sizes are estimates.

#### Preparing for a compaction

A compaction keeps a summary; what the agent had in mind and wrote
nowhere is lost. `[auto] compact_prepare` is an instruction the agent
gets before that:

```toml
[auto]
compact_prepare = "update PLAN.md with where you are and what is next"
compact_prepare_at = "70%"   # or tokens: 300000, 300k, 1.2m; default 70%
```

When a workspace agent's context reaches `compact_prepare_at` (a share of
the start-up model's window, of 200k when unknown), the daemon asks it to
finish its current step, follow your instruction, and call
`compact_ready`, which compacts and tells it to carry on. If the agent
lets the context grow halfway to the automatic threshold (or 10% of the
window further with `warn`) it is reminded once; the automatic
compaction still comes at its threshold. The request re-arms once the
context is below the threshold again. The agent may call `compact_ready`
on its own too; below 20% of the window it does nothing. A
`compact_prepare_at` at or above the automatic threshold would never run
first, and a start-up notice says so. Helpers and forks get no
instruction.

The keys layer like the other compaction keys (your `tui.toml`, the
realm's, `--set`, the project's `.rkat/tui.toml`). In the TUI, these
forms are shorthands for `/set` ([Settings](#settings)), written for this
workspace, or with `/compact prepare --user …` to your own file, and
applied at once:

| Command | Effect |
|---|---|
| `/compact prepare <text>` | sets the instruction |
| `/compact prepare off` | none for this workspace, even if your `tui.toml` has one |
| `/compact prepare default` | removes the workspace's; the lower layer's applies |
| `/compact prepare at <N \| Nk \| N%>` | sets the threshold |
| `/compact prepare at default` | removes the workspace's threshold |
| `/compact prepare at 65% <text>` | sets both |
| `/compact prepare default all` | resets both |
| `/compact prepare` | shows what is set and where from |

A value from the command line or the project's file outranks the
workspace's: a change is then written but not applied, and the answer
says which layer wins.

## Skills

A skill is a named piece of instructions the agent opens when it needs
it. The system prompt lists every skill's name and description under
`<available_skills>`; the agent reads a skill's body with `skill_read`.
Skills come from, in this order:

1. **Yours**: `~/.rkat/tui/skills/` (`<config-dir>/skills/`), listed for
   every agent and helper of every workspace. It is in the config
   directory, so the agent's shell and file tools cannot read or change
   it.
2. **Repositories** in the realm config
   (`~/.rkat/tui/workspaces/<id>/config.toml`): Meerkat's
   `[[skills.repositories]]` (a directory, a Git repository, an HTTP
   source).
3. **The project's** `<workspace>/.rkat/skills/`, read whenever it exists.
   A skill is text, not code, so `/trust` does not cover it.

A project skill with the same name as one of yours is not listed, and a
request for it gets yours: a cloned repository cannot replace your skill.

**Writing one.** Each skill is a directory directly under the skills
directory, named in lowercase letters, digits and single dashes, holding
a `SKILL.md`:

```markdown
---
name: code-review
description: How I want a change reviewed before I merge it.
---

Read the whole diff first. Check error handling and tests before style.
```

`name` (exactly the directory's name) and `description` (one line) are
required; `version`, `metadata` (a map of strings) and
`requires_capabilities` are optional, and any other key must be
namespaced (`myteam.owner`). A skill whose frontmatter does not parse or
whose name differs from its directory is left out, and the daemon's log
names it. A skill nested deeper is found but cannot be loaded.

**Cost.** Every listed skill costs every model request about 120 bytes
plus its name and description, so keep descriptions to one line. Above
`[skills] inventory_threshold` skills the list shows only a count per
source, and the agent finds them with `skill_read` without a name.
Meerkat's 12 built-in skills fill its default threshold of 12, so
rkat-tui uses 32 when the realm config leaves it at the default: room for
20 of yours and the project's.

**Settings** (realm config): `[skills] enabled = false` turns all skills
off, yours included; `inventory_threshold` sets the threshold (12 counts
as unset).

**`skill_read`** returns a skill's body (at most 32 KiB; `source` picks
between two sources sharing a name), or with no name lists every skill.
It reads through the skill sources, never a path, is read-only, and is
allowed without asking in every mode, plan mode included. Meerkat's own
`browse_skills` and `load_skill` are not offered.

`/context` shows your and the project's skills directories, how many
valid skills each holds, and whether the list names them all.

## Keys and commands

The keys, the commands and the approval prompt follow Claude Code's
wherever both have the feature. [`docs/ux-parity.md`](ux-parity.md)
compares the two feature by feature.

### Commands

`/help` shows these in a dialog over the transcript, with the keys one Tab
away; it scrolls (↑/↓, `j`/`k`, PgUp/PgDn, Home/End, the mouse wheel) and
Esc, `q` or Ctrl-C closes it. Nothing of it goes into the transcript, and
the agent goes on underneath. `--user` (or `--global`) directly after a
command that sets something (or after its subcommand) writes your own
`tui.toml` (every workspace) instead of the workspace's; anywhere else
it is part of the text. A command that takes no argument (`/help`,
`/quit`, `/clear`, …) refuses one. `//` at the start of a prompt sends a literal `/`.

| Command | What it does |
|---|---|
| `/settings [filter]` | Every setting, its value and the layer it comes from; edit or remove it there ([Settings](#settings)) |
| `/set [--user] <key> <value>`, `/unset [--user] <key>` | Write or remove a setting in the workspace's `tui.toml` |
| `/help`, `/?` | The commands and keys, in a dialog (Tab switches) |
| `/quit`, `/exit`, `/q` | Leave; your agent retires, and the daemon may stop ([The workspace daemon](#the-workspace-daemon)) |
| `/detach` | Leave; the agent keeps working |
| `/agents [name]` | Pick an agent of this workspace: switch, start a new one, retire an idle one ([Several agents](#several-agents)) |
| `/wire [workspace/agent \| ssh://host[/workspace/agent]]` | Wire this agent to one in another workspace or, over ssh, on another machine; the picker lists its wires first, and `d` on one removes it ([Wiring agents across workspaces](#wiring-agents-across-workspaces)) |
| `/audit [n]` | The audit log's last `n` (20) lines for this agent; `rkat-tui --audit` prints it all |
| `/schedules [pause\|resume\|delete <name>]` | This agent's schedules ([Schedules](#schedules)) |
| `/clear`, `/new` | Start a new session |
| `/resume [ref]` | Switch to another conversation; no ref opens a picker ([Sessions](#sessions)) |
| `/permissions [blocked\|rules\|hosts \| list \| add [--user] allow\|deny <tool> [pattern] \| rm <n>]` | The permissions dialog, or list, add and remove rules (`remove` = `rm`) ([The permissions dialog](#the-permissions-dialog)) |
| `/trust` | Show what the project's `.rkat/mcp.toml` and `.rkat/config.toml` would run and ask; `y` trusts them as shown, their MCP servers and hooks run from the next session (`--trust-project`: for one run) |
| `/mode [[--user] default\|accept-edits\|plan\|auto]` | Pick or set the permission mode ([Permission modes](#permission-modes)) |
| `/auto [on\|off\|resume\|stats]` | Show auto mode; switch it on or off; resume it after a denial pause; its review numbers |
| `/auto model [[--user] [endpoint:]<id>]` | Pick or set the classifier's model |
| `/auto check [all\|statements\|<n>\|<case id>…]` | Measure the classifier's model on built-in cases, one paid call each ([Checking your classifier model](#checking-your-classifier-model)) |
| `/model [[endpoint:]<id> \| refresh]` | Pick or switch the model; `refresh` fetches the lists again |
| `/remember <text>`, `#<text>` | Add a line to the workspace `AGENTS.md` and this session (`\#` sends a literal `#`) |
| `/btw <question>` | A side question; see [below](#a-side-question-btw) |
| `/compact [now \| status \| prepare …]` | Compact before the next model request (other text is refused); `status` shows the policy; `prepare` sets the pre-compaction instruction ([Preparing for a compaction](#preparing-for-a-compaction)) |
| `/status` | Version, model, session, workspace and realm, confinement, mode, classifier, trust, helper depth, file watches |
| `/daemon stop\|start` | Shut the daemon down (sessions stay), or connect again, starting it if needed |
| `/memory [workspace\|user\|system]` | Edit `AGENTS.md` (workspace or yours) or `SYSTEM.md` in your editor; an `AGENTS.md` edit reaches this session, `SYSTEM.md` the next |
| `/init` | Ask the agent to write or update the workspace `AGENTS.md` (an ordinary turn; its edits pass approval) |
| `/export [file]` | Write the transcript as plain text, by default to the workspace's state directory |
| `/rehome [directory \| id \| old path]` | Move this workspace's state to a directory, or adopt the state of a workspace that moved here; alone, a picker (`from <ref>` / `to <dir>` when it reads both ways) |
| `/context` | Context usage, compaction policy and what the context holds |
| `/copy [N]` | Copy the latest (or N-th latest) answer to the clipboard |
| `/usage`, `/cost`, `/stats` | Token usage and cost per model |
| `/helpers [list [all] \| expand\|fold <label>\|all]`, `/tasks` | This conversation's helpers: open, fold or stop one ([Helpers](#helpers)) |

`/rehome <directory>` onto a directory that has state of its own asks
before deleting that state. `/export` writes rkat-tui's own file, readable
only by you; it is not a tool call.

#### Commands during a turn

`/model`, `/clear`, `/resume`, `/init`, `/agents <name>` (and the picker's
switch) and `/rehome` with an argument need the agent idle. Typed during a
turn, each waits in the queue, marked "when the turn ends", and runs once
the agent is idle, in the order typed: `/model x` then a message switches
first and sends the message on the new model. A message typed after a
waiting command waits too. ↑ takes a waiting command back. Without an
argument, `/model`, `/resume` and `/agents` open their picker at once and
only the choice waits. `/compact` does not wait. Quitting or detaching
drops waiting commands, with a notice.

#### A side question: `/btw`

`/btw <question>` asks about this session without touching the agent's
run, even while it works. The daemon makes one model call of its own on
the agent's model, with no tools, from the stored history (about the last
60 KB and the first message), the running turn and the tool now running.
It answers in a dim `↳ btw:` block. The agent never sees the question or
the answer; `/export` keeps it, a re-attach does not. Esc while it is
pending cancels only the question. One at a time; its tokens count in
`/cost`.

#### List dialogs

The lists of `/permissions`, `/schedules`, `/agents`, `/wire`, `/mode`, `/memory`, `/settings`, `/rehome` and `/helpers` show their keys
in a footer (`?` lists them too). ↑/↓ or `k`/`j` move; Esc, `q` or Ctrl-C
close. A list stays open after you act in it and refreshes in place; only
acts that take you elsewhere close it (choosing a mode, opening a helper,
switching agent). Anything that deletes, retires, removes or grants asks a
`y`/`n` question first. An approval request takes over the screen, and the
list (or the help) comes back after it.

#### Menus, mentions and shell mode

`/` at the start of a one-line input opens a menu of matching commands:
↑/↓ choose, Tab completes, Enter completes a partial name or sends a full
one, Esc closes it. An entry recalled from the history opens no menu until
you edit it.

`@` at the start of a word opens a menu of workspace paths (`.gitignore`
honoured; `.git`, `.rkat` and the config directories left out). On send,
each `@file` is attached after the prompt (64 KiB per file, 256 KiB in
all) and each `@dir/` as a listing of up to 200 entries; a sealed path is
never read. An attached file is data, not your words: the classifier sees
only the file names, so an instruction planted in a file cannot pass for
your consent.

An input starting with `!` is a shell command that runs **as you, with no
sandbox and no approval**: your `$SHELL` (else `/bin/sh`), in the
workspace, with your whole environment and credentials. `--confinement`
applies only to the agent's commands. It may run `shell.user_timeout_secs`
(default 120; a project file may only lower it); Esc or Ctrl-C cancels it.
Its output (capped at 64 KiB) shows in the transcript and joins the
session as context the model reads on its next call, without starting a
turn. During a turn it waits and runs when the turn ends, before queued
messages. `!!` or `\!` at the start sends the text as a prompt.

### Keys

`?` on an empty input shows these, in the help dialog.

| Key | What it does |
|---|---|
| `?` (empty input) | Open the help on the keys (Tab: the commands) |
| Enter | Send; while the agent works, queue for its next step |
| Shift+Enter, Alt+Enter, Ctrl+J, `\` Enter | Newline |
| Esc | Interrupt the turn (a pending `/btw` takes the first Esc); in a helper's full view, go back |
| Esc Esc (idle, with text) | Clear the input into the prompt history |
| Ctrl-C | Interrupt, else clear the input, else press twice to quit |
| Ctrl-D | Quit on a second press (empty input); else delete forward |
| Ctrl-X Ctrl-S | Send now: interrupt and send what waits, then the input |
| Up (empty input) | Take queued messages and waiting commands back |
| Up/Down, Ctrl-P/N | Move between lines, then through the prompt history |
| Ctrl-R | Search the prompt history (workspace, then global) |
| Ctrl-O | Show every tool call, reasoning block and outcome in full, or fold them back |
| Ctrl-T | Show or hide the [task checklist](#task-checklist) |
| Ctrl+Up/Down | Select a helper in the panel; Enter on an empty input opens its full view, where Ctrl-C asks whether to stop it |
| Ctrl-G, Ctrl-X Ctrl-E | Edit the input in `$VISUAL`, `$EDITOR` or `vi` |
| Ctrl-A/E, Alt+B/F, Ctrl/Alt+Left/Right | Line start/end, word left/right |
| Ctrl-K/U, Ctrl-W, Alt+D, Ctrl/Alt+Backspace | Kill to line end/start, word before/after |
| Ctrl-Y, Alt+Y | Yank; cycle older kills |
| Ctrl-S | Stash the input; on an empty input, restore it |
| Ctrl-_ (Ctrl-7, Ctrl-Shift--) | Undo |
| PgUp/PgDn, Shift+Up/Down | Scroll the transcript |
| Home/End | Top or bottom when the input is empty (Ctrl+Home/End always) |
| Ctrl-L | Redraw the screen |
| Ctrl-Z | Suspend (`fg` resumes); the agent keeps working in the daemon |
| Shift+Tab | Cycle manual → accept edits → plan → auto ([Permission modes](#permission-modes)) |
| Alt+P | Pick a model without clearing the input |
| `/`, `!`, `@` | The command menu, shell mode, the path menu |

In the approval prompt:

| Key | What it does |
|---|---|
| `1`, `y` | Allow once |
| `2`, `t` | Allow, and grant what the row shows for this session |
| `3`, `a` | Allow, and add the rule the row shows |
| `4`, `n`, Esc | Deny; type what the agent should do instead and Enter sends it with the denial |
| Up/Down, `j`/`k`, Enter | Move, and pick the row moved to |
| Ctrl+O, then PgUp/PgDn, Home/End | Show and scroll the whole arguments |
| `s` | Stop the turn (on a helper's prompt, only that helper) |
| Ctrl-C | Stop this prompt and the others of its caller |

A prompt that opens while you type ignores keys until you have paused for
0.6 s, so typing cannot answer it by accident.

A paste of several lines, or one line over 80 characters, shows as a
`[Pasted text #N]` chip that edits as one character and is sent as its
full text.

#### Working or waiting

The input's prompt marker shows whether the agent is busy: `›` when idle,
a turning cyan spinner while a run goes (also one this client did not
start, such as a schedule's), a classifier review or a compaction. The
spinner turns only while events arrive. After 20 seconds without one it
becomes a slow yellow pulse, and the status bar says what is waited for
and since when: your answer, a compaction, the classifier, a running
tool, the model's reply, or simply no word from the agent. That is a fact,
not a verdict: a long tool or a slow model is silent too. After two
minutes it adds that Esc interrupts.

#### Bell

When a turn ends, or an approval prompt opens, while the terminal does not
have the focus, rkat-tui rings the terminal bell, which most terminals
turn into a sound, badge or notification. A terminal that sends no focus
reports counts as focused and never rings. `ui.bell = false` turns it
off.

#### Mouse and clipboard

The mouse is captured: the wheel scrolls, a click opens or folds one tool
call, and a drag (double click: a word, triple: a line) selects and
copies. Middle-click pastes what rkat-tui copied last. Shift with the
mouse gives you the terminal's own selection. `--no-mouse` (one run) or
`ui.mouse = false` (default `true`) leaves the mouse to the terminal.

Copying uses OSC 52 only, which kitty, foot, WezTerm, Alacritty, iTerm2
and xterm (with `allowWindowOps`) support and GNOME Terminal and other
VTE terminals do not. It works over ssh. Inside tmux, set `set -g
set-clipboard on` (or `allow-passthrough on`, tmux 3.3+); tmux's default
ignores it. rkat-tui cannot check either, so its notice says the copy was
written, not that it arrived. A copy is cut at 100 KiB.

### Task checklist

The agent keeps a task list with its `task_create` and `task_update` tools,
which need no approval. Ctrl-T shows it under the input: `☐` pending, `◐`
in progress, `☑` done, at most five lines. The list belongs to the
session, so a resumed session gets it back and a new one starts empty.
Ctrl-T saves the choice as `ui.checklist` (`"hidden"` by default, or
`"shown"`) in the workspace's `tui.toml`; every attached client follows
it.

## Where things live

**The workspace's state** lives outside the workspace, in
`~/.rkat/tui/workspaces/<id>/` (`<config-dir>/workspaces/<id>/` with
`--config-dir`), `<id>` being 16 random hex digits. Messages call this
directory a *realm*. It is owner-only and holds everything rkat-tui
keeps for the workspace: the conversations and the engine's stores,
endpoint connections (never a key), the workspace's `tui.toml`, usage
records, helpers' bookkeeping, wires and their sockets, input history,
unsent messages, workspace notes, logs (`logs/rkat-tui.log`; a log over
10 MiB is moved to `<name>.1` when it is next opened and started afresh,
one old generation kept), `/export`'s default files and the audit log. Neither the agent's shell nor a cloned
repository can reach it. The workspace itself keeps only what the
project owns: `.rkat/tui.toml`, `.rkat/mcp.toml`, `.rkat/config.toml`
(hooks) and `AGENTS.md`, untrusted until `/trust`. A `.rkat/tui/` in the
workspace is never read.

Each state directory's `workspace.toml` names its workspace (path, name,
git remote, created, last used), and is the only thing that decides
which workspace owns it. Two naming the same workspace, or one that
cannot be read, refuse the start until you remove or re-home one.
`rkat-tui --list` lists every workspace's state.

**Which workspace:** `--workspace <dir>` (`-w`), else the nearest
ancestor of the current directory that has state (up to the git root,
never your home directory), else the current directory. From your home
directory you must pass `--workspace`. The daemon refuses to start when
the sandbox would reach the state directory, as with `-w ~` or
`--confinement-write ~/.rkat`; use `--config-dir` elsewhere, or drop the
grant.

**Your own files** are in the config directory, `--config-dir`, else
`~/.rkat/tui` (`<--user-config-root>/.rkat/tui` with that flag;
`$XDG_CONFIG_HOME` is not used): `tui.toml`, `AGENTS.md`, `SYSTEM.md`,
`skills/`, `notes/`, `history`, host trust records (`trust/`) and
`workspaces/`. The price-list cache is in `~/.cache/rkat-tui/`
(`<config-dir>/cache` with `--config-dir`); the daemon's endpoint file
is under `~/.local/state/rkat-tui/` ([The workspace
daemon](#the-workspace-daemon)); the engine keeps each conversation's
comms key pair in `~/.rkat/session_comms_identity/`.

### Moving or renaming a workspace

A moved workspace starts with fresh, empty state and offers its old
state in a notice; nothing is adopted on its own. Re-home it once:

| Command | Does |
|---|---|
| `/rehome` | opens a picker: state you could adopt (workspaces gone first, then, dim, ones that still exist) and a row to move this workspace's state to a directory you type; each choice asks first |
| `/rehome <id or old path>` (`/rehome from <ref>`) | pull: adopts that state here; this workspace's empty state is removed and the TUI continues the latest conversation |
| `/rehome <directory>` (`/rehome to <dir>`) | push: moves this workspace's state to that existing directory; the daemon restarts there and this TUI reattaches to the same conversation |
| `rkat-tui --rehome <id or old path>` | pull from the shell, starting nothing |

State is named by its id, its own directory, or the path its workspace
had (`~` expands). Use `from` or `to` when an argument reads both ways.
After a push, other attached TUIs are disconnected, and the old
directory is not touched. Adopting state of a workspace that still
exists takes it from there; that directory starts fresh next time.
`rkat-tui -c` in the new directory continues the latest conversation.

`/rehome` typed during your agent's turn waits for the turn to end. A
re-home is refused, changing nothing, while a turn runs on another
daemon it would stop, or while the destination's state has another TUI
attached. If it would replace state that holds a conversation, the TUI
asks before deleting it, which cannot be undone; `--rehome` refuses and
names the delete command. Moving empty state to a directory no state
names is refused as a likely typo (`/rehome to` forces it). A directory
inside the state, holding it, or in the config directory is refused.

### Deleting a workspace's state

`rkat-tui --remove-state <ref>` (named as for `--rehome`) prints what
it would delete; with `--yes` it deletes the state directory and every
conversation in it, which cannot be undone. An idle daemon on it with no
TUI attached is stopped first; a running turn or attached TUI refuses.
Only directories under the config directory's `workspaces/` are ever
deleted. The `/rehome` picker's `d` does the same for a row.

### The audit log

`audit.jsonl` in the state directory, written by the daemon alone and
out of the agent's reach. One JSON line per approval decision (tool,
argument, tier, `allow`/`deny`, and who decided: `rule:<id>`,
`classifier`, `human:<connection>`, `mode`, `policy`, `unattended`,
`timeout`, `review-failed`) and per change of authority (mode, rules,
`/trust`, a retired agent, daemon start and stop). Past 50 MiB it rolls
to `audit.1.jsonl`; one older generation is kept. `/audit [n]` shows the
last lines for your agent; `rkat-tui --audit` prints it all.

### When the disk fills

A full or failing disk under the state directory stops the agent from
saving: the turn fails with a line saying so, and the step it was on is
lost (earlier work stays). Free space and send again; the daemon reloads
the agent itself, with no restart. It warns each agent's clients when
free space drops below 200 MB.

## The workspace daemon

The agent does not run in `rkat-tui`. It runs in the workspace's daemon,
`rkat-tui-mob`, and `rkat-tui` is a client: it attaches to the running
daemon, or starts one. So the agent keeps working when you `/detach`, and
any number of TUIs can attach. Each `rkat-tui` you start gets an agent
of its own, named `main-<id>` ([Several agents](#several-agents)).

### Which daemon, which agent

A start answers two separate questions. `--attach` says which daemon (by
default this workspace's). `-c` or `--resume` says to join an agent
already in it; without them you get a new agent there:

| | joins an agent | starts a new agent |
|---|---|---|
| this workspace's daemon | `rkat-tui -c` (or `--resume <ref>`) | `rkat-tui` |
| another daemon | `rkat-tui --attach=<dir or HOST:PORT> -c` | `rkat-tui --attach=<dir or HOST:PORT>` |

`-c` (or `--resume last`) joins the agent active last; `--resume <ref>`
the agent that owns that conversation (seating it again if it quit);
`--resume` alone the agent active last, with the conversation picker
open.

Agents you leave cost nothing while idle, but their schedules and file
watches can wake them (and spend model calls). `/agents` lists them,
switches to one, and retires an idle one.

| Flag | Meaning |
|---|---|
| (none), `--attach`, `--attach=auto` | The workspace's daemon; start it when none answers |
| `--attach=existing` | The workspace's daemon, only if it runs |
| `--attach=DIRECTORY` | The running daemon of the workspace at or above `DIRECTORY` (never starts one; use it instead of `-w`) |
| `--attach=HOST:PORT` | A daemon through a tunnel, with `--attach-token-file`; loopback only, unless `--attach-insecure` |
| `--list` | Every daemon with its agents, then every workspace's state; exit (also `rkat-tui-mob --list`) |
| `--audit` | Print the daemon's audit log; exit ([Where things live](#where-things-live)) |
| `--rehome REF`, `--remove-state REF [--yes]` | Adopt or delete a workspace's state; exit ([Where things live](#where-things-live)) |
| `--classifier-check[=quick\|all\|N]` | Measure the classifier model; exit 0, 3 or 1 ([Checking your classifier model](#checking-your-classifier-model)) |

`--attach` takes a value only with `=`: `rkat-tui --attach "fix the
test"` attaches and sends the prompt.

**Another workspace.** With `--attach=DIRECTORY` everything local happens
in that workspace: `!` commands, `@` mentions, `/export`, `/remember`,
`/memory` and the input history.

**Another machine.** The daemon listens on loopback only. Forward a port
and copy its endpoint file, which holds the token:

```bash
rkat-tui --list                      # on the remote host: the daemon's port
ssh -N -L 127.0.0.1:4100:127.0.0.1:DAEMON_PORT host &
scp host:.local/state/rkat-tui/<key>/mob.endpoint ~/.rkat/devbox.token
chmod 600 ~/.rkat/devbox.token
rkat-tui --attach=127.0.0.1:4100 --attach-token-file ~/.rkat/devbox.token
```

- The token is the whole access control: whoever holds it can answer
  approvals, change the mode and add rules. A token file others can read
  or write is refused.
- The token itself never crosses the wire (both sides prove they hold
  it, the daemon first), but the session does, unencrypted. So
  `--attach=HOST:PORT` accepts only a loopback address, which `ssh -L`
  gives; `--attach-insecure` allows others, for a network you trust end
  to end.
- If the workspace is not on this machine, the features that touch local
  files (`!`, `@` mentions, `/remember`, `/memory`, `/export` to a path)
  are refused; everything else works.

**The endpoint file** holds the daemon's address, token, pid and
workspace: `$XDG_STATE_HOME/rkat-tui/<key>/mob.endpoint` (default
`~/.local/state/…`; `<key>` is the first 32 hex digits of the SHA-256 of
the workspace's path), mode 0600 in a 0700 directory, removed on
shutdown. It is never in the workspace, which the agent's confined shell
can read. If the confinement would grant the state directory (a
workspace at your home directory, say), the daemon uses
`$XDG_RUNTIME_DIR/rkat-tui/<key>/` instead, and refuses to start when
neither is safe; set `XDG_STATE_HOME` or `rkat-tui-mob --endpoint-file`
then. The agent's file tools cannot read it; with `--no-confinement` the
agent's shell can.

`--list` marks a file whose address refuses connections `stale` (and
removes it when its process is gone), and one that does not answer
`unknown`. No daemon starts beside an `unknown` file: if no daemon runs,
remove it.

**Start on demand.** The TUI starts `rkat-tui-mob` (next to `rkat-tui`,
else from `PATH`) with this command line's engine flags, detached from
the terminal (through `setsid` when available), its output in
`~/.rkat/tui/workspaces/<id>/logs/rkat-tui-mob.stderr.log`. If it fails,
the status bar quotes the log and retries a few times; then fix what the
log says and type `/daemon start`. You can also run it yourself:
`rkat-tui-mob -w . --model …`.

**Confinement is the daemon's.** The agent's shell runs under the flags
the daemon started with (`/status` shows them); an attaching TUI's
`--confinement` flags change nothing. To change them, `/daemon stop` and
attach again with the new flags. `/daemon stop` also picks up a new
build when the status bar shows `daemon build …`.

**Detach and quit.** `/detach`, closing the terminal, losing the
connection or a TUI crash leaves the agent running, turn included.
`/quit` (Ctrl-D, Ctrl-C twice) means you are done with your agent:

| Situation | `/quit` |
|---|---|
| Other TUIs work with this agent | Leaves; agent and daemon stay |
| Other TUIs are attached to other agents | Retires this agent (interrupting its turn; the conversation stays for `/resume`); the daemon stays |
| Last TUI, it started the daemon, no agent left detached | Stops the daemon; every agent retires |
| Last TUI otherwise (an agent left detached, a daemon you started, or the daemon cannot say) | Asks: detach (default) or terminate (stop the daemon, retire every agent, interrupt a running turn; conversations stay) |

Messages this TUI still held when it left are saved to
`~/.rkat/tui/workspaces/<id>/tui_unsent.txt`, and the next start says so.

**Stopping the daemon.** `/daemon stop` from any attached TUI, or `kill
<pid>` (from `--list`), stops it and keeps every agent: the next start
restores them, detached. Terminate retires them, and the next start seats
a fresh agent. Sessions stay either way. A daemon that died is started
again by a reconnecting TUI; reattaching shows only what happened since.

**Shared by every attached TUI:** the agent's output and diffs, its
helpers, the token totals and `/usage`, the mode and rules, and approval
dialogs (the first answer counts; an unanswered one is denied when it
times out). Messages typed during a turn, from any TUI, go to the agent's
next request. The daemon refuses `/new`, `/resume` and `/model` while a
turn runs, from whichever TUI; every TUI on the agent follows a switch. A conversation
belongs to the agent that ran it; `/resume` of another agent's says
where it is. The daemon records every approval as made by
`principal` under `[daemon]` in your own or the realm's `tui.toml`
(default: your login name), whatever the client claims. `!` commands run
in the TUI as you, with no sandbox (Shell mode in [Keys and
commands](#keys-and-commands)).

**Limits:**

- There is no rewind.
- `/compact now` right after a daemon start is refused until the agent
  has run; send a message first.
- `/trust` applies the project's hooks from the next conversation or a
  daemon restart. A `.rkat/mcp.toml` trusted later does not reach an
  agent whose daemon's mob already exists.
- Until a helper's first run ends (or the turn of an agent a daemon
  restart resumed mid-turn), the gate knows only rkat-tui's and the
  engine's tools; any other tool, such as an MCP tool, is reviewed as an
  unknown tool by the classifier or you.

## Several agents

One daemon serves a workspace, and it hosts one agent per `rkat-tui` you
start there. Several agents can work in the same directory at once, each
with its own user, conversation, model and helpers. Each is named
`main-<id>` (four letters and digits, fixed for its life).

The first `rkat-tui` in a workspace starts the daemon and its first
agent; every later start gets a new agent. `-c` joins the agent that was
active last instead, and `--resume <ref>` joins (or seats again) the
agent that owns that conversation.

Approvals, rules, permission mode, notes and semantic memory belong to
the workspace, so all agents share them. Each TUI shows only its own
agent's transcript, helpers, diffs, approval requests and classifier
notes.

### `/agents`

`/agents` opens a picker of the agents with their state, conversation
and attached TUIs. Enter switches this TUI to an agent (the one you
leave keeps running; during your turn the switch waits for its end, see
[Commands during a turn](#commands-during-a-turn)); the last row, `+ new
agent`, starts a new one. `d` retires an agent (see [List
dialogs](#list-dialogs)), but only an idle one with no TUI attached and
not your own. `/agents <name>` switches directly.

### `/quit` and `/detach`

`/quit` retires your agent: a running turn is interrupted, its helpers
stop, its schedules are deleted, the other agents are told, and nothing
can wake it any more. Its conversation stays in `/resume`, and
`rkat-tui --resume <id>` seats the agent again. If other TUIs work with
the same agent, `/quit` just leaves it to them. The last `/quit` stops
the daemon only if that TUI started it and no agent was left detached;
otherwise it asks whether to detach or terminate every agent.

`/detach` keeps your agent running and reachable: another agent's
message can wake it while you are away. `rkat-tui -c`, or `/agents` from
another TUI, brings you back.

### Messages between agents

Every agent can message every other agent of the workspace with
`send_message`. An idle agent wakes for a message and takes a model
turn; a busy one reads it at its next step. The receiving TUI shows it
labelled with the sender (`✉ from agent main-abc`), so you see why your
agent acted. Runs of messages from the agent's own helpers fold into one
line.

A message to another agent of the workspace, from an agent to its own
helper, or between connected helpers of the same agent, is tier 1 in
every mode, plan mode included: it changes nothing, and whatever the
receiver does passes its own gate. Any other send is reviewed.

The agents are told to announce file changes only to agents that are
working, not to edit a file another agent just announced, and to treat a
peer's message as a colleague's, not as your instruction. This is
guidance, not a gate. The daemon tells every agent when another joins or
leaves, and nothing more.

The engine's `send_request`, `send_response`, `reply_to_peer`, `peers`
and `list_members` are not offered; agents answer with `send_message`
and find each other with `contacts`.

### `contacts`

`contacts` tells an agent or a helper whom it can message: its parent,
its helpers (kind, running or idle, task, connections), helpers it was
connected with, this workspace's agents and whether each is working, and
wired agents of other workspaces. Each entry carries the `peer_id` that
`send_message` needs; a name does not work. It is always allowed, also
to a helper at the depth limit.

### Connecting helpers

A helper is wired to its parent only, not to the other agents or to
other helpers. `connect_helpers` (`{"helpers": ["scout", "tester"]}`)
wires two or more of the caller's own running helpers to each other,
every pair both ways; any kind counts, in one mob or across mobs. A pair
stays connected until one end leaves, also across a daemon restart.
There is no `disconnect_helpers`. Every message wakes its receiver, so
connect only helpers that must coordinate.

### What is and is not kept apart

The agents share one daemon, one user and one token. The daemon keeps
them apart for clarity, not as a security boundary:

- Every TUI attached to the daemon receives every agent's approval
  requests (command line, diff, reason), though it shows only its own.
  Anyone with the token can read them all.
- The daemon refuses a prompt, interrupt, compaction or context note a
  TUI sends for another agent. That stops a stray client, not a token
  holder, who can attach to any agent by name.
- Another agent can start a turn on yours that nobody prompted: a
  message wakes an idle agent, a `/detach`ed one too. While no TUI is
  attached to your agent, such a turn is **unattended**:
  - reads, allowlisted commands and in-project edits your mode allows
    without asking run as usual;
  - in auto mode the classifier still judges, and runs what it allows;
  - anything that would ask you is denied: what your mode or a
    protected file would ask about, a paused auto mode, an action too
    large to review, a review that could not complete.

  The next TUI to attach shows a warning for each denied action; ask
  again then. Attaching mid-turn makes its later calls reviewed as
  usual. A turn you started stays yours after `/detach` and is reviewed
  as usual. The classifier never takes a peer's text as your words.

## Wiring agents across workspaces

An agent knows only the agents of its own workspace. `/wire` connects
your agent to an agent of another workspace, on this machine or over
ssh on another, so the two can message each other: an API project's
agent and its client project's, say.

| Command | Does |
|---|---|
| `/wire` | picker: your agent's wires first (marked wired, with the other side's state or the ssh link; `d` then `y` removes one, Enter on one only says it is wired), then the top-level agents of every other workspace on this machine whose daemon runs (Enter wires) |
| `/wire <workspace>/<agent>` | wire directly; the workspace by directory name or path, alone when one agent works there |
| `/wire ssh://<host>` | list the agents running on another machine |
| `/wire ssh://<host>/<workspace>/<agent> [--remote-rkat-tui <path>]` | wire to an agent there |

**Running `/wire` is your consent, for both directions.** Neither
workspace asks anything further. A message to a wired agent is tier 1
(not reviewed) outside plan mode, so a wire is an unreviewed data path:
what one agent read in its project it can hand to the other. A message
carries no authority: the receiver's calls pass its own workspace's gate
and sandbox, and a turn it starts while no client is attached there is
unattended. A message to an idle agent costs a model turn there.

In plan mode a send to an agent of another workspace is put to you,
because plan mode covers what your agent sets in motion there too; in a
turn no client watches it is denied.

Each daemon trusts one named agent's key, not the other daemon. Across a
wire an agent is named `<workspace>/<agent>` (`api/main-ab12`). Only
top-level agents can be wired, not helpers, one pair per `/wire`.

**What to expect.**
- To wire to a workspace whose daemon is not running, start `rkat-tui`
  there first. A wire from a TUI attached to a daemon on another machine
  (`--attach=HOST:PORT`) works only as `/wire ssh://…`.
- When the other side's daemon is down, the wire stays but sends fail at
  once and are not kept. Starting `rkat-tui` there brings it back; nothing
  needs wiring again.
- `/new` and `/resume` give an agent a new key. Your side updates the
  other daemons; if one is not running you are told to `/wire` again once
  it runs, and until then messages are not delivered.
- If `/wire` is cut off halfway (the TUI killed), the wire is one-sided
  and the other side's messages are dropped: run `/wire` again.
- Your side of a wire is always removed by `d` on its row in `/wire`; the
  other side goes too when its daemon runs, otherwise when that agent
  retires or the wire is removed there. A retired agent loses its wires;
  the other side's `/wire` picker marks the stale one. The picker opens
  with your wires even when no other daemon runs.
- The socket path must fit a Unix socket address (107 bytes on Linux,
  103 on macOS); a very long `--config-dir` makes `/wire` fail with the
  path.

### Agents on another machine (over ssh)

`/wire ssh://…` works the way rsync does over ssh: `<host>` goes to
`ssh` as is, so your `~/.ssh/config` applies. It needs:

- **here:** OpenSSH's `ssh`, and `ssh <host> true` working without a
  prompt. rkat-tui runs ssh with `BatchMode=yes`, stores no credentials
  and never asks for a password;
- **there:** the same rkat-tui version on a non-interactive shell's
  `PATH` (or named with `--remote-rkat-tui`), rkat-tui running in that
  workspace, and an ssh server that allows Unix-socket forwarding
  (`AllowStreamLocalForwarding`, on by default).

Your workspace's daemon runs `ssh`, as you and outside the agent's
sandbox; nothing the agent can call starts it. It holds one connection
per wire, and the other machine never connects back, so it cannot
`/wire` back through it, and removing the wire there removes only its own
half. ssh
is only the transport: every message is still signed by the sending
agent's key.

When the connection drops, your daemon notices within about 45 seconds,
tells you, and reconnects by itself; sends meanwhile fail and are not
kept. The wire is stored (`wires.json` in the workspace's state), so a
daemon restart reconnects it.

## Timers and watches

### Schedules

An agent can set itself a timer with the engine's schedule tools
(`meerkat_schedule_create`, `_get`, `_list`, `_update`, `_pause`,
`_resume`, `_delete`, `_occurrences`), which it reveals with
`tool_search` ([Tools an agent reveals](#tools-an-agent-reveals)). A
schedule fires once, at an interval, or on cron-like calendar fields,
and runs a prompt in the agent's own session as if someone had typed it;
the TUI labels it `⏰ scheduled: <name>`. A schedule follows its agent
across `/new`, `/resume` and daemon restarts. Helpers have no schedule
tools.

**Approval.** Reading schedules is tier 1. Creating, changing, pausing,
resuming or deleting one is reviewed (the classifier in auto mode, you
in manual mode).

**Fired turns.** With a TUI attached, a fired turn is reviewed like one
you typed. With nobody attached it is **unattended** ([Several
agents](#several-agents)): what would ask you is denied. So a nightly
`cargo test` runs unattended only if it is allowlisted
(`--trust-project-builds`, an `[[allow]]` rule) or the classifier allows
it; in manual mode everything reviewed waits for you.

**`/schedules`** lists this agent's schedules with their next
occurrence, state, fire count and the cost of their runs, and lets you
pause, resume or delete them. `/schedules pause|resume|delete <name or
id>` does it directly. These are your own actions, so nothing asks for
approval. The audit log records fired runs (`run_triggered`) and your
changes (`schedule_changed`).

**Lifetime.** Schedules keep firing after `/detach`. Retiring the agent
(`/quit`, terminate) deletes its schedules. Nothing fires while the
daemon is stopped; on restart an occurrence more than 30 seconds late is
skipped, unless the schedule was created with `"misfire_policy":
{"type": "catch_up_within", "window_seconds": N}`.

**Scope.** Schedules belong to the workspace: an agent's
`meerkat_schedule_list` shows every agent's, and an agent can target
another agent's session (reviewed like any change). `/schedules` shows
only those that target your agent.

### Tools an agent reveals

Every tool definition rides on every model request, so two families
stay out of an agent's requests until it asks for them: the eight
schedule tools (about 38 KB, roughly 9k tokens) and the engine's 17 mob
tools (`council`, `mob_create`, `mob_spawn_member`, `mob_profile_*` and
the rest). A fresh conversation's request carries about 56 KB of tool
definitions instead of 122 KB. Once revealed, a family's definitions
travel with every request of that conversation, which a model with a
small context window feels; searching for one tool by name reveals only
that one.

`tool_search` takes free text (every word must match a tool's name or
description), a family (`schedule` or `mob`), or both, and reveals the
matches from the agent's next step on. It is tier 1 in every mode; a
revealed tool's calls are judged as that tool. A conversation keeps what
it revealed across restarts, `/model`, compaction and `/resume`
(`tui_tools.json` in the workspace's state); `/new` starts with nothing
revealed. Helpers have no `tool_search` and no hidden families.
`/context` shows what the requests carry and what is hidden.

### File watches

`watch_paths {"patterns": ["src/**"], "once": false}` asks to be told
when workspace files matching the globs change; `unwatch` (a watch's
`id`, or none for all) drops watches. Both are tier 1. The daemon
watches the files, not the agent's sandboxed shell.

Changes are batched and reach the agent as one message (`files changed:
a, b and 3 more`): a running agent reads it at its next step, an idle
one starts a turn for it, unattended if nobody is attached. So a watch
on an idle agent costs a model turn per batch. The TUI labels it `◉
files changed`; `/status` lists live watches.

A watch lives until the turn that made it ends; with `"once": true`,
until its first event instead (even past the turn). Changes made by the
agent's own `write_file` and `edit_file` are not reported back to it;
changes its shell makes are.
