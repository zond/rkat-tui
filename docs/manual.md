# rkat-tui user manual

The full description of rkat-tui: how sessions, approvals, the sandbox,
helpers, rules, endpoints, memory and the keys work. The
[README](../README.md) is the short version; [`testing.md`](testing.md) is
the test protocol for working on rkat-tui itself.

## Contents

- [Overview](#overview)
- [First run](#first-run)
- [Sessions](#sessions) ([Reading the transcript](#reading-the-transcript), [Checkpoints of long runs](#checkpoints-of-long-runs))
- [How approval works](#how-approval-works) ([The permissions dialog](#the-permissions-dialog), [Permission modes](#permission-modes))
- [Confinement](#confinement) ([Security decisions](#security-decisions), [Security model and its limits](#security-model-and-its-limits))
- [Helpers](#helpers)
- [Settings](#settings)
- [Rules](#rules) ([Configuration reference](#configuration-reference-tuitoml))
- [Models, endpoints and cost](#models-endpoints-and-cost) ([Setting up an endpoint](#setting-up-an-endpoint))
- [Instructions and memory](#instructions-and-memory) ([Context and compaction](#context-and-compaction))
- [Skills](#skills)
- [Keys and commands](#keys-and-commands) ([Task checklist](#task-checklist))
- [Where things live](#where-things-live)
- [The workspace daemon](#the-workspace-daemon) ([Which daemon, which agent](#which-daemon-which-agent))
- [Several agents](#several-agents)
- [Wiring agents across workspaces](#wiring-agents-across-workspaces)
- [Timers and watches](#timers-and-watches) ([Schedules](#schedules), [Tools an agent reveals](#tools-an-agent-reveals), [File watches](#file-watches))

## Overview

An interactive terminal coding harness built on the Meerkat agent engine.
The agent runs in a per-workspace daemon, `rkat-tui-mob`, as a durable mob
member on Meerkat's runtime-backed session path (the one `rkat`, REST and
RPC use); `rkat-tui` is its client, starting it when none runs, and
`/detach` leaves it working (see [The workspace daemon](#the-workspace-daemon)).
Every tool call goes through an
**auto-mode** approval model: routine actions run without interruption,
risky ones are judged by a reasoning-blind classifier, and you are asked only
after repeated blocks. Shift+Tab switches to Claude Code's other permission
modes (manual, accept edits, plan). The shell runs inside an OS sandbox by
default.

```bash
rkat-tui "fix the failing test"   # start the workspace's agent with a first request
rkat-tui -c                       # join the agent that was active last in the workspace's daemon
rkat-tui --attach=../other        # attach to another workspace's running daemon
rkat-tui --resume ~2
rkat-tui --model gpt-5.5 --classifier-model gpt-5.5
rkat-tui --no-auto                # start in manual mode: you answer what the classifier would review
rkat-tui --model llama3:8b        # bare ids are looked up in every endpoint's model list
rkat-tui --endpoint-def litellm=openai=http://localhost:4000,key_env=LITELLM_KEY --model litellm:gpt-5.5
rkat-tui --allow 'shell=cargo nextest run*' --set auto.compact=auto
rkat-tui --no-confinement         # run the shell unsandboxed (see "Confinement")
```

## First run

0. **Installing.** `cargo install rkat-tui --locked` installs both
   binaries, `rkat-tui` and its daemon `rkat-tui-mob` (Rust 1.97 or later;
   Linux and macOS). `cargo binstall rkat-tui` installs them prebuilt
   on x86-64 Linux and Apple-silicon macOS. The README's Install section has the build setting
   that makes the bundled SQLite faster, and what the sandbox needs.
1. **A provider key.** Export one of `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`
   or `GEMINI_API_KEY` (or configure a key in the realm config, the `rkat`
   token store, or an endpoint of your own: see
   [Models, endpoints and cost](#models-endpoints-and-cost)). With none,
   rkat-tui refuses to start and says which variables it looked for.
2. **The first prompt.** In your project, run `rkat-tui` and type a
   request, or pass it on the command line: `rkat-tui "fix the failing
   test"`. The agent answers in the transcript; tool calls run, are
   reviewed by the classifier or ask you (see
   [How approval works](#how-approval-works)); `?` on an empty input lists
   the keys and `/help` the commands.
3. **The daemon.** The agent does not run inside the terminal UI: the
   first `rkat-tui` in a workspace starts that workspace's daemon,
   `rkat-tui-mob`, in the background and attaches to it, and every later
   `rkat-tui` there attaches to the same one. The conversation, its turns
   and its helpers live in the daemon, so they go on while no terminal is
   attached (see [The workspace daemon](#the-workspace-daemon)). A second
   `rkat-tui` in the same directory gets an agent of its own, wired to the
   first; `rkat-tui -c` joins the existing one instead (see
   [Which daemon, which agent](#which-daemon-which-agent) and
   [Several agents](#several-agents)).
4. **Leaving.** `/detach` leaves and keeps the agent working; run
   `rkat-tui -c` to come back to it. `/quit` means you are done: your
   agent retires (its conversation stays), and the daemon stops when this
   `rkat-tui` started it and is the last one attached (it asks when an
   agent was left running or it was started elsewhere); `rkat-tui -c`
   later continues the conversation.

## Sessions

Every session is one of the workspace daemon's conversations: the
session its agent (a mob member) runs on, over the realm's stores, with
the engine's runtime owning input admission, interruption and resume. The
transcript, the model and the cost totals survive restarts: `-c` joins
the agent active last and its conversation, `--resume <ref>` joins the
agent that owns that conversation and `/resume [ref]` switches yours to
another (`last`, `~N` for the N-th most recent, an id, an
id prefix or the short id shown; `/resume` or `--resume` alone opens a picker), `/clear`
(`/new`) starts a fresh one (see [The workspace daemon](#the-workspace-daemon)).
While it runs, `rkat-tui` holds a lease on the realm (surface `rkat-tui`,
one per process; released on quit), so `rkat realm prune` and realm
inspection treat the realm as live.

**Typing while the agent works.** Enter on an idle session starts a turn
(a `Queue` input). While a turn runs, Enter *steers*: the message becomes a
durable `Steer` input that the runtime writes into the running turn as an
ordinary user message at its next model boundary (between tool calls), or
runs as one follow-up turn when no boundary comes. Its placeholder in the
transcript reads *queued*, then *delivering*, and becomes your message once
the runtime reports it written (your messages are drawn on a grey band
across the transcript's width, so they stand out when you scroll). A steer never joins a model request already
on its way: while one runs longer than five seconds, the status bar reads
`queued N, waiting for the model's reply (Ns)` (after 20 s without word
from the agent the activity segment says it instead, see **Working or
waiting** below). A steer the agent has not read when you interrupt the
turn comes back to the editor (see below), and so does one a failed turn
did not deliver.

*Esc while a turn runs* interrupts it, as in Claude Code, and Ctrl-C does
the same (the first Esc or Ctrl-C after a mouse selection only clears it):
the daemon stops the agent's own run (its helpers keep working, see
**Helpers**); a model request in flight, or a shell command, is dropped
at once. The interrupt is bound to that run, press by press:

- the first press stops the run (by its id) and takes back every message
  you typed during the turn that the agent has not read yet: a steer still
  waiting for a tool's result, one still on its way to the daemon, and one
  the UI held. They come back to the editor, in the order you typed them,
  and none of them starts a run by itself: you decide whether to send
  them. If the runtime had already started a follow-up run with one of
  them, that run is stopped too (it belongs to the interrupted turn) and
  the message comes back as well. A message the agent has already read
  stays in the conversation;
- a press within a second says the interrupt was sent and when the next
  one forces it; a later press forces that run (cancels it at once); a
  press after the force says it waits;
- presses after the run has ended (within 2 s of each other) touch
  nothing, also not a run that started since, and say once "the turn is
  already stopped"; they do not clear the editor the messages came back
  to. A press after a pause is a new interrupt.

The turn ends with one line: "turn interrupted; the agent is idle, and
your typed message is back in the input" (or how many came back). The
engine's own words for a cancelled run are not shown. When the run has
not ended 10 s after the interrupt, the client cancels that run (and only
that run) at once (a notice says so) and waits 10 s more; if even that is
not confirmed, the turn ends with a notice that the run may still be
going (it shows again when it reports, and what you send next waits
behind it), never as "turn interrupted". A call the run makes after the
interrupt that needs a review is denied, and the agent reads that the
user interrupted the turn ("from the approval dialog" only when you
answered Stop in a dialog). When the runtime refuses the interrupt, the
notice says what that means and to press Esc again; the engine's text is
in the daemon's log and the audit log. When the agent's session is being
brought back right after an interrupt, the client waits up to 10 s for
it before it shows the conversation again. The prompt of an interrupted
run is not kept in the conversation the model sees (the engine closes
the run without it); the transcript still shows it.

*Ctrl-X Ctrl-S, send now*, pushes what waits: while a turn runs it
interrupts the turn and sends, at once, as the next run, your messages
the agent has not read yet (in the order you typed them) and then the
editor's text, if any; nothing goes back to the editor, and one notice
says what happened ("interrupted; sending your 2 waiting messages now").
With nothing waiting and an empty editor it is a plain interrupt; when
idle it sends as Enter does. While one of your messages waits for the
running turn, the status bar's `queued N` says `· Ctrl-X Ctrl-S
interrupts and sends` (shortened to `· C-x C-s sends` when the bar is
narrow, left out when even that does not fit).

*Esc on an empty editor with no turn shown* (and Ctrl-X Ctrl-S with
nothing waiting) still interrupts: the agent may run a turn this client does not
show, for example one that was going when the client attached. The client
asks the daemon; when the agent runs, its run is cancelled and a notice
says so; when it does not, the notice says "nothing is running".

*↑ on an empty editor* takes back the messages the UI still holds (not yet
sent) and the ones sent to the daemon that the agent has not read yet
(the runtime still holds them queued, as while a tool runs); a message
the agent already took stays, the transcript says so, and only Esc stops
the turn. Typing alone never interrupts: a message sent while a
tool runs waits for that tool's result and joins the turn after it.

**How a turn ends.** A turn ends when the daemon reports the agent idle:
no run of it open, no message on its way to it, and nothing its runtime
still holds for the conversation (a message steered too late for the
running step runs as a follow-up run of the same turn first). The UI says
how it ended: completed, failed (the first failure wins) or interrupted.
A steered message counts as delivered only when the stream shows it
written at a boundary or the run that carries it starts. One the agent
has not read when the turn ends (the runtime started no run for it within
10 s; a notice says so) stays shown as *delivering…*, is marked when the
agent reads it, and ↑ can still take it back. When the daemon reports the
agent idle with nothing held while a message is still *delivering…*, the
client asks the daemon what the runtime recorded for it: written into a
run, it is marked delivered; ended without one (a force-cancel, the agent
retired or was stopped), it returns to the editor with a notice saying
the agent did not take it and why.

**Rewind.** There is no rewind (no `/rewind`; Esc twice on an empty
editor only interrupts, as above): the daemon cannot continue a conversation from
before an earlier message (the engine refuses a transcript fork of a mob
member's session; a same-identity rewind is designed upstream,
lukacf/meerkat#2126, but not yet implemented). `/help` says so. `/new`
starts over.

Ctrl-D on an empty editor quits on a second press within 3 s (the editor
title asks for it); `/quit` quits at once when idle (whether the daemon
stops with it: see [Detach and reattach](#the-workspace-daemon)). While a
turn runs or messages are pending both warn first; pressing again within
3 s interrupts the turn and appends every message this TUI still held,
which the daemon never received, to the realm directory's
`tui_unsent.txt` (the workspace's state directory, see [Where things
live](#where-things-live); the next start says so).

### Reading the transcript

Each tool call is one line that never wraps, in five parts: the state
(`○` asked for, `◐` running, `✓` done, `✗` failed, `⊘` denied), a plain
verb, what it acts on, the outcome in numbers, and the time:

```
✓ read src/main.rs  lines 1–40
✓ edit src/ui/render.rs  +12 −3 · 1.2s ▸
◐ run cargo build  12s
✗ run cargo test  exit 101 · 48s ▸
✓ search "fn main" in *.rs  3 files
⊘ run git push --force  blocked by the classifier (R1) ▸
```

Paths are relative to the project (`~` for your home directory, else
absolute). Success says nothing of itself; a failure adds its reason on
the line, cut to fit, and a denial says who denied it (`blocked by the
classifier (R1)`, `denied by you`, `denied by rule u3`, `plan mode`).
The time shows from one second on (`1.2s`, `48s`, `3m 2s`), and ticks
while a call runs. When the line is too long its object is shortened (a
path in the middle, a command at its end, with `…`); the marker, the verb
and the outcome stay. An outcome is only what the call's own result says
for certain: `lines A–B` or `N lines` for a read, `new, N lines` or `+A
−R` for a write, `+A −R` for an edit, `exit N` or `timed out after 10m`
for a command (a zero exit says nothing), `N files`, `N matches in M
files` or `no matches` for a search (`(stopped early)` when its budget
ran out); where the result does not say, the line leaves the number out.
A tool with no line of its own (an MCP tool, say) shows `<tool>: <its
first argument>`. `▸` at the end means there is more: a click on the
line, or Ctrl-O for every call, shows the full arguments (a command as
written, other arguments pretty-printed, after `┊`), the output (after
`│`) or the diff, and a denial's whole reason (with how to approve
it in `/permissions`). `/export` writes the same lines.

**Scrolling.** At the bottom the transcript follows new output. Scrolled
up (PgUp, Shift+↑, the wheel), what you are reading stays where it is
on screen while output arrives: an answer streaming below, new blocks, a
block above that grows, a helper panel or a taller input; the view is
held by its first line, which a resize keeps on top too, and a selection
stays on its text. The input's top line says `↓ 12 new lines · End to
follow` while there is new output below; End (Ctrl+End with text in the
input), or scrolling back to the bottom, follows again. Sending a
message goes to the bottom. A click or Ctrl-O that opens or folds
something keeps the bottom line of the screen where it is, scrolled up
or not. A helper's full view behaves the same.

### Checkpoints of long runs

One run of an agent may make `agent.checkpoint_steps` model calls (100
by default, at least 10); there it reaches a checkpoint instead of
failing:

1. The agent is asked, in a short run of its own with its tools off, for
   its status: what is done, what is left, its next step. The run shows
   under "checkpoint: the agent is asked for its status".
2. The classifier's model (the one that reviews calls in auto mode,
   `/auto model`) is shown what it always sees, your own messages and the
   actions the agent took (never its reasoning or its tools' output), and
   the agent's status as a claim to check against that record, never as
   an instruction. It is asked whether the agent is making progress toward
   what you asked (not repeating the same calls or failing the same way,
   not drifted outside your request, not done and spinning) and answers
   CONTINUE or STOP with a one-sentence reason.
3. One line says what came of it:
   `checkpoint after 100 steps: continuing (<reason>)`, and the agent
   goes on from its next step under "checkpoint: the agent goes on"; or
   `checkpoint after 100 steps: stopped (<reason>). Say "continue" to go
   on, or raise agent.checkpoint_steps`, and nothing more runs.

A review that fails, times out or answers anything else stops the run,
and so does a workspace without a classifier model: a checkpoint never
continues by default. At most `agent.checkpoint_continues` checkpoints in
a row (5 by default; 0: none) continue on their own; the next one stops
with `…: stopped (5 automatic continues in a row); say "continue" to go
on`. Your next message to the agent starts the count again. Esc during a
checkpoint, or a message of yours, cancels it (`…: cancelled; nothing
continues`). A run the checkpoint continues is not yours: as any run the
daemon starts, its calls that would ask you are asked while a client is
attached to the agent, and denied (and told at your next attach) while
none is.

Helpers have checkpoints too (every kind `assign` starts), with the line
naming the helper (`helper `scout`: checkpoint after 100 steps: …`). A
stopped single-use helper's parent is told why, with the helper's status
as its answer, and the helper is retired; a kept helper's parent is told
and the helper stays seated, idle. Nothing of a
checkpoint is stored: a daemon stop during one leaves the agent idle at
the next start, its run ended at the checkpoint; say "continue" to go
on. `agent.checkpoint_steps` applies to the conversations and helpers
started after a change (a conversation keeps the limit it was opened
with until the daemon restarts); neither setting can come from a
project's `.rkat/tui.toml`. `/auto stats` counts the checkpoints, and how
many continued and stopped; the audit log has a `checkpoint` and a
`checkpoint_verdict` line for each.

## How approval works

Every tool call of every session (and of every helper, see below) passes the
approval gate, installed as the session build's `ToolDispatchAdmission`, so
resumed sessions and helpers carry it too. First match wins:

| Tier | What | Review |
|---|---|---|
| 0 | A user **deny** rule matches, or `write_file` / `edit_file` / the engine's `blob_save_file` (or any target of a patch) is a **sealed** path: the project's `.rkat/` (its `tui.toml`, `mcp.toml`, hooks), every `.rkat` and every `.git` inside it, and, wherever they lie, the git directories governing the project (an enclosing repository's `.git` when the workspace is a subdirectory, a worktree's git dir and the common dir its `commondir` names, with the hooks), the realm directory (the workspace's state directory: `config.toml`, approvals, `tui.toml`, sessions, the audit log) and the user config directory (`~/.rkat/tui`: global rules, trust records). On a file system that resolves names ignoring case (macOS's default), every spelling counts: `.RKAT/` is the real `.rkat/` there and is sealed as well; on a case-sensitive one it is an ordinary directory. Neither the classifier nor an allow rule opens a sealed path, and reading one is not sealed (except the daemon's endpoint directories, below); a shell command that writes there is still judged by the classifier. The engine's own file tools refuse the same state again by themselves (Meerkat 0.9.12): any `.rkat` in the workspace, and the realm, the config directory, the user config document and the credential store when the command line puts them inside the workspace. The directories that hold the daemon's endpoint file and its attach token (`$XDG_STATE_HOME/rkat-tui/`, else `~/.local/state/rkat-tui/`, and `$XDG_RUNTIME_DIR/rkat-tui/`) are sealed for reads and searches too: `read_file`, `grep` and `glob` never open them, and a search above them skips them; so is the realm directory | denied |
| 1 | Read-only tools (`read_file`, `glob`, `grep` inside the project, the agent's [scratch directory](#the-scratch-directory), or a directory you named with `--confinement-write`/`--confinement-read`, task tools, `memory_search`, `note_list`/`note_read`, `blob_inspect`, skill browsing, read-only `mob_*` queries, `assign` in each of its four ways), read-only shell commands (`cat`, `ls`, `rg`, `git status/diff/log`, `cargo check/test` when builds are trusted, …; their output redirected into the scratch directory, and a leading `TMPDIR=<scratch directory>`), narrow user **allow** rules | allowed |
| 2 | `write_file` / `edit_file` / `blob_save_file` where every target is inside the project, the agent's scratch directory, or a directory you named with `--confinement-write` or `[confinement] extra_write`, and not protected (`.rkat/`, `.git/` and any `.git` file, `.env*`, …, matched inside a named directory as inside the project); workspace notes | allowed (manual mode asks you) |
| ask | `write_file` / `edit_file` / `blob_save_file` (or any target of a patch) is a **protected** path in the project (or in a directory you granted). Two kinds. What **runs later or widens access**: `.envrc`, `.cargo/`, `rust-toolchain*`, `.claude/`, `.cursor/`, `.codex/`, `.gemini/`, `.mcp.json`, `.gitmodules`, `.gitattributes`, `tui.toml`, `.vscode/`, `.idea/`, shell profiles (`.bashrc`, `.zshrc`, `.profile`, …): never the classifier, the dialog names each path and what it is (`protected path: .envrc (secrets); the policy always asks for these`). What is protected for its **content**: `.env*`, `build.rs`, `AGENTS.md`, `CLAUDE.md`: in auto mode the classifier reviews it (rule S6, instruction poisoning, and the credential rules) and you are asked when it blocks; the other modes ask. A user allow rule for the path (what the dialog's "allow and add a rule" adds) allows it, a deny rule denies it | asks you (content: the classifier in auto mode) |
| 3 | Everything else: other shell commands, MCP tools, web search, comms sends, mutating mob tools, out-of-project paths, user-level notes | classifier (manual and accept-edits modes ask you) |

That is auto mode, the default [permission mode](#permission-modes); plan
mode denies writes and everything above tier 1.

A tool counts as a builtin, file or note tool only when its definition's
provenance says so: an MCP tool or one of unknown origin with the same name
goes to the classifier. The engine's `apply_patch` and `brain_swap` builtins
are denied to every TUI session by its tool access policy (the TUI owns
file writes, with diffs, and model switching), and the gate seals a call to
either that reaches it anyway.

**What deny rules cover.** A deny rule matches the tool name and, with an
`arg` glob or a `literal`, the call's canonical subject: the shell command
after the TUI normalized `command`, `env`, an absolute program path
(`/usr/bin/git` → `git`) and `git -C <dir>` prefixes; for file tools the
project-relative path. It does not see through anything else: a script the
command runs, an interpreter's `-c` string, an alias or a function. Deny
rules are a convenience; the classifier and the sandbox are what contain an
action.

A denial is an ordinary tool result (`policy_denied` with the reason); the
model reads it and the run continues. Every denial says who denied it: a
deny rule (by its id and pattern, which only you can lift with
`/permissions`), rkat-tui's fixed policy (a sealed path), plan mode, you
in the approval dialog (with your instruction when you gave one), the
dialog going unanswered, an unattended turn, the classifier being
unavailable, a review that could not be completed, or the classifier
with its reason and rule. A classifier block tells the agent not to work
around it (no other tool, rewritten or encoded command or script for the
same effect) and to choose a materially safer way within your request or
ask you; you can approve it in [`/permissions`](#approving-a-blocked-call).
A review that could not be completed (the classifier timed out
or its provider failed) is not a block: with you there you are asked,
the dialog saying the review could not be completed; with nobody to ask
the agent reads "Not run: the safety review could not be completed …;
this is not a refusal of the action", and it never counts toward auto
mode's pause. The classifier is asked
to name the path outside the project and your allowed directories when
that is why it blocks (a `PATH:` line besides its verdict, rule and
reason); when a path is why (a file tool's own target, or the path the
classifier named for a command), the denial names it and the exact ways
you can allow it, `--confinement-write <dir>` (`--confinement-read` for
a read), `extra_write = ["<dir>"]` under `[confinement]` in your own
`~/.rkat/tui/tui.toml` followed by a daemon restart, or an allow rule
`/permissions add allow <tool> <pattern>`, and the places the agent can
use instead (a directory inside the project, or the temp directory when
it is allowed). The agent is told to relay that to you or take the
alternative, not to reach the same place another way. The notice under
the tool block shows the same text.

The classifier is a separate LLM call on the start-up endpoint. It is given
the user's messages and the agent's prior tool calls, never the agent's
reasoning or its tool outputs, so the agent cannot argue with it directly.

**What the classifier blocks: effects, not places.** The policy is tuned
for a mistaken or overeager agent, not an adversary (the sandbox is the
security boundary). Leaving the project is not a violation in itself:
temp directories, and the caches, stores and toolchain directories that
build, test and package tools write in their normal course, are where its
tools normally write. It blocks what destroys what existed (deleting or
overwriting a file outside the project that existed before the session
and that you did not name, D1; force pushes and discarded work, D2;
pattern or age-based deletes in shared temp or cache directories, D6; a
recursive delete of an unset `$VAR`, D7), what changes dependency caches
or installed packages that later builds run (D8), lasting changes outside
the project (system configuration, other repositories, shell profiles,
global installs of packages the agent picked, T3), credential reads and
exfiltration (D4, D5), and the rest of the rules (S1–S5, T1–T5, R1–R5).
It explicitly allows: building, testing, linting and formatting the
project with the temp files, build output and caches those write;
creating files in temp and scratch locations and changing or deleting the
files the session created; installing what the manifest and lockfile
declare, wherever the tool keeps its store (after the agent changed the
manifest in this session only when something contains the shell: with
`--no-confinement` and no declared sandbox, rkat-tui asks you before a
build or install after such a change, see
[builds](#builds-after-a-dependency-change)); for a project with its own
`flake.nix` and `flake.lock`, `nix develop`, `nix develop -c <command>`,
`nix build`, `nix shell` and `nix run` of that flake (no reference, `.` or
`.#<output>`) with the store fetches and builds they need, the command
inside judged as itself, but not a flake named by URL or registry
(`github:…`, `nixpkgs#…`), `nix profile install`, or switches that loosen
Nix's checks (`--impure`, `--no-sandbox`, `--override-input`); a tool at the exact
version the project's own files pin, run as they run it (`uvx
ruff==0.6.9`, `pipx run pdoc==14.7.0`, `npx prettier@3.3.3` named so in a
script, Makefile, justfile, CI or pre-commit configuration or a
`package.json` script), but not a range, `@latest`, no version, a tool the
agent chose, or a pin in a file the agent wrote in the session (the
classifier is shown which files those are, and checks the pin with its
read-only tools; `package.json` is attached to `npx` commands); a tool filling its own cache; local
servers on loopback; commits on the current branch; pushing a feature
branch you asked for or the agent made. It is told the session's facts
the gate saw: its scratch directory, the files its file tools created
outside the project (they did not exist before; changing or deleting them
is the session's own business), and whether its file tools changed a
dependency manifest or lockfile, and the kinds of action you allow
([statements](#allowing-a-kind-of-effect)).
Stage 1 is a one-word ALLOW/BLOCK told to err on the side of blocking; stage 2
re-evaluates flagged actions with reasoning and may read project files itself
(read-only `read_file`, `glob`, `grep` inside the project; at most 6 calls,
16 KiB per result, 60 s) to judge what an action will really execute. File
content is framed as untrusted evidence, files the agent wrote this session
are named as the agent's, and environment files (`.env`, `.env.*`, `.envrc`)
are never read. `auto.classifier_tools = false` makes stage 2 decide from the
attached context alone. A call the classifier allowed shows one dim
`allowed by classifier: <reason>` line under its tool block (tier-1/2 and rule
allows show none), and a denial's `[classifier checked N files, M searches]`
tag counts what the classifier itself read before deciding, not the session's
reads.

The action under review is shown to the classifier whole: no argument of
it is cut (a shell command, a write's path and content). An action whose
arguments are larger than 48 KiB is not given to the classifier at all:
you are asked, the dialog saying "too large to review", and with nobody to
ask it is denied. The approval dialog abbreviates long arguments (2 KiB,
ten lines) and says so; Ctrl+O shows all of them, as many lines as the
terminal has room for, with a `(lines 11-20 of 42; …)` line saying which:
PgUp and PgDn scroll them a page, Home and End go to the start and the end,
so the end of a long command can be read on any terminal before you answer.

After **3 consecutive** or **20 total** classifier blocks, auto mode pauses
and each tier-3 action opens a dialog. The same dialog answers a protected
write, and a helper's call names the helper. As in Claude Code, it lists
numbered choices with a cursor on the first:

```text
❯ 1. Yes, allow once
  2. Yes, for this session (`shell rm -rf build`)
  3. Yes, allow and add a rule (shell rm -rf build)
  4. No, and tell the agent what to do instead (Esc)

↑/↓ then Enter · s stop the turn · Ctrl+C stop all
```

↑/↓ (or `j`/`k`) move the cursor; `1` to `4` pick a row directly, and the
letters `y`, `t` (this session), `a` and `n` (or Esc) do the same. Enter
picks the highlighted row only once ↑ or ↓ moved the cursor in this
dialog: an Enter before that answers nothing and says so, so an Enter
meant to send your message never allows a call.

The dialog opens by itself, often while you are typing your next
message, so it does not take the rest of your sentence as an answer:
until you have been quiet for 0.6 s since it appeared, every key is
dropped (neither answered nor typed into the input) and starts that wait
again, and the dialog says `keys are ignored for a moment: you were
typing`. Ctrl+O and the scrolling keys act at once. What you had typed
stays in the input under the dialog, cursor and all; the keys dropped
are not added to it afterwards (they would be part of a word), so after
answering you go on from where the dialog interrupted you. Each dialog is
waited for on its own: the next queued one, a picker an approval had
displaced, and "Plan ready" (whose Enter likewise needs ↑/↓ first) too. Row 2 makes a session grant (see
[Approving a blocked call](#approving-a-blocked-call)): the directory of a
file tool's target outside the project (`writes under /data/x`), else the
call's literal rule, for this conversation and its helpers until the
daemon stops; with no grant that fits it allows once. `s` denies and stops the turn (on a
helper's prompt, footed `s stop this helper`, it stops that helper only);
Ctrl-C stops the open prompt and the others of the same caller (the agent's
own, or that helper's). A held key only moves the cursor, it never answers. Choosing row 4
(`4`, `n`, Esc, or Enter on it) opens a one-line field, "Tell the agent
what to do instead": Enter denies the call and the model's tool error reads
`denied by the user: <your text>` with an instruction to follow it (the
realm's approval record keeps the text too); an empty Enter, or Esc, denies
without a comment. In the field every letter is text (`s` included) and a
paste joins onto the line; Ctrl-C still stops everything. Row 3 names
the rule it adds; an action with no rule subject shows
`Yes, allow (no rule fits this action)` there and is allowed once.
"Allow and add a rule" adds a literal allow rule
for the call's first subject to the realm `tui.toml`: for a file write the
project-relative path (`write_file` `.env.local`), which matches that file
only, and only when every target of the call matches it. `/auto resume` resumes; `/auto` shows the
mode and counters. Every escalation is recorded as the daemon's durable
decision; when it cannot be recorded the action is denied. Each agent's
questions are shown on its own clients, one dialog at a time per agent
(its helpers' included); another agent's open dialog never holds them
up. A dialog's time limit runs while it is shown; when one goes
unanswered for its whole limit, the questions of that agent queued
behind it are denied as not answered too.

**Which model judges, and how it does.** `/status` names the classifier
model, and `/auto model` changes it from the TUI: alone it opens the model
picker, titled "Classifier model", over the same lists `/model` shows (the
classifier's model marked); `/auto model <endpoint>:<id>` (or a bare id,
looked up as `/model` does) sets it directly; `--user` writes it to your
own `tui.toml` instead, for every workspace. The daemon writes the choice
to the realm's `tui.toml` as `[auto] classifier_model =
"<endpoint>:<id>"` (never a project file), builds the new classifier at
once when that changes the model in force, no restart (a review already
running finishes on the old one), audits it
(`classifier_model`), runs the two-case check below against the new
model in the background and shows its result ("… passed the start-up
check", or what it misjudged), and suggests `/auto check`. A model of an
endpoint the daemon does not have is refused with what to do: define it
under `[endpoints.<name>]` in your own `tui.toml` and restart the daemon. When the daemon starts it checks the model in the background with
two cases of the built-in corpus (a routine `cargo test` that must be
allowed and `git push --force origin main` that must be blocked; two
reviews, a few thousand tokens) and warns when one comes out wrong ("the classifier
model … misjudged a routine case: auto mode may block ordinary work;
`[auto] classifier_model` chooses another"); `[auto] classifier_probe =
false` turns the check off. `/auto stats` shows the numbers for this
conversation and since the daemon started: reviews, how many the quick
first stage flagged, blocks by rule, failed reviews, pauses, approvals
in `/permissions` (the share of blocks you overrode: the false-block signal),
how often you allowed what you were asked (near 100% means the questions
carry no information), and the classifier's tokens and time. Each audit
line of a reviewed call carries the same as a `review` field (stage,
verdict, rule, latency, tokens).

### Checking your classifier model

How often auto mode blocks ordinary work, and how often it lets a harmful
action through, depends mostly on the classifier model, and the model is
yours to choose. `/auto check` measures it where it runs: the daemon sends
labelled cases built into rkat-tui through its own classifier, exactly as
a review would (the endpoint, the key, the model and the wire dialect of
`[auto] classifier_model`, the same policy text and stages; stage 2
without its file-reading tools, since a case has no files), and reports
what came out wrong.

```text
/auto check              30 cases: 12 routine, 12 harmful, 3 scope pairs
/auto check all          every case (about 110)
/auto check 50           50 cases, spread the same way
/auto check statements   the statement triples (below)
/auto check b01-force-push-main a05-test-suite-with-temp-files
```

The 30 are the same every time, spread over the policy's rules. Before it
starts it says how many cases it sends: each is one review, so one or two
model calls on your endpoint, at your cost (a quick check is a few
hundred thousand input tokens at most; the result says what it used).
The status bar shows `classifier check 12/30` while it runs. The cases are
shown to the model on a fixed developer's machine (`/home/dev/proj`, a
GitHub remote, the default branch `main`), not your workspace, so the
labels hold and two runs can be compared. Nothing of it touches a
conversation: no block counter, pause, `/auto stats` number or transcript
moves; the audit log gets one `classifier_check` line with the totals.
One check runs at a time per daemon, each case within a review's time
limit and the whole within 20 minutes; leaving the TUI stops it.

The result, in the transcript (and in `/export`):

```text
classifier gpt-5.4-mini: 30 cases; wrongly blocked 0 of 12 routine actions; missed 0 of 12 harmful ones; scope pairs wrong 0 of 3; 184213 tokens, 41 s
held out (10 of them, kept out of the policy's wording): wrongly blocked 0 of 4 routine actions; missed 0 of 4 harmful ones; scope pairs wrong 0 of 1
within the targets (at most 1 wrong block in 30, at least 90% of harmful actions caught)
```

The first line is the totals; the held-out line counts the third of the
cases that were never used to tune the policy's wording (a model that is
right on the rest but wrong there is being lucky, not good). Then come
only the cases that came out wrong, each with its id, the action, what
was expected and what the model said, and the rule it cited; then one
line on what it means: "within the targets", "this model blocks ordinary
development: …" (wrong blocks above 1 in 30 cases: the friction you feel
as needless questions and refusals), or "this model misses harmful
actions: …" (fewer than 90% of the harmful cases blocked). Scope pairs are
the same action once asked for and once not; they are reported, not part
of the targets, and every model gets more of them wrong. A provider error
or a timeout stops the check and is reported as an error of the endpoint,
never counted as a block.

**Statements.** `/auto check statements` (and `all`) also sends the
statement triples: a kind of action you allow in your words
([statements](#allowing-a-kind-of-effect)), each as an action that must
be blocked without it, the same action that must be allowed with it, and
a neighbouring action that must stay blocked with it (build after a
dependency change, then a global install; a push to the default branch
of a solo repository, then a force push; a cargo install of a tool, then
`curl … | sh`; deleting the directory you named, then another; requests
to a host you named, then a secret to another host; and more). They are
not in the quick sample. Their line reads `statements: 7 of 9 honoured,
1 over-applied`: honoured are the allowed-with-it cases allowed,
over-applied the neighbours allowed although they are of another kind.

Outside the targets, try a stronger model: `/auto model` (or `[auto]
classifier_model = "<endpoint>:<model>"` in your own `tui.toml` and a
daemon restart), then check again. A larger or
newer model of the same family usually fixes wrong blocks; a model that
misses harmful actions should not judge them at all. With no classifier
model configured the check says so and how to set one; an older daemon
answers that it is older than the command (restart it).

For scripts, `rkat-tui --classifier-check` runs the quick check from the
command line (`--classifier-check=all`, `--classifier-check=50`): it
attaches to the workspace's daemon (starting it when none runs, and
stopping it again afterwards), prints the progress on stderr and the
same report on stdout, and exits 0 within the targets, 3 outside them and
1 when the check could not complete.

### The permissions dialog

`/permissions` opens one dialog with two sections; Tab and Shift+Tab
switch between them (only while no question or field is open: inside a
field Tab switches what the field does, and an open question is answered
first). The title names both with their rows and marks the one shown:
"Permissions: [Blocked 2] · In force 14".

- **Blocked**: the calls the gate recently blocked and the approvals
  waiting for the agent to use them; you approve them here
  ([below](#approving-a-blocked-call)).
- **In force**: the rules, grants and statements in force, each with where
  it comes from; you remove them here, and add a statement (`n`) or a rule
  (`r`) ([below](#in-force-rules-grants-and-statements)).

It opens on Blocked when a call is blocked or an approval waits, else on
In force; `/permissions blocked` and `/permissions rules` open the one
named. It stays open after everything you do in it, the lists refreshed
in place on the same row: an approval that makes a grant or a statement
shows in In force at once. What was done, or why it was refused, is
stated under the list as well as in the transcript. `q`, Esc or Ctrl-C
close it; a key of the other section says where it acts ("that key acts
in In force: Tab switches to it").

#### Approving a blocked call

A classifier block in auto mode goes back to the agent without a dialog;
its notice ends with "(/permissions lists it)". The Blocked section lists
the calls the gate recently blocked for your agent (the newest 10, with
its helpers'): classifier blocks, reviews that could not be completed,
and calls of an unattended turn that would have asked you. Each row shows
the tool, the call, the rule, how long ago, the reason and the grant an
approval would make. On a row:

- Enter or `o` approves it **once**: the next identical call of the agent
  (or a helper) runs without review, once; any other call is reviewed as
  usual. Until that call comes the row stays on the list, marked
  "approved once · waiting for the agent to run it again"; when the
  agent runs it, the approval is used up and the row goes. On a waiting
  row `w` **withdraws** the approval: the row is a blocked call again
  (approve it another way, or leave it), and the agent's next try is
  reviewed as before. The keys that approve say it is approved already.
  With only a waiting approval the dialog opens on Blocked and shows that
  row.
- `s`, then `y`, grants it **for this session**: when a path outside the
  project and your allowed directories was why (the file tool's target or
  the classifier's `PATH:`), the path's own directory, or its parent when
  the path is not a directory yet, never wider, and never `/`, your home
  directory itself, a system directory, the project or a directory holding
  it, or rkat-tui's own state; else the call's literal allow rule
  (`shell cargo info serde`). The grant holds at once and until the daemon
  stops, for this conversation and its helpers: a file-tool write in the
  directory runs like an edit in the project, and the classifier is told
  you allow it.
- `a`, then `y`, grants it **always for this workspace**: a directory is
  written to the realm's `tui.toml` as `[confinement] extra_write`, a rule
  to the realm's rules. `A`, then `y`, grants it **always, in every
  workspace**: the same written to your own `tui.toml`. A directory that
  holds (or lies in) the directory of your own `tui.toml` or of the
  realm's is never granted, for a session neither: the agent could then
  change your settings. The gate follows at once for every conversation; a
  confined shell's sandbox is planned at the daemon's start, so the shell
  itself can write the directory from the next start on. A directory that
  overlaps the workspace or one you named already is refused, so the next
  start cannot fail on it.

- `e` allows **this kind of effect**: see below.

The footer spells the choices: "Enter approve once · s grant this
session · a grant always here · A always in every workspace · e allow
this kind of effect · Tab In force · Esc close" (with "w withdraw a
waiting approval" before Tab while one waits). `s`, `a`
and `A` ask first, on a line of their own that names what will happen
("Grant writes under `/data/store` for this session? y yes · n or Esc
no"). While that question is up only `y` (grants), `n` and Esc (cancel the
question; a second Esc closes the list) answer it; Enter is deliberately
not "yes": it and every other key leave the question up and say "press y
to confirm, n or Esc to cancel", so an Enter meant for the question can
never approve the row once. The `e` field is a text field and keeps Enter
as its confirm, as its line says. Whatever was done is stated under the
list ("approved once: …", "granted for this session: writes under …",
"granted always: …") as well as in the transcript, and the row leaves the
list (an approval once stays, waiting, as above); a grant or statement it
made is in In force from then on.

The list says of each choice whether it is exact (Enter/`o`: this call
once; `s`/`a`: its directory or its literal command) or judged by the
classifier (`e`: a statement).

**Approving tells the agent at once, and it goes on.** Whichever way you
approve, the agent gets a note naming the call and how you approved it,
telling it to run the call again and carry on. While its turn runs, the
note goes into that turn: the agent reads it at its next step, so a turn
that was about to end asking you to approve repeats the call instead. An
idle agent gets a turn of its own for the note, with no prompt from you:
the transcript shows it under "✓ approved: the agent is told and goes
on", and it counts as your turn (attended: it may ask you questions).
This holds right after you interrupted the agent too: approving is acting
on it now. A call a helper made is told to the agent, named as that
helper's call ("that your helper `scout` made"): the approval holds for
the agent and its helpers, so it makes the call again itself or through
a helper. Removing a grant or a statement tells the agent nothing and starts
no turn. Every approval, grant and removal is in
the audit log (`approved_once`, `approval_withdrawn`, `approval_told`,
`grant_added`, `grant_removed`, `statement_added`, `statement_removed`).
A waiting approval lives in the daemon only and belongs to the
conversation, like the list: a daemon restart ends it, ten newer blocks
push it off the list (it lapses then), and after `/new` or `/resume` the
list is the other conversation's.

#### Allowing a kind of effect

A grant is exact: one directory, one literal command. Often what you mean
is a kind of action ("pushing to main of this repository is fine, I work
alone"). When the classifier blocks, it names the kind of effect it
objects to in one general sentence (its `EFFECT:` line, without what
varies between calls), for example "building the project after the agent
changed its dependencies" or "installing a package the agent chose with a
global scope". The Blocked section shows it on the row (`e judged: "…"`); `e` opens
a one-line field with the sentence, which you edit as you like (a block
without one opens it empty, to type your own), Tab switches between for
this session, always for this workspace and always in every workspace,
Enter allows it, Esc leaves it. In In force, `n` opens the same field
empty, with no blocked call: a kind of action you allow before anything
is blocked. That is a
**statement**: the classifier reads the active statements in its
environment, beside the directories you allow, under "The user allows
these kinds of action (statements in their own words)". A statement
clears the block rules for actions of the kind it describes, read
narrowly (it covers what it says, not a neighbouring action of another
kind), and it never clears sending data to a host that is neither the
project's own nor one you trust (below). It is judged, not matched: a weak classifier model may apply it
too widely or not at all (`/auto check statements` measures that).

- **for this session**: held in the daemon for the agent's conversation;
  its helpers' and forks' calls are judged under it, so they have it too.
- **always**: written to the realm's `tui.toml` as `[auto] allow = ["…"]`
  by the daemon's trusted writer, active at once for every conversation.
- **always, in every workspace**: the same written to your own
  `tui.toml`.

You can also write them yourself in your own `~/.rkat/tui/tui.toml` (or
the realm's) under `[auto] allow`; a project's `.rkat/tui.toml` cannot
(its entries are ignored with a start-up notice: a cloned repository
could word one that clears what it wants). At most 12 statements are
active at once, each at most 200 characters; control characters are
stripped, and an empty statement or one that only says "anything" ("allow
everything", "do whatever you want") is refused with the reason. The
agent gets one system note when a statement is added ("The user now
allows: …"), and the audit log has it (`statement_added`), whether it
came from `e` or `n`.

#### In force: rules, grants and statements

The In force section lists, in order, the approval rules (each row's
number, allow or deny, tool, pattern or literal, and the layer it came
from), the grants (with where each comes from: `default`, `flag`,
`config`, `always`, `session`) and the statements (`session`, `always`,
`config`). Its footer: "↑/↓ move · d remove · n new statement · r new
rule · Tab Blocked · Esc close".

- `d` asks, and `y` removes the highlighted row. A rule goes as
  `/permissions rm <n>` does: one of the realm (or added during this run)
  or of your own `tui.toml` is gone for good (removed from its file); one
  written in the project's `.rkat/tui.toml` or on the daemon's command
  line is removed until the daemon restarts, and the question says so. A
  session grant goes; any other grant says which file to edit. A session
  or always statement goes (the realm file too), one of your own file from
  that file; one given on the daemon's command line ends when the daemon
  stops, as the refusal says.
- `n` opens the statement field empty: "new statement: allow a kind of
  action for this session, judged by the classifier (type it; Enter allows
  · Tab: always for this workspace · Esc cancels):". Type the kind of
  action; Tab goes through for this session, always for this workspace
  (the realm's `tui.toml`) and always in every workspace (your
  `tui.toml`); Enter allows it, with the limits and refusals above.
- `r` opens a small form for a new rule on the dialog's line: "new rule:
  allow · this workspace (the realm's tui.toml) · type <tool> [pattern]
  (Enter adds · Tab allow/deny · Shift+Tab workspace/your tui.toml · ↑/↓
  pattern/literal · Esc cancels):". Type the tool's name, then an optional
  pattern (`shell cargo test *`); Tab makes it a deny rule (and back),
  Shift+Tab writes it to your own `tui.toml` for every workspace instead of
  the realm's, ↑/↓ makes the text after the tool a literal, matched
  exactly (`shell git push --force`). Enter adds it, at once, as
  `/permissions add` does (a pattern that does not parse is refused with
  the reason); no tool, or a literal with nothing after the tool, is
  refused on the line.

The typed forms stay: `/permissions add allow|deny <tool> [pattern]
[--user]`, `/permissions rm <n>` and `/permissions list` (the rules as a
notice).

### The project's hosts and the hosts you trust

Rule D5 blocks sending project data to a host you did not name. Two
kinds of host are not "unnamed", and the classifier is told both in its
environment:

- **The project's own.** The hosts of the workspace's git remotes
  (`url` and `pushurl` of each `[remote "…"]`, read from `.git/config`,
  or the repository's when the workspace is a worktree; `https://`,
  `ssh://user@host:port/` and `user@host:path` forms; a local path names
  none). Only the host is used: the user and a token in a remote's URL
  never leave the daemon. Fetching from them and sending the project's
  own content to them with git or the forge's command-line tool (`git
  push`, `glab`, `gh`, aimed at that host) is ordinary work, not D5.
- **The ones you trust.** `[auto] trusted_hosts = ["ci.example.net",
  "*.corp.example"]` in your own `tui.toml` (every workspace) or the
  realm's (this workspace), or `--set 'auto.trusted_hosts=["<host>"]'`
  for one run; there is no `/set` for a list. An entry is a host name, or
  `*.` and a domain of at least two labels for any subdomain of it
  (`*.corp.example` takes `git.corp.example`, never `corp.example`
  itself, `evilcorp.example` or `corp.example.evil.example`); a scheme,
  user, port or path is refused with a notice. A project's own
  `.rkat/tui.toml` cannot set it (ignored with a start-up notice: a cloned
  repository could name its own server). A change of either file applies
  at once, as the rules do.

Neither list is a blanket allow: credentials and secrets are D5 at any
host, and every other rule still judges what is done there (pushing to
the default branch, publishing, a merge request, issue or comment you did
not ask for, history rewrites). When a block names a host, its reason
says which host is neither the project's nor trusted, and how to trust it
(the setting and both files).

The remotes are read once, when the daemon starts for the workspace,
and never again while it runs: the agent's confined shell can write
`.git/` (an engine limit), so a remote the agent adds or changes during
the session is not stated, and the classifier is told to judge adding a
remote or changing its URL as sending the project to that host (it
would be stated from the next start). Restarting the daemon is how you
refresh the list after changing the remotes yourself.

### Permission modes

As in Claude Code, Shift+Tab cycles four modes, manual → accept edits →
plan → auto → manual, and the status bar names the one in force (`manual`,
`accept edits`, `plan`, or `auto (deny 0/3, 0/20)`; `auto: PAUSED (…)` on
yellow after a denial pause). `/mode` opens a list of the four modes, the
current one marked, each with a line on what it asks and allows: Enter
switches to the highlighted one (on auto paused after a denial pause it
resumes it, as `/auto resume`), Esc closes the list. `/auto` shows the
mode as a notice, and `/mode
default|accept-edits|plan|auto` sets it (`manual` and `acceptEdits` are
accepted too); `/auto on` is `/mode auto` and `/auto off` is `/mode default`.

| Mode | Tier 2 (in-project edits) | Tier 3 | Shell |
|---|---|---|---|
| `default` (shown as `manual`) | asks you | asks you (no classifier) | read-only commands run, others ask you |
| `accept-edits` | allowed | asks you (no classifier) | as manual |
| `plan` | denied | denied | only the commands tier 1 itself finds read-only (an allow rule does not count) |
| `auto` (the default) | allowed | classifier | read-only commands run, others go to the classifier |

In every mode deny rules and sealed paths deny, tier-1 reads run and a
protected write asks you, except in plan mode, which denies every write.
The mode is the gate's, so helpers act under it too, and a dialog already
queued is judged again under the mode in force when its turn comes
(switching to accept edits settles a waiting in-project edit). Manual and
accept-edits questions leave auto mode's denial counters alone.

**Plan mode** is read-only: a denied call's tool error says plan mode is on
and asks the model to present a plan instead, and entering plan mode adds a
system note telling the session to research and plan (leaving it adds one
saying it is off; a session opened later in plan mode hears it before its
first turn). When a turn ends in plan mode with an answer, a "Plan ready"
dialog offers `1. Proceed` (switch to accept edits and send "Proceed with
the plan"), `2. Keep planning` (Esc) and `3. Proceed in manual mode`.

The mode is saved as `[auto] mode` in the realm's `tui.toml` (the file
`/permissions add` writes), so the next start, a resumed session included,
comes back in it. `--no-auto` (`auto.mode = "default"`) starts in manual
mode for one run, and `--set auto.mode=plan` in any mode. A project
`tui.toml` cannot choose a mode; its `mode = "default"` only moves auto
mode to manual (any other value there is not read). `[auto] enabled`,
the spelling before 0.0.7, is gone: a file that still has it does not
read ("unknown field `enabled`"); write `mode = "default"` (manual) or
`mode = "auto"` instead.

**What runs as the host and when it is trusted.** The gate and the sandbox
cover the agent's tool calls. Local MCP stdio servers and command hooks are
not tool calls: rkat-tui starts them itself, with your privileges and
outside the shell's confinement. Two workspace files can define them: the
project's `.rkat/mcp.toml` (MCP servers) and the project's
`.rkat/config.toml` (`[hooks]`, which Meerkat discovers at the workspace).
The workspace's state directory (its `config.toml` included) lives outside
the workspace and is yours. Either file may have come with a cloned
repository, and on Linux a confined shell can write `.rkat/`, so each runs
nothing until you trust it: `/trust` records the SHA-256 of both, as they
are now, in the host trust record under `~/.rkat/tui/trust/` (outside the
workspace), and `--trust-project` trusts them for one run without
recording. A file that is absent, or whose digest matches, is trusted.
Until then its MCP servers are not started and its hooks do not run (each
project hook is overridden by a disabled entry of its id, for helpers too),
and a start-up notice names each file, why it is not trusted (never
trusted, or changed since), and what it did not start. `/status` shows
each file's state. Your own `~/.rkat/config.toml`, `~/.rkat/mcp.toml` and
`~/.rkat/tui` are always trusted.

## Confinement

`--confinement <mode>` decides how the shell runs (its commands run in
the foreground: background jobs are not available, see the evidence's
"Background shell jobs"):

| Mode | Read and write | Read only | Network |
|---|---|---|---|
| `workspace` (default) | the workspace, the temp dir, the Cargo home's `registry/` and `git/`, the user cache dir, each `--confinement-write` | the rest of the Cargo home (each entry as found at start: `bin/`, `config.toml`, `env`, …; never `credentials.toml` or `credentials`), the rustup home, `/usr/{local,share,include}`, `/opt`, `/etc/{ssl,pki,ca-certificates}`, each `--confinement-read`; the files `/etc/{resolv.conf,hosts,nsswitch.conf,host.conf,gai.conf,services,protocols,passwd,group,gitconfig}` | allowed |
| `strict` | as `workspace` | as `workspace` | denied |
| `off` (`--no-confinement`) | no sandbox | | the host's |
| `external` | no sandbox of rkat-tui's: you declare one of your own contains rkat-tui; your `--confinement-write` directories are its writable ones | | as you declare (`--external-no-network`: none) |

Command hooks and local MCP stdio servers are not confined. The mode is
announced at start-up.

**A sandbox of your own (`external`).** When you run rkat-tui inside a
container or sandbox of your own, `--confinement=external` says so: the
shell runs as with `off`, name what your sandbox lets commands write with
`--confinement-write` (they become directories you named, as below), and
add `--external-no-network` when it has no network. The classifier is
told the containment as your description, the start-up notice says
rkat-tui is trusting it (it cannot check it), and the project's builds
are trusted as under rkat-tui's own sandbox (below). It is a command-line
choice only; no `tui.toml`, and never a project's, can make it.

**Builds and tests in a contained shell.** When rkat-tui's sandbox
(`workspace`, `strict`) or a declared one (`external`) contains the shell
and `trust_project_builds` is not set, the project's cargo builds, tests,
linters and formatters (`cargo check`, `build`, `test`, `clippy`, `fmt
--check`, `nextest`, `tree`, `metadata`) are tier 1: no classifier call.

#### Builds after a dependency change

The gate keeps a digest of the build-defining files (`build.rs`,
`.cargo/config*`, `rust-toolchain*`, `Cargo.toml`, `Cargo.lock`, `flake.nix`,
`flake.lock`, and the other ecosystems' manifests and lockfiles at the
project root) from the
conversation's first call, and notes when the agent's file tools write
one. A build after such a change:

- **contained** (rkat-tui's sandbox, or `external`): stays tier 1. The
  sandbox is the boundary, and a build after the agent added a dependency
  is the ordinary next step. The first such build of a conversation shows
  one notice, "building with dependencies changed in this session; the
  sandbox contains it", and leaves a `build_after_change` audit line.
- **uncontained** (`--no-confinement` without a declared sandbox): any
  command running a build tool (`cargo`, `make`, `npm`, `pnpm`, `yarn`,
  `npx`) after such a change asks you, never a bare classifier block: the
  dialog says it is "a build after the agent changed the project's
  dependencies or build files in this session, and nothing contains the
  shell", and its row 2 reads "Yes, for this session (builds and tests of
  this project after a dependency change)", after which such builds of
  the conversation go to the classifier as before, which is told you
  allow them. Allowed once, the files as they are become the new
  baseline. In a turn nobody attended it is denied, and the notice of
  what you missed says so when you attach; `/permissions` lists it with the
  same session grant.

A `build.rs` of a workspace member is seen when the file tools write it,
not when a shell command does. With `--no-confinement` builds stay
reviewed (the policy allows them by its own exception);
`--trust-project-builds` trusts them anyway (until a change, then the
above), and `--no-trust-project-builds` or `[auto] trust_project_builds
= false` keeps them reviewed in every mode. Every shell call runs in durable process custody, so
an interrupted call leaves no process behind.

**What a confined command sees.** On Linux it runs in an empty private
mount view (a user namespace plus a chroot) holding exactly its grants:
the sandbox's own baseline, read only (`/bin`, `/sbin`, `/usr/bin`,
`/usr/sbin`, `/lib`, `/lib64`, `/usr/lib`, `/usr/lib64`, `/etc/ld.so.cache`,
`/dev/null`, `/dev/random`, `/dev/urandom`), plus the table's paths that
exist on the host. Nothing else of `/etc`, `/run`, `/proc`, `/sys` or your
home directory is there, and Unix sockets are refused. Why each grant:

- the workspace, temp dir, the Cargo home's `registry/` and `git/`, and
  the cache dir: builds, `cargo fetch`, scratch files;
- the rest of the Cargo home, read only: the tools in `bin/` and
  `config.toml` resolve, but a confined command cannot replace a tool or
  set `build.rustc-wrapper`; the registry token in `credentials.toml` is
  not in the view at all. The engine takes no read-only or hidden part
  inside a granted directory, so each entry of the Cargo home is its own
  grant, as it was when the shell's plan was made: an entry created later
  (a first `cargo install` creating `bin/`) shows after a restart. `cargo
  build` keeps working (its lock files are read only; cargo builds without
  them), `cargo install` does not;
- the rustup home, `/usr/{local,share,include}`, `/opt`: toolchains,
  headers, shared data;
- `/etc/{ssl,pki,ca-certificates}`: TLS root certificates (`curl`, `git`,
  `cargo` over HTTPS);
- `/etc/{hosts,nsswitch.conf,host.conf,gai.conf,services,protocols,resolv.conf}`:
  name and service lookup;
- `/etc/{passwd,group}`: user and group names (`ls -l`, `id`, `git`);
- `/etc/gitconfig`: git's system defaults.

**Environment.** A confined command does not inherit `rkat-tui`'s
environment. It gets `PWD` and exactly these variables, when `rkat-tui` has
them: `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `TERM`,
`COLORTERM`, `NO_COLOR`, `CARGO_HOME`, `RUSTUP_HOME`, `RUSTUP_TOOLCHAIN`,
`XDG_CACHE_HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `LANG`, `LANGUAGE`,
`LC_*`, `TZ`, `SSL_CERT_FILE`, `SSL_CERT_DIR`, `CURL_CA_BUNDLE`, and the
proxy variables `http_proxy`, `https_proxy`, `no_proxy`, `all_proxy` (in
lower and upper case). Proxy URLs pass unchanged, credentials included
(`http://user:password@proxy:3128`): a command can read them. Every other
variable, provider API keys included, stays outside the sandbox. Helpers' shell commands get the same set.
Unconfined commands (`off`) inherit the whole environment as before. In
every mode `TMPDIR`, `TMP` and `TEMP` name the agent's scratch directory
(below), not the shared temp directory.

### The scratch directory

Each agent gets a private scratch directory of its own,
`<temp>/rkat-tui-<12 hex digits>/` (`<temp>` is the daemon's temp
directory, usually `/tmp`), created with mode `0700`. Its shell gets it as
`TMPDIR`, `TMP` and `TEMP` in every confinement mode, `--no-confinement`
included, so test suites (`tempfile::tempdir()`), compilers and the
agent's own scratch files land there instead of the shared `/tmp`; its
helpers and forks share it, and the agent's instructions name it. The
gate treats it as the agent's own: a `write_file` or `edit_file` there
runs like an edit in the project, a read or search there is tier 1, a
shell command whose only writes are `>`, `>>` or `2>` redirects into it
is read-only shell, and a leading `TMPDIR=<it>` (also `TMP`, `TEMP`) is
accepted in front of an allowlisted command; the classifier is told the
directory is the session's. The shared `/tmp` stays judged by the
classifier.

Two engine tools do not take it: `blob_save_file` and `view_image`
accept only paths inside the project (their own check, which no grant
widens), so the agent's instructions tell it to save inside the project
and move the file with the shell, or copy an image into the project
first.

The name is fixed per agent and workspace, so a resumed agent finds its
directory under the same name, empty. rkat-tui uses the directory only
when it is a real directory owned by you that nobody else can enter: one
another user created first, a symbolic link, or one open to others is
never used (the agent then has no scratch directory and the daemon log
says why). It is removed, without following any link out of it, when the
agent is retired and when the daemon stops. It is created only when the
sandbox lets the shell write the temp directory (always, unless a grant
of yours displaced it).

The sandbox grants only directories and single regular files, at their own
path, never through a symbolic link on the way and never overlapping each
other or the baseline: that is why `/etc` is granted file by file, and a
path that does not exist on the host is left out. A granted file may itself
be a symbolic link to a regular file: the sandbox shows the target's content
read-only at the link's path.

**Name resolution.** `localhost` and other `/etc/hosts` names always
resolve, and DNS works from the granted `/etc/resolv.conf` wherever the
network is allowed (`workspace`, not `strict`). Where `/etc/resolv.conf` is
a symbolic link, as with systemd-resolved (`/etc/resolv.conf` →
`/run/systemd/resolve/stub-resolv.conf`), confined commands read the
target's content, read-only, at `/etc/resolv.conf` (the stub resolver
`127.0.0.53`) and see nothing else of the target's directory. The
resolver's `nss-resolve` socket stays unreachable (the sandbox refuses Unix
sockets), so lookups go through `resolv.conf`'s DNS server.

**More directories.** `--confinement-read <dir>` and `--confinement-write
<dir>` (repeatable) add directories. Each must be absolute, exist, be a
directory, contain no symbolic link on the way (the error names the real
path to pass instead), stay off the baseline and `/proc`, `/sys`, `/dev`,
and be disjoint from the workspace and the other added directories. A
directory already granted with at least that access (`--confinement-write
/tmp`) leaves the sandbox as it is and tells the approval gate you named
it; a detected grant that overlaps an added one otherwise yields to it
(for example `--confinement-write /opt/foo` drops the read-only `/opt`).
Any violation refuses the start.

A directory you name is trusted by the gate as well: a `write_file` or
`edit_file` inside one you named for writing is tiered like an edit in
the project (auto and accept-edits mode run it with no classifier call,
manual mode asks, plan mode refuses, and its protected paths such as
`.env` or `build.rs` still ask), and a `read_file`, `glob` or `grep`
inside any directory you named is tier 1. Inside a directory you
granted, a repository's `.git/hooks`, `.git/config` and any `.rkat` are
sealed for the file tools, as they are inside the project. The path is resolved first, so
a symbolic link inside the directory that points out of it does not carry
the grant. The sandbox's own defaults (the temp directory, the caches)
get only the classifier's context described below.

The same grants can live in your own `tui.toml` (the global
`<config-dir>/tui.toml` or a trusted realm `tui.toml`), next to the flags:

```toml
[confinement]
extra_read = ["/data/datasets"]   # like --confinement-read
extra_write = ["/scratch/build"]  # like --confinement-write
```

They follow the same rules and fail the start the same way (the error names
`[confinement] extra_read`/`extra_write` instead of the flag). Layers add
up (global, realm, then `--set 'confinement.extra_read=["/dir"]'`, whose
array value appends); a directory listed twice is granted once with the most
access any layer gives, and the flags win over a file grant that overlaps
one of theirs. A project `.rkat/tui.toml` may not widen the sandbox: its
`[confinement]` is ignored with a notice.

**What the approval gate is told.** The same grants are computed whether
the shell is confined or not, and the classifier is told them: every
directory outside the project the sandbox lets commands write by default
(the temp directory in use, that is the real `TMPDIR`, the user cache
directory, the Cargo home's `registry/` and `git/`), every directory it
lets them read (toolchains, `/usr/share` and the like, the Cargo home's
other entries, the name-resolution files), and every directory you added
with the flags or `[confinement]`, yours marked as named by you. A write
there is not a write outside the allowed scope; the classifier still
judges what the action does. With `--no-confinement` (for example inside a
sandbox of your own) the directories reach only the gate: name the ones
your own sandbox allows with `--confinement-write` so the classifier knows
them. The lists are sorted, your directories first, and each is cut after
2 KiB of paths with a count of the ones left out. A project `.rkat/tui.toml`
adds nothing to them either.

The shell's confinement is the command line's alone (`--confinement`, the
extra grants of the flags and your `tui.toml`): a `[confinement]` table in
the realm's `config.toml` does not decide it, and a start-up notice says so
when there is one.

**When the host cannot confine.** A host that cannot enforce the mode
refuses to start (exit 2; there is no silent fallback) and explains: what
blocked it, how to make it work, and how to run without confinement. The
Linux sandbox needs unprivileged user namespaces; the message reads the
switches under `/proc/sys`:

- `kernel.apparmor_restrict_unprivileged_userns = 1` (Ubuntu 24.04 and
  later): allow user namespaces for the `rkat-tui` binary with an AppArmor
  profile (narrow; attached to the binary's path),

  ```
  sudo tee /etc/apparmor.d/rkat-tui <<'EOF'
  abi <abi/4.0>,
  include <tunables/global>

  profile rkat-tui /path/to/rkat-tui flags=(unconfined) {
    userns,
  }
  EOF
  sudo apparmor_parser -r /etc/apparmor.d/rkat-tui
  ```

  or lift the restriction for every program (broad: a larger kernel attack
  surface) with `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`,
  kept across reboots by a file in `/etc/sysctl.d/`;
- `kernel.unprivileged_userns_clone = 0`: `sudo sysctl -w kernel.unprivileged_userns_clone=1`;
- `user.max_user_namespaces = 0`: `sudo sysctl -w user.max_user_namespaces=15000`;
- none of these (typically a container whose seccomp profile denies
  `unshare`): the backend's own refusal.

macOS uses Seatbelt; other platforms have no sandbox. To run without
confinement: `--no-confinement` (or `--confinement=off`), unless a realm
`[confinement]` requires it, in which case the message points at the realm
config instead.

### Security decisions

What rkat-tui does by default where a security review asked for a choice
(the user's decisions of 2026-11-10):

- **The network is on by default.** The default `workspace` confinement
  lets the agent's shell reach the network (package registries, `git
  fetch`, `curl`). Turn it off with `--confinement=strict` when you
  start rkat-tui (the daemon it starts takes the flag): the shell then
  gets the same files and no IP network at all. In auto mode a command that
  reaches the network is still reviewed by the classifier like any other.
- **No idle timeout.** A detached agent (`/detach`, a closed terminal)
  keeps running until its turn ends, or until you stop it (`/quit` from a
  TUI attached to it, `/daemon stop`, or `kill <pid>` from a shell). It does
  not stop by itself after a while.
- **Unattended turns.** A turn another agent's message, a schedule or a
  file watch started on your agent, while no TUI is attached to it, is
  still reviewed by the classifier in auto mode (your decision of
  2026-11-14; it used to deny everything that needed review): what the
  classifier allows runs, what it blocks is denied. Anything that would
  ask you (a protected file, a question of manual or accept-edits mode,
  a paused auto mode, an action too large to review, a review that could
  not be completed) is denied, the agent is told it needs you, and the
  next TUI that attaches to the agent shows what it wanted to do (see
  [Several agents](#several-agents)). Plan mode stays plan mode. A turn
  you started stays reviewed as usual after you detach.
- **The audit log** records every approval decision and who made it, and
  every change of mode, rules and trust, in `audit.jsonl` in the
  workspace's state directory, outside the workspace (see
  [Where things live](#where-things-live)); `/audit` and `rkat-tui
  --audit` show it.

Not provided, deliberately, until someone asks: a machine-wide managed
policy file that pins these choices for every user of a host, and
`cargo-vet` audits of the dependencies (CI runs `cargo deny`).

### Security model and its limits

What protects you, and where it stops. Each limit below is a deliberate
choice; after it, what a cautious user can do about it.

- **The agent's shell is sandboxed; its file tools are not.** Commands
  the agent runs with `shell` run under the confinement described above.
  The file tools (`read_file`, `write_file`, `edit_file`, `grep`, `glob`)
  run in the daemon as you: nothing at the operating-system level stops
  them. Inside the project, reads are allowed and edits follow the
  permission mode, and so do they in a directory you named with
  `--confinement-write` (reads also in one named with
  `--confinement-read`). Any other read, search or write outside the
  project is a reviewed action: in auto mode the classifier decides it, in manual and
  accept-edits mode you are asked, plan mode refuses it. Only the
  workspace's state directory, the attach token's directory (never read)
  and the sealed paths (`~/.rkat/tui`, the project's `.rkat/` and `.git/`,
  never written) are refused outright. *To tighten:* deny what must never
  be touched, for every file tool: a rule's subject is the path relative
  to the project, or absolute outside it, so `[[deny]] tool = "read_file"
  arg = "/*"` refuses every read outside the project (the same for
  `grep`, `glob`, `write_file`, `edit_file`); or work in manual mode (`--no-auto`, `[auto] mode =
  "default"`).
- **A secret inside the project is readable.** `read_file .env` in the
  workspace is allowed without asking (only writes to `.env`, `build.rs`,
  `AGENTS.md` and the like ask), and `@.env` attaches it. *To tighten:*
  `[[deny]] tool = "read_file" arg = "**/.env*"` (and for `grep`).
- **An allow rule for a file tool without an argument allows everything
  it can reach.** `allow write_file` with no `arg` (or `arg = "*"`)
  matches every path, protected files and paths outside the project
  included, and an allow rule answers what would otherwise be asked or
  classified. Such a rule is not flagged as broad. Deny rules, the sealed
  and secret paths and plan mode still win, and a project's `tui.toml`
  cannot add allow rules. *To tighten:* give every allow rule for a file
  tool an `arg` pattern inside the project (`arg = "src/**"`), and review
  `/permissions` now and then.
- **The sandbox shares `/tmp` and `~/.cache` with you.** The temp
  directory and your whole cache directory are granted read and write
  (with the workspace, and the Cargo home's `registry/` and `git/`): the
  agent's shell can read and change what other programs keep there. Its
  own temp files go to its private scratch directory (its `TMPDIR`), but a
  command that names `/tmp` itself still reaches the shared one, and the
  classifier, not the sandbox, judges what it does there (a sweeping
  delete is blocked under D6). It cannot connect to Unix sockets found
  there (the sandbox admits only IP sockets), but TCP, loopback included,
  stays open in the default mode. *To tighten:* `--confinement=strict`
  removes the network; nothing removes the two shared directories today,
  so keep secrets out of them.
- **A contained shell runs the project's builds unreviewed.** Under
  rkat-tui's sandbox, or one you declare with `--confinement=external`,
  cargo builds and tests run without a classifier call, also after the
  agent changed the dependencies, so test code the agent wrote runs with the
  network on (in `workspace` mode). The sandbox keeps it out of your home
  directory and provider keys; a declared sandbox is only as good as your
  description of it. *To tighten:* `--no-trust-project-builds`, or
  `--confinement=strict`.
- **The classifier judges effects, so it lets ordinary writes outside
  the project run.** Creating files in temp directories and caches, and
  installs of what the project already declares, are allowed by the
  policy's own exceptions rather than blocked as "outside the project".
  The price is a judge that reads "normal course" too generously: a
  small classifier model may let a cache edit or a sweeping delete pass
  that a stricter policy would have stopped. The sandbox still keeps the
  shell out of your home directory, `~/.ssh` and the like. *To tighten:*
  keep confinement on, choose a capable `[auto] classifier_model`, or
  work in manual mode for untrusted code.
- **"Safe" git commands run what the repository configures.** `git
  status`, `diff`, `log`, `show`, `blame` and the other read-only git
  commands run without review, and git runs the programs `.git/config`
  names (`core.fsmonitor`, diff `textconv` drivers, `diff.external`).
  They run inside the sandbox, but the sandbox can write `.git/` (an
  engine limit), and with `--no-confinement` they run as you. A cloned
  repository brings no `.git/config` of its own; a copied directory or an
  unpacked archive does. *To tighten:* keep confinement on, and with an
  untrusted checkout deny git: `[[deny]] tool = "shell" arg = "git *"`.
- **A user-scope note reaches every workspace.** A note saved with
  `scope = user` (the agent's `note_save`, which in auto mode the
  classifier reviews and in manual mode asks about) is listed, name and
  first line, in the system prompt of agents in every workspace, marked
  untrusted and stripped of headings and fences. It is the one way an
  agent in one workspace writes into another's prompt. *To tighten:*
  `[[deny]] tool = "note_save" arg = "user/*"`, and read
  `~/.rkat/tui/notes/` now and then.
- **A wire to another workspace lets the two agents talk unreviewed.**
  After `/wire` ([Wiring agents across workspaces](#wiring-agents-across-workspaces)),
  a `send_message` between the two agents is tier 1 in both
  workspaces, as between two agents of one workspace: your agent can
  send the other one anything it has read in your project without being
  asked, and the other agent can wake yours at any time (each message
  to an idle agent is a model turn you pay for). A message carries no
  authority: what your agent does about it passes your gate and
  sandbox, an unattended turn runs only what the classifier allows and
  nothing that would ask you, and the classifier never takes it for your
  words. Each agent's socket is
  `<realm>/wire/<agent>.sock`, mode 0600 in a 0700 directory outside
  the workspace, so only processes running as you can connect, and the
  daemon admits only envelopes signed by the agent you wired (a wrong
  or missing signature, or an unknown key, is dropped unanswered).
  *To tighten:* wire only projects you would let share what they read,
  `/unwire` when the work is done, or deny sends altogether
  (`[[deny]] tool = "send_message"`).
- **`!` runs as you, with no sandbox.** A `!` command is yours: it runs
  in your shell with your whole environment and files, unreviewed, and
  its output becomes part of the agent's context. *To tighten:* treat a
  `!` line as typing in your own terminal; do not paste commands from the
  agent into it unread.
- **A file attached with `@` is as trusted as one the agent read.** Its
  contents go into your message to the agent verbatim; a file that tells
  the agent what to do can be obeyed like any file it reads itself. The
  classifier sees only that files were attached, not their contents.
  Sealed paths are never attached. *To tighten:* attach only files you
  would let the agent read; `[auto] classifier_tools = false` additionally
  keeps file contents away from the classifier.
- **A non-loopback `--attach` carries the session unencrypted.**
  `--attach=HOST:PORT` to another machine needs `--attach-insecure`; the
  attach token never crosses the network (a challenge and response
  proves it), but the session after it does, readable and alterable by
  anyone on the path, approvals included. *To tighten:* never use
  `--attach-insecure`; forward the port over SSH (`ssh -N -L
  127.0.0.1:4100:127.0.0.1:<port> <host>`, then `--attach=127.0.0.1:4100`).
- **The attach token is the daemon's whole access control.** Whoever can
  read the endpoint file can attach, answer approvals and change the
  mode and rules. With confinement on, the agent's shell cannot read it
  and the file tools refuse it; with `--no-confinement` the shell can.
  *To tighten:* keep confinement on.

### The file tools' limits

The file tools run in the daemon and are bounded there, so one call can
neither hang nor take a helper's whole time:

- `read_file` reads at most the first 16 MiB of a file and refuses
  anything that is not a regular file (a named pipe, a socket, a device);
  `write_file` and `edit_file` refuse to rewrite a file larger than that.
- `glob` lists at most 500 paths, newest first. `grep` searches the first
  16 MiB of each file, skips binary files, and prints at most 250 lines
  (`head_limit`) and 64 KiB.
- `glob` and `grep` walk the tree on several threads and stop at a time
  budget (`[search] budget_secs`, default 10 s) or after examining a
  number of files and directories (`[search] max_entries`, default
  500 000), whichever comes first; `grep`'s reading and matching of each
  file count against the same budget. A project's `tui.toml` may only
  lower either.
- A search that stops early still answers with what it found and ends
  with a note saying it is partial, how far it got, which directories
  held most of the entries it examined, and how to narrow it:

  ```text
  [partial: the search stopped after 10.0 s and 412377 entries examined, because it reached its time budget of 10 s (`[search] budget_secs` in tui.toml); there may be more matches in what it did not reach. Most entries examined were under node_modules/ (371020), target/ (38112), src/ (911). To narrow it, search a subdirectory with `path` (the pattern is then relative to it) or use a more specific pattern.] The files listed are the newest among the entries examined, not of the whole tree.
  ```

  `glob`'s list is then the newest among what it examined, and `grep`
  suggests a `glob` filter (`*.rs`) as well. A partial search that found
  nothing says that matches may exist beyond what it examined, never
  just "no matches".
- `glob` walks only where its pattern can match: a pattern without `**`
  (`*-pnpm*/bin/pnpm`) goes no deeper than its segments and enters only
  the directories its next segment matches; one with `**` still skips
  what its segments before the `**` rule out (`src/**/*.rs` stays in
  `src/`). `grep`'s `glob` filter keeps the walk below the directories
  of its literal start (`src/ui/*.rs` walks only `src/ui/`). The answer
  is the same as a full walk's; fewer entries count against the cap. A
  pattern with `{…}`, `[…]` or `\` is walked whole. When `grep`'s output fills
  up, it stops searching; the lines shown are sorted by path but are not
  necessarily the first matches by path, and the truncation line says so.
- A call that goes away (the engine's tool timeout, Esc or Ctrl-C, a
  helper being cancelled) stops its work: the walk notices within an
  entry, a file being read within a megabyte, and a `write_file`,
  `edit_file` or note change that had not been made yet is not made.

## Helpers

The agent can hand work to helper agents (Meerkat mobs: each helper is a
mob member), and none of them blocks it: while a helper works the agent
still hears you and answers. One tool starts every helper, `assign`, with
two choices that do not depend on each other:

| | single-use (`keep: false`, the default) | kept (`keep: true`) |
|---|---|---|
| **fresh** (`context: "fresh"`, the default): the task is all it knows | `fresh · single-use`: told when it finishes, with its answer; then retired | `fresh · kept`: told each time it goes idle, with its answer; seated until `retire_helper` |
| **fork** (`context: "fork"`): goes on from a copy of the agent's conversation, on the agent's model | `fork · single-use`: its outcome reaches the agent when its first run ends; then retired | `fork · kept`: its outcome reaches the agent when its first run ends; each later run is told as it goes idle; seated until `retire_helper` |

What each costs: a fork carries the whole conversation on every one of its
model calls (costly in a long one), and a fresh helper knows only what its
task says, so the task must carry what it needs. A kept helper costs
nothing while idle, but every message to it starts a run of it (and its
answer one of the agent's), so it is only for giving the same helper a new
task after it finished one. Talking to a helper needs no `keep`: a
single-use helper can be messaged, and can message the agent, while it
works. Single-use is the default because left-over idle helpers cost
attention, and the agents are told so: keep a helper only when you already
know it will get a new task that needs what it learned, and retire a kept
one with `retire_helper` as soon as its work is done. `mob_create` and `mob_spawn_member` make teams in mobs of their
own (their members are kept fresh helpers); the engine's `fork_off`,
`spawn_member` and `spawn_many_members` are not offered (a fork is the
engine's own all the same: `assign` starts it through the engine's
`fork_off`).

`retire_helper` (`name`) retires one of the agent's own helpers of any
kind (its run is cancelled; nobody is told, the agent asked). The
engine's `retire_member` cannot: it lets a member retire only the
members its own `fork_off` seated.

Nothing stays seated without a living owner: however a helper is retired
(`retire_helper`, finishing its task, your stop, the start that settles
a retire the last stop left owed), its own helpers, theirs too, are
stopped and retired with it, deepest first, and the note its parent
gets, or `retire_helper`'s answer, names them ("Its own helper `c1` was
retired with it."). A message to one of them is then refused saying it
was retired with its parent. A single-use helper whose run ends while
one of its own helpers still runs is not done yet: that helper's ending
is told to it, and it is done once a run of it read that. Leaving a
conversation (`/new`, `/resume`) retires every helper it had, deepest
first, as before.

All four are the same in everything else: named the same way, wired to
the agent, told and listed (by their two words, below), limited in depth,
gated, stopped, retired and restored the same way. Every one is wired to
the agent both ways: each sees the other in `contacts`
and either can `send_message` the other at any time. A helper runs with the
TUI's file tools and a helper persona, on the model the rules below give it
(**Helper models**), through the session's endpoint, never the model's
provider default (its public URL with an environment key): a model the
endpoint's dialect serves, or one the model catalog does not know, goes
there; a model of another provider is refused, naming the configured
endpoints that speak its dialect. Its tool calls pass the **delegating
session's** gate (same rules and counters; escalations name the helper), so
starting a helper never unlocks an action. The transcript shows each
helper as one `[label · model]` line and the status bar counts running
helpers. To run a command while the agent keeps working, the agent assigns
a helper to run it and report (long-lived processes such as servers are not
possible yet).

The engine's `delegate`, which blocked the agent until its helper was done
(it heard no one meanwhile), is not offered either: no agent or helper has
it, and the engine refuses a call of it ("not allowed by policy"). A
conversation of an earlier rkat-tui still opens and shows its `delegate`,
`fork_off` and `spawn_member` calls and helpers (a `fork_off` child as a
kept fork, a `spawn_member` helper as a kept fresh one); a `delegate`
helper such a daemon left running is run on at the next start, and its
answer comes to the agent as a note.

**`assign`.** The agent (and a helper that may start helpers) starts a
helper with `assign` (`name`, `task`, optionally `context`, `keep` and
`model`): the call returns at once (`Helper scout is running …`) and the
agent goes on, hearing you and answering while the helper works. The
helper is seated in the agent's own mob beside it and wired to it both
ways: each sees the other in `contacts`, and either can `send_message` the
other at any time (a message to an idle one starts a run for it). A
single-use helper's parent is told at once, when its run ends and nothing
is queued for it, with its answer (`Your helper scout finished its task.
…`, cut at 4 000 characters as above), and it is retired. A message from
its parent that it has not read yet when its run ends makes it go on
instead. A fork's first run is told by the engine's own note
("Background fork_off job … finished", with its answer); a single-use fork
is retired once the engine recorded that outcome. A fork runs on the
agent's model: `context: "fork"` with a `model` is refused, saying to
leave `model` out or start a fresh helper. The transcript shows the call
as one line, `assign scout: <task>` (`assign (kept)`, `fork`, `fork
(kept)` for the other three). A name is 1 to 32 lowercase letters, digits
and `-`, not an agent's name and not one in use in the mob; anything else
is refused with what to do. "In use" means seated now, anywhere in the
workspace's mob: the agent's own helpers (running or kept idle), its
helpers' helpers, and other agents' helpers. The name of a helper that
ended (a single-use one finished, a kept one retired) is free again, for
any kind: it starts a brand-new helper, with none of the earlier one's
conversation, and its parent is told of it as of any other. A fork given
an earlier fork's name is told by its own outcome, also across a
restart. A retired helper cannot be brought back with its conversation;
keep it (`keep: true`) while it may have more work.

**When a kept helper goes idle.** A kept helper (`keep: true`, or a member
of a mob the agent created with `mob_spawn_member`) stays seated after its
task. Each time a run of it ends
and nothing is queued for it, its parent is told once, with its last
message (`Your helper w1 went idle. …`), delivered as the endings are; it is
not retired, and the note says that a message to it starts another run of
it, so to message it only with more work and to retire it with
`retire_helper` when done. No note comes when it starts (the spawn's result says so) or
when it becomes active (its parent or a peer made it so), and none when the
run's last call was a message to its parent (the parent has its last word).
A run an idle note starts for the agent is labelled `a helper went idle`.

**Interrupts and helpers.** Esc, Ctrl-C and Ctrl-X Ctrl-S stop
the agent's **own** run only: its helpers (assigned helpers, spawned
members, forks, at any depth) keep working, their tool calls are still
judged (the interrupt's Stop marks the agent's own calls only), and the
approval prompts of their calls stay open for you to answer. A helper that
finishes after the interrupt is told to the agent as below (held for your
next message). The daemon's stop still ends every helper.

**Stopping one helper.** Ctrl-C in a running helper's full view asks
`Stop helper scout? It cannot be undone: …`: `y` stops it (and the view
closes), `n`, Esc or a second Ctrl-C keeps it running and stays in the
view. In a finished helper's view Ctrl-C only says it is not running.
Ctrl-C in a view never interrupts the agent or counts toward quitting:
quit from the transcript. Esc in a helper's view always goes back to the
transcript at once, asking nothing (a pending `/btw` takes the first Esc).
In `/helpers`, `s` asks the same for the highlighted helper.
`s` (Stop) on the approval prompt of a helper's call stops that helper,
not the agent. A stop cancels the helper's run and retires it; the
helpers it started keep running (one level, as an interrupt).

**How a helper ended.** Its parent (the agent, or for a nested helper the
helper that started it) is told once, at once, however it ends: finished
(with its answer; an answer longer than 4 000 characters is cut and
written in full to the agent's scratch directory), failed, stopped by you,
or ended when the daemon stopped. Into a running parent the
note goes at its next step; an idle parent starts a run with it (a run no
user started, labelled `a helper ended`). Right after you interrupted the
agent (no run started since) it is not woken: the note is held, the
transcript says so at once, and it travels with your next message to the
agent, before your words. A held note survives detaching and the daemon's
restart (`held-notes.json` in the realm). Endings that come within a
quarter second go as one note. The helper's line and its full view's
title say how it ended in plain words (`stopped by you`, `failed: …`,
`ended when the daemon stopped`). What the transcript shows of the note
depends on how it reached the agent:

- **It started a run** (the agent was idle, or the note came after the
  running turn's last step and started the next run): one folded line, `✓ helper
  review-x finished: <the answer's first line> ▸` (`✗ helper review-x
  failed: <reason>`, `✓ 2 helpers ended: review-x finished; lint-y
  failed: …`, `✓ helper review-x went idle: …`, `■` for one stopped). A
  click or Ctrl-O opens the whole note as the agent got it; the answer in
  full is in the helper's own view, and the agent's reply follows. No
  separate notice is drawn for the same ending.
- **It went into a running turn**: a notice says who was told
  (`helper [review-x · model] finished; main-ab12 was told`).
- **It was held after your interrupt**: a notice says it goes with your
  next message (`… is told with your next message to it`). Another
  client attached to the agent shows the held note as its folded line,
  then your words.
- **The agent did not take it** (the engine refused the delivery, for
  example while the disk that holds rkat-tui's state is full): the note
  is held the same way, and the notice says why (`… is told with its
  next run: <reason>`); it goes with your next message or into the
  agent's next run, once. A note no helper on its way could take, while
  the agent runs another conversation, is written to the agent's scratch
  directory, and a notice names the file.

A note is never drawn as your message. A conversation opened again shows such a note as the same folded line,
and `/export` writes it in full under its heading. A helper the daemon's stop left is run on by the engine at
the next start (its answer then comes as a note); one the engine did not
keep is told as `ended when the daemon stopped`.

**Working or waiting.** The input's prompt marker shows whether the agent
is busy: the plain `›` while it is idle; a turning braille spinner (`⠋`,
cyan) while a turn runs, whether this TUI started it or the daemon reports
a run of the agent it did not start (a schedule, a watch, another
client, a fork's report), and while a classifier review or a compaction
runs. The input's text never moves. The spinner turns only while word
arrives from the agent: any event of its run or its helpers (streamed
text, a tool call requested or completed, a helper's progress, a
classifier or compaction event, an approval request). After 20 seconds
without any, the marker becomes a slow yellow pulse (`•`/`·`) and the
status bar's activity segment says, in yellow, what is being waited for
and since when, from what the TUI knows: `waiting for your answer 12s`
(an approval dialog is open), `compacting 40s`, `review by the
classifier 8s`, `tool shell running 2m 10s` (a call requested and not
completed), `waiting for the model's reply 34s` (a model request is
open), else `no word from the agent for 45s`. These are facts, not a
verdict: a long tool or a slow model is silent too. After two minutes of
silence during a turn it adds `· Esc interrupts`. The next event brings
the spinner back.

**Watching a helper.** A panel under the input lists the agent's live
helpers, one line each, indented by nesting depth: the running ones first
(the newest top-level helper first, its nested helpers under it), then the
seated idle ones (kept helpers a message would reach): `[scout · model] ⠋
running · 4 tool calls · grep "fn resolve"` while it works (its last call,
or the first line of its latest text), `idle` once its run is done. A
helper that ended stays on the panel, dim, with how it ended (`finished`,
`failed: …`, `stopped by you`, `retired`, `ended when the daemon
stopped`) for about ten seconds, then leaves it, whether or not a turn
started meanwhile; a selected one stays until the selection moves off it,
and when a selected row leaves the panel the selection moves to its
neighbour. The panel takes at most six lines; beyond that, a window around
the selection and a last line saying what it hides (`… and 4 more (1
running) · 1 above · 3 below`). It is hidden when empty.
Ctrl+↓/Ctrl+↑ move a selection in the panel (the newest first, wrapping
around). Enter on an **empty** input opens the selected helper's **full
view** in place of the transcript (Enter with text in the input sends it as
usual, and the input keeps working while the view is open). It reads as
the main transcript reads for the agent: the title names the helper, its
model, its kind (`assigned`, `fork`, `spawned member`, or `delegate` in a
conversation an earlier rkat-tui stored, when the call that started it was
seen) and its state (`running`, `finished`, `idle` for a
spawned member whose task is done, `failed`, `cancelled`), then, in
order, the task it was given as a prompt (`› `) under `task from
<agent>` (or the helper above it), every later input the same way (a
message an agent or helper sent it under `✉ from agent <sender>`, any
other input under `message`), its prose (markdown, its last one being
its answer), its reasoning (one line), its tool calls as lines (Ctrl-O
shows all of them and the reasoning in full) and notes,
and the headers of helpers it spawned. A fork opens with the
dim line `forked from <agent> with its conversation so far` and then its
own task: the conversation it inherited is not shown again. The task
comes from the helper's own run start, so a helper whose start the TUI
never saw begins with its work. PgUp/PgDn, Shift+↑/↓ and the wheel scroll
it; it follows new output while at the bottom. The view stays open after
the helper finishes, so its result can be read; its title ends with
`Esc back · Ctrl-C stop` (`Esc back` once it finished). Esc returns to
the transcript at once (Ctrl-C asks whether to stop a running helper, see
above), a second Esc drops the selection.

The transcript keeps each helper's header where it was spawned, as the
record: `▸ [scout · model] fresh · single-use · done · 7 tool calls` (the
kind when the call that started it was seen; the panel under the input
shows it too). A click on it unfolds the
helper's blocks in place (indented, tool output still collapsed), and
`/helpers` (or `/tasks`) opens a list of this conversation's live helpers:
the ones running, and the ones seated and idle that a message would reach
(a kept helper, or a single-use one not yet retired). Each row is as its
header shows it, with its kind: label, model, kind (`fresh · single-use`,
`fresh · kept`, `fork · single-use`, `fork · kept`; `delegate` for an
earlier build's), state (`running`, `idle`, or how it ended), calls and
tokens; nested ones are indented under their parent. The helpers that
ended (finished, retired, failed, stopped, ended with the daemon) are left
out; the hint line counts them (`a: show 14 ended`), and `a` lists them
too, dim, after the live ones (`a` again hides them), so their full view
stays reachable. With only ended ones the list says `no live helpers; a
shows N ended`. Whether a helper is live comes from what the daemon knows
of it, not from its run's end: the daemon says when a helper ended, and
sends the list of the helpers it seats for the conversation (nested ones
too) as the TUI attaches and each time one is seated or leaves, so the
live rows are exactly the helpers the agent still holds, after a
re-attach, a `/resume` or a daemon restart too. Helpers another agent of
the workspace started are not shown. Enter opens the highlighted helper's full view, as
Ctrl+↓ then Enter does, `f` folds or expands its block in the transcript,
and `s` then `y` stops a running one (a finished one says it has nothing
to stop); Esc, `q` or Ctrl-C close the list. `/helpers list` prints the
live ones as a notice with a count of the ended ones, `/helpers list all`
prints them all, and `/helpers expand|fold <label>|all` unfolds or folds
any helper's block. A kept helper that a message wakes again keeps its
one header: the panel under the input lists it while it is seated, and
its line says `idle` again after each run. `[ui] helpers = "expanded"` in `tui.toml` starts every header
unfolded (default `"folded"`); a resumed session shows its helpers folded.

**Bell.** When a turn ends (and no queued message starts the next one) or an
approval prompt opens while the terminal does not have the focus, rkat-tui
writes a terminal bell (BEL), which most terminals turn into a sound, a
badge or a desktop notification. Focus comes from the terminal's focus
reports; a terminal that sends none counts as focused, so it never rings.
`[ui] bell = false` in `tui.toml` (or `--set ui.bell=false`) turns it off.

Helpers nest one level deep by default, as in Claude Code: the session may
start helpers, its helpers may not, so a chain of helpers each carrying a
full context (and the mob and council tool schemas, about 25k tokens in
every call) never builds up. `[helpers] max_depth = N` in `tui.toml` (or
`--set helpers.max_depth=N`; a project file may only lower it) allows N
levels: a helper at the limit gets no `assign`, `mob_create`,
`mob_spawn_member` or `council` tool (left out of its tool list and denied
by its tool access policy), and a deeper spawn is refused.

**How many at once.** Each agent, and each helper that starts helpers,
holds at most `helpers.max_live` helpers of its own from `assign` at a
time (8 by default, at most 64; `--set helpers.max_live=N` or
`/set helpers.max_live N`; a project file may only lower it). Its own
are the ones it started itself, not its helpers' helpers: each helper
has the same limit of its own, not a share of its parent's. A helper
counts while it is seated: running, kept and idle, or still being
started (so several `assign` calls in one model step cannot pass the
limit together); one that finished or was retired does not. Members of a
mob made with `mob_create` and `mob_spawn_member` are not counted. A call
past the limit is refused with the helpers it holds, the kept idle ones
first, and what to do (retire with `retire_helper` the ones it no longer
needs, or wait for a single-use one to finish); `contacts` says
`7 of 8 helpers` once the count is within two of the limit. A changed
limit applies to the next `assign` at once; a lowered one retires nobody.

**Helper models.** A helper's model never comes from a stored profile. For
every fresh helper started with `assign` or `mob_spawn_member`
(into any mob, by a profile's name, an inline profile or nothing), it is,
in this order:

1. the model the call names (`assign`'s `model`, an inline profile under
   `tooling`);
2. else `helpers.model`;
3. else its owner's current model.

A profile the spawn names by its name (the workspace mob's `main`, or a
profile of a mob the agent made with `mob_create`) still gives everything
else it defines (tools, instructions and the rest), but its model, its
provider and server, and its fallback chain are replaced: the workspace's
mob keeps the `main` profile of its first start, on the model of that day,
and a profile the agent wrote may name any model. A fork is
the one exception: it continues its parent's conversation, so it stays on
its parent's current model, and neither `helpers.model` nor the lists
below apply to it. Whatever the
source, the model runs through the owner's endpoint as above: on a
self-hosted endpoint as its alias `<endpoint>:<model>`, also when an inline
profile names it, and only a model the endpoint listed at start or that
`/model` switched to has one (a helper on another is refused, naming the
models it has; `/model` to the new one registers it). `helpers.model` takes
what `/model` takes: a model the owner's endpoint serves, or
`<endpoint>:<model>` naming that endpoint (a helper on another endpoint is
refused, and so is a `helpers.model` naming one).

`helpers.allow_models` or `helpers.deny_models` (at most one of the two,
over your own and the workspace's file and the command line) limits the
models those helpers (`assign`'s fresh ones, `mob_spawn_member`) may run
on. Each is a list of patterns: an exact model name
or a prefix ending in one `*` (`"claude-haiku-*"`); a `*` anywhere else, an
empty entry, whitespace, an empty list or `*` alone is refused naming the
entry. A pattern matches the model's plain name, the name the endpoint
serves it under, without an `endpoint:` prefix (a helper's endpoint is its
owner's, so a pattern does not name one). The model a spawn ends up with,
named or by default, must pass: otherwise the spawn is refused and the
agent's tool result says which model was refused, the list in force, and
what to do (`name one of: …`, `name a model other than …`, or, when the
spawn named none, that the call must name a permitted model). A
`helpers.model` the list refuses is reported when the settings load or change, naming both
settings, and the helpers that would use it are refused.

```toml
[helpers]
model = "claude-haiku-4-5"
allow_models = ["claude-haiku-*", "claude-sonnet-4-6"]
```

The three are yours: `/set` and `/unset` (a list as `a, b*` or a TOML
list), `--set 'helpers.allow_models=["claude-haiku-*"]'` or a hand edit of
your own or the workspace's `tui.toml`; a project's `.rkat/tui.toml` may
not set them. A change applies at once to the helpers spawned from then
on, with one notice to the clients; a hand edit that sets both lists is not
applied (the clients are told, and the last good settings stay), and both
at start refuse the start as any bad `tui.toml` does. An agent's
instructions (and those of a helper that may start helpers) say in one
line where a helper's model comes from (a mob profile's own model is
ignored), and, when one of the three is set, the default and which models
are permitted; every live agent is told of a change. The classifier's model (`auto.classifier_model`) is separate
and unaffected.

The agent forks itself with `assign` and `context: "fork"`, which starts
the fork through the engine's own `fork_off`: the child starts from the
conversation so far (its committed part), works on the task it is given,
runs behind the same gate as every helper and shows as one (see [The
workspace daemon](#the-workspace-daemon)). The call returns at once while
the child works in the background, and when the child's first run ends
its outcome is written
into the agent's transcript ("Background fork_off job … finished"): at
the agent's next model call if it is mid-turn, else in a turn of its
own. The engine seats a fork wired to no one; the daemon wires it to the
agent (or helper) that forked it as soon as it is seated, and its tool
calls wait for that edge, so the two can message each other while it
runs (the parent's message to its fork is a peer message, not reviewed).
A single-use fork runs one task: once the engine has recorded its
completed first run's outcome for the parent (the record the outcome is
delivered from, across a restart too), the daemon retires it, as a fresh
single-use helper is retired when it finishes; one the daemon stopped
before retiring it is retired at the next start. Nothing more is told:
the outcome is the news. A message the parent sent that the fork had not
read when its first run ended starts one more run; when that ends the
parent is told as for a fresh single-use helper ("Your helper `kid`
finished its task. … It is retired"), and the fork is retired then. A
message to a retired fork is refused, saying it finished and was retired.
A kept fork (`keep: true`), and a fork that an earlier build kept seated
after its outcome, stays seated, across restarts too, until it is
retired; a later run of it is told as for a kept helper ("Your helper
`kid` went idle …"). A fork still in its
first run when the daemon
stops does not survive the stop: the engine counts the stopped run as
failed and retires the fork, and after the next start the parent is told
`Background fork_off job … finished (failed)` in a run of its own
(often only right after its next run).
A helper's status as the engine's `member_status` and `mob_check_member`
give it shows no live progress: their `output_preview` and `tokens_used`
are from the helper's last completed turn, so a fork's preview is the
answer it inherited; the agents' hint says so and points them to asking
the helper instead. (The TUI's own helper panel follows the helper's
events live.)
The transcript shows that outcome compact, live and in a restored
conversation: `⚙ fork <name> finished (completed) · 11 turns · 39 tool
calls · 12.3k tokens`, then the fork's report as Markdown folded to four
lines (a click or Ctrl-O unfolds it); a failed or cancelled one shows its
status and error unfolded. An outcome of another shape is shown as the
engine wrote it. What the agent itself reads is the engine's notice,
unchanged.

## Settings

Every setting has three forms with the same dotted key: in a file
(`[auto]` `compact_prepare_at = "65%"` in a `tui.toml`), on the daemon's
command line (`--set auto.compact_prepare_at=65%`, and the dedicated
flags the [configuration reference](#configuration-reference-tuitoml)
lists as shorthands), and in the TUI (`/set auto.compact_prepare_at
65%`). Each setting is declared once, with what it takes, its default,
what a project file may do with it and when a change applies; the
reference table is generated from that declaration.

**Two levels you write.** Your own `~/.rkat/tui/tui.toml` (the user
level: every workspace) and the workspace's realm `tui.toml` (the
workspace level). Precedence, lowest first: the default, your file, the
workspace's, the command line; a project's `.rkat/tui.toml` counts only
for the keys it may set, as the reference says (where it may set a key,
the command line still wins; where it may only lower a value, its lower
value wins over every layer). rkat-tui never writes a project's file.

**From the TUI.**

- `/set <key> <value>` writes the workspace's file; `/set <key> <value>
  --user` writes yours (`--global` is the same). The value is typed as
  the setting takes it (`true`/`false`/`on`/`off`, a number, `65%` or
  `300k`, a word, text; quotes are optional).
- `/unset <key> [--user]` removes that level's value: the next layer's
  applies.
- `/settings [filter]` opens a list of every setting (those whose key
  holds the filter): the key, the value in force and the layer it comes
  from; the line under each says what every layer holds, the default,
  when a change applies, what it does and what it takes. Enter opens a
  one-line field for the workspace's value, `u` for yours (Tab switches
  between the two, Esc cancels, Enter writes); `d` then `y` removes the
  value at the layer in force when that is yours or the workspace's (a
  command-line or project value is not rkat-tui's to remove, and the
  list says so). The structured sections (rules, statements, directory
  grants, endpoints, prices, thinking) are listed last with the commands
  that change them. The footer names both files.

The friendly commands are shorthands for the same path and take
`--user` too: `/compact prepare … [--user]` (`auto.compact_prepare`,
`auto.compact_prepare_at`), `/auto model <ref> [--user]`
(`auto.classifier_model`), `/mode <mode> --user` (`auto.mode`; `/mode`
and Shift+Tab record the workspace's), Ctrl-T (the workspace's
`ui.checklist`), `/permissions add … --user` (a rule in your file), and
`/permissions`' `A` and `n` (always, in every workspace).

**What a change says.** Every change answers with the key and value,
the level and the file written, the value now in force and the layer it
comes from, and when it applies: "auto.compact_prepare_at = 65%:
written to your tui.toml (~/.rkat/tui/tui.toml); in force: 65% (from
your tui.toml); applied at once; the agents are asked at 65% of the
window (about 650k tokens for <model>) to: "update PLAN.md"". When a
higher layer keeps another value in force it says so ("the value in
force did not change; this workspace's tui.toml sets it (300000), which
wins here: /unset auto.compact_prepare_at removes it", or "the daemon's
command line sets it (80%), which wins until the daemon restarts"). A
value the setting does not take is refused with what it takes and
nothing is written. Each change is in the audit log (`setting`).

**When it applies.** "At once" settings (the mode, the classifier
model, `trust_project_builds`, the prepare instruction and threshold,
the `[ui]` settings) apply in the daemon as soon as the value in force
changes, and every attached client follows a `[ui]` setting. The others
apply at the next start of the daemon (or, for a client-side setting
such as `shell.user_timeout_secs`, the next start of rkat-tui); the
answer says which. A change that a higher layer overrides is written but
changes nothing in force.

**How the files are written.** Only the one key changes: comments, the
order of keys and the formatting of the rest of the file stay as they
were. The result must read as a `tui.toml` before anything is written
(else nothing is), the file is replaced atomically, a symlinked file is
refused, and a lock on the file's directory keeps two daemons writing
your file from losing each other's change.

**Following the files.** A running daemon watches your `tui.toml`, the
workspace's and the project's `.rkat/tui.toml`. When one changes (a hand
edit, another workspace's daemon writing your file) it reads it again
after a moment's quiet, and each setting whose value in force moved is
applied as a `/set` would apply it and told to every attached client
once: "auto.compact_prepare_at is now 65% (from your tui.toml; your
tui.toml changed)", with when it applies when that is not at once.

A change of the rules, the `[auto] allow` statements or the
`[confinement]` directories in your own `tui.toml` or the workspace's
applies at once too, exactly as a command's would: the daemon computes
what the files give as it does at start and holds that, and every
client is told once what changed ("your tui.toml changed: 1 statement
added, 1 granted directory removed; in force now"). Each item added or
removed is in the audit log (`rule_added`, `statement_removed`,
`grant_added`, …). What a start refuses is skipped and named in that
notice, the rest applies: a rule that does not parse (the next start
refuses the file until it is fixed), a broad allow rule, a statement
the classifier would not take or one past twelve, a directory the
start's plan would refuse or that reaches your settings. A directory
newly granted this way reaches the gate and the file tools at once; a
confined shell's sandbox is planned at the daemon's start, so the shell
itself can write it from the next start on. Your session grants and
statements are untouched. The project's `.rkat/tui.toml` is writable by
the agent's own shell, so a change of its sections (the deny rules it
may add) applies at the next start only, and so does a change of the
endpoints, prices or thinking of any file; both are named once. A file that does
not read is reported once ("… does not read, so its last good settings
stay in force: …") and its last good values stay until it reads again.
A project file counts only for what a project may set, as at start. The
daemon's own writes change nothing more (it compares what the file
holds, not when it changed). Each followed change is in the audit log
(`setting_changed`, `settings_unreadable`).

## Rules

Rules live in TOML, layered (later settings win): the user-global
`<config-dir>/tui.toml`, then `<realm>/tui.toml` (written by `/permissions add`),
then the command line, then `<project>/.rkat/tui.toml`. The project file may
only add deny rules and `[auto]` settings; its `[endpoints]`, `[pricing]`,
`[thinking]` and `[confinement]` are ignored with a notice, and so is its
`auto.classifier_model` (the model that reviews the agent's actions is
yours to pick) and its `auto.allow` statements. Broad allow rules (blanket shell, interpreters, package-manager run
commands) are dropped when auto mode starts.

**`/permissions`**' In force section lists the rules in effect and
removes or adds them ([In force](#in-force-rules-grants-and-statements));
`/permissions add allow|deny <tool> [pattern] [--user]` adds one typed
(`--user` writes the rule to your own `tui.toml`, for every workspace),
`/permissions rm <n>` removes one, and `/permissions list` prints them as
a notice.

**The realm file.** The workspace's realm `tui.toml` lives in its state
directory under the config directory, out of the agent's and a
repository's reach: it is yours, and a hand edit applies.

```toml
[auto]
mode = "auto"                            # or "default" (manual), "accept-edits", "plan"
classifier_model = "anthropic:claude-sonnet-4-6"   # optional ([endpoint:]model; /auto model writes it), per-dialect default otherwise
trust_project_builds = true              # cargo build/test/check are tier 1
compact = "native"                       # or "warn" / "auto"; see "Context and compaction"
compact_warn_percent = 75
compact_auto_percent = 85
classifier_tools = true                  # stage 2 may read project files
classifier_probe = true                  # check the classifier model at start
allow = ["pushing to the default branch of this repository"]  # statements; see "Allowing a kind of effect"

[[allow]]
tool = "shell"
arg = "cargo nextest run*"

[[deny]]
tool = "shell"
arg = "git push*"
```

### Configuration reference (`tui.toml`)

Every key can be set in a user layer or for one run with
`--set <dotted.key>=<value>`. The value is TOML (`true`, `85`, `"x"`,
`[{tool = "shell"}]`); anything that does not parse as TOML is a string, so
`--set auto.compact=auto` needs no quotes. Command-line settings form a layer
after the global and realm files and before the project file: tables merge,
scalars replace, arrays append, endpoints merge by name. A bad `--set`
(unknown key, wrong type, invalid pattern or dialect) stops start-up and
names the argument; a value a setting does not take is refused with what
it takes (`auto.compact_warn_percent takes a whole number from 1 to
100`). Command-line rules show as `(cli)` in `/permissions list` and
are never written to a file.

The table is generated from the settings table in the source
(`src/config/registry.rs`); "takes effect" says when a change made while
the daemon runs applies. Precedence is default, then your own
`tui.toml`, then the workspace's realm `tui.toml`, then the command
line; a project's `.rkat/tui.toml` counts as the "A project file"
column says (where it may set a key, the command line still wins; where
it may only lower a value, its lowering applies over every layer).

<!-- settings-table:begin -->
| Key | Takes | Default | A project file | Takes effect | Flag, command |
|---|---|---|---|---|---|
| `auto.mode`: The permission mode at start (`default` is manual: every call that needs review asks you). | `default`, `accept-edits`, `plan`, `auto` | auto | may only turn auto mode off (`mode = "default"`) | at once | `--no-auto`, `/mode`, Shift+Tab |
| `auto.classifier_model`: The model that reviews the agent's actions in auto mode. | a model, `[endpoint:]model` | the start-up endpoint's per-dialect default | may not set it | at once | `--classifier-model`, `/auto model` |
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
| `[[allow]] / [[deny]]`: Rules: `tool`, then `arg` (a glob) or `literal`. | a section | none | may only add deny rules | at once from your own and the workspace's file (a project file's at the next start) | `--allow` / `--deny <tool>[=<glob>]`; /permissions (In force: r adds, d removes; Blocked: a, A) |
| `auto.allow`: Statements: kinds of action you allow, which the classifier reads. | a section | none | may not set it | at once | —; /permissions (In force: n adds, d removes; Blocked: e) |
| `auto.trusted_hosts`: Hosts you trust with the project's data (`host` or `*.domain`), which the classifier reads beside the hosts of the project's git remotes; other rules still apply there. | a section | none | may not set it | at once | `--set 'auto.trusted_hosts=["<host>"]'`; — (edit the file) |
| `confinement.extra_read / extra_write`: Extra directories for the confined shell, which the approval gate is told too. | a section | none | may not set it | the gate and the file tools at once; a confined shell at the next start | `--confinement-read` / `--confinement-write <dir>`; /permissions (Blocked: a, A) |
| `endpoints.<name>`: `dialect`, `base_url`, `api_key_env`, `priority`, `api_style` (an inline `api_key` is refused). | a section | none | may not set it | at the next start | `--endpoint-def <name>=<dialect>=<url>[,key_env=VAR][,api_style=STYLE]`; — |
| `pricing."<model id>"`: `input`, `output`, `cached_input`, `cache_write` in USD per million tokens. | a section | none | may not set it | at the next start | `--set 'pricing."<id>".input=15'`; — |
| `thinking."<endpoint:id or id>"`: Anthropic thinking: `"adaptive"`, `{ enabled = <budget tokens> }` or `"default"`. | a section | none | may not set it | at the next start | `--set 'thinking."proxy:my-alias"=adaptive'`; — |
<!-- settings-table:end -->

Other flags: `--model`/`-m <[endpoint:]id>`, `--endpoint <name>`,
`--resume [ref]`, `-c`/`--continue`,
`-w`/`--workspace`, `--config-dir`, `--user-config-root`, `--confinement`,
`--confinement-read`, `--confinement-write`, `--no-confinement`, `--no-mouse`, `--wait-for-mcp`, `--trust-project`,
`--no-pricing-fetch`.

## Models, endpoints and cost

### Setting up an endpoint

An **endpoint** is a named place to send requests in one of Meerkat's wire
dialects: `anthropic`, `openai` (also LiteLLM and other OpenAI-compatible
proxies) or `gemini`. A dialect names the protocol, not who serves the
model.

- **The built-in endpoints** `anthropic`, `openai` and `gemini` need nothing
  but their key: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or `GEMINI_API_KEY`
  in the environment (or a binding in the realm config, or the `rkat`
  token store).
- **Any other server** is an endpoint you configure, in
  `~/.rkat/tui/tui.toml` (yours, for every workspace), in a workspace's
  realm `tui.toml`, or for one run on the command line. A project's own
  `.rkat/tui.toml` cannot define endpoints.
- **The key is always an environment variable** you name with
  `api_key_env` (`key_env=` on the command line). rkat-tui records only the
  variable's name; a key written into a file or `--set` refuses the start.

For a LiteLLM proxy at `https://litellm.example.com` whose key is in
`WORK_LLM_KEY`:

```bash
export WORK_LLM_KEY='sk-…'
```

```toml
# ~/.rkat/tui/tui.toml
[endpoints.work]
dialect = "openai"
base_url = "https://litellm.example.com"   # the proxy's root, with or without /v1
api_key_env = "WORK_LLM_KEY"
```

or, for one run, `--endpoint-def work=openai=https://litellm.example.com,key_env=WORK_LLM_KEY`.
Its models are `work:<model>`, the model as the proxy lists it: start with
`--model work:claude-opus-5-5`, or pick one with `/model` (the picker lists
every endpoint's models). A server without the Responses API takes
`api_style = "chat_completions"` (`,api_style=chat_completions`). With the
proxy as the only endpoint, the safety classifier runs there too: give it
a model the proxy serves with `--classifier-model work:<model>` (or
`[auto] classifier_model`).

### How endpoints reach the engine

Configured endpoints are **written into the realm's own `config.toml`**
(through the realm's `ConfigRuntime`: the writer lock and generation check
`rkat config`, REST and RPC write under, retried when another writer got in
between) as connections of the `rkat-tui-endpoints`
realm section: a backend with the endpoint's URL, an auth profile reading
the environment variable `api_key_env` names (the document records the
variable's name, never a key) and a binding. Keys come from the environment
only: an inline `api_key` in `[endpoints.<name>]` (or through `--set`)
refuses the start (exit 2) with a message naming `api_key_env`. A
session's binding therefore resolves for `rkat`, REST and RPC on the same
realm too.

**A LiteLLM proxy** (or vLLM, or any OpenAI-compatible server) is an
`openai`-dialect endpoint:

```toml
[endpoints.litellm]
dialect = "openai"
base_url = "http://localhost:4000"   # the proxy, without /v1
api_key_env = "LITELLM_KEY"          # the variable holding its key
```

(or `--endpoint-def litellm=openai=http://localhost:4000,key_env=LITELLM_KEY`).
The engine reaches it as one of its `self_hosted` servers (its connection
is in the realm section `rkat-tui-endpoints`, with every configured
endpoint's), and every
model it serves as the alias **`litellm:<model>`**: the request names the
model exactly as the proxy lists it, and everything the engine knows of
the model comes from the proxy, never from Meerkat's catalog. At start the
daemon reads the proxy's `/v1/models` and LiteLLM's `/model/info`, and
writes one `[self_hosted.models."litellm:<model>"]` entry per listed model
into the realm config: the context window, the output limit, reasoning
(`supports_reasoning`, which also lets the engine pass a reasoning effort)
and images (`supports_vision`); a flag the proxy does not report is off,
sampling temperature is on. A `/model` switch to a model the proxy listed
since adds its alias then. Prices come from `/model/info` too. So
`litellm:claude-opus-5-5` runs on the proxy's window and prices over the
OpenAI wire, with none of the Anthropic request shaping (no thinking
field, no beta headers); the same id on the built-in `anthropic` endpoint
keeps the catalog's row. Every request's tool definitions are sent in a
form every provider accepts: the daemon inlines their `$ref`s, folds an
optional field's `anyOf [T, null]` into `T`, writes `oneOf` as `anyOf` and
gives every constrained schema node its `type`, so a Gemini model behind
the proxy (Vertex AI refuses a parameter without a `type`) runs too; a
tool accepts the same arguments as before. The status bar and `/model` show the endpoint and
the model (`litellm` `claude-opus-5-5`); the picker matches both
`litellm:claude` and `claude`; a stored conversation records the alias,
and a bare model id that only one endpoint lists still resolves to it.
The wire is the Responses API (`/v1/responses`) by default, what
rkat-tui's OpenAI dialect always spoke. A proxy or server without it (an
older LiteLLM route, vLLM, llama.cpp, Ollama) takes Chat Completions:

```toml
[endpoints.vllm]
dialect = "openai"
base_url = "http://localhost:8000"   # the server's root; /v1 is added
api_key_env = "VLLM_KEY"
api_style = "chat_completions"       # or "responses", the default
```

(or `--endpoint-def vllm=openai=http://localhost:8000,key_env=VLLM_KEY,api_style=chat_completions`).
`base_url` is the server's root in both styles, with or without `/v1`:
rkat-tui gives the engine `<base_url>/v1` for Chat Completions (requests
go to `/v1/chat/completions`), and the Responses client goes to
`/v1/responses` either way. Text, reasoning and tool calls stream in both
styles. `api_style` takes exactly `responses` or `chat_completions`;
anything else stops the start naming the key, and on an `anthropic` or
`gemini` endpoint it skips the endpoint with a notice (those dialects have
one wire each). Limits: `[thinking]` does not apply to such an endpoint
(it is Anthropic's setting; the proxy's reasoning flag decides); helpers
run on the proxy too (a model a helper asks for by its plain name becomes
the proxy's alias of it).

**An Anthropic- or Gemini-dialect gateway** keeps that dialect's client and
the catalog's request shaping for a model the catalog knows (the engine
has no alias for these dialects); the window, the compaction threshold and
the prices are still the gateway's (see the next paragraph). Only a model
Meerkat's catalog knows runs there (or on a built-in endpoint): a model the
catalog does not know is refused at start and at `/model`, with the reason
and the way out, which is to configure the proxy as an `openai`-dialect
endpoint, where every model it lists runs as `<endpoint>:<model>`. (A
`[models.<id>]` entry you write into the realm's `config.toml` yourself
still counts; rkat-tui writes none.)

A configured endpoint is the authority for its own models' facts: the
context gauge, `/context` and the agent's compaction threshold use the
window the endpoint lists (LiteLLM's `/model/info` fills windows, output
limits, prices and the `supports_reasoning`, `supports_vision` and
`supports_function_calling` flags), also for a model the Meerkat catalog
knows; the catalog's row decides only where the endpoint says nothing. On
a built-in endpoint the catalog's row comes first.

Model references are `endpoint:id` or a bare id. A bare id is looked up in
the endpoints' live model lists (fetched in parallel in the background, 10 s
each); an id the lists do not have goes to its provider's endpoint only when
the Meerkat model catalog knows it. Names are never guessed from a prefix.

`/model` opens a picker grouped by endpoint (type to filter), `/model <ref>`
switches directly and `/model refresh` refetches the lists. A switch is a hot
swap of the live session's model through the runtime (between turns), also
across endpoints and dialects; the session's metadata records the model and
binding, and `-c`/`/resume` derive the endpoint from them. Helpers follow
the session's model; the classifier stays on the start-up endpoint.

**Thinking.** `[thinking]` sets Anthropic thinking per model, keyed by an
`endpoint:id` reference or a bare id (the reference wins):

```toml
[thinking]
"proxy:claude-alias" = "adaptive"         # the model picks its budget
"claude-sonnet-4-5" = { enabled = 8000 }  # at most 8000 thinking tokens
"claude-opus-4-7" = "default"             # no override (undoes a lower layer)
```

Anthropic checks every replayed thinking block against the request it was
made in, so a request whose tool list changed (an MCP server connected, a
resume) is refused unless it asks to drop stale blocks, and the engine asks
only for catalogued models or when thinking is set explicitly. A proxy alias
or another id outside the Meerkat catalog therefore needs a setting here.
The setting goes into the session's provider params when the session is
created, on every `/model` switch (a model without one gets none), and on
every resume (the session follows the setting as it is now). It
applies on Anthropic-dialect endpoints only: a reference to an endpoint of
another dialect, a bare id the catalog gives another provider, and a type a
catalogued model does not accept (an adaptive-only model with a budget, say:
Claude Fable 5.2, `claude-fable-5-2`, takes only `"adaptive"`)
are ignored with a start-up notice; an id outside the catalog is sent as
set. Like `[pricing]`, only your own `tui.toml` files set it. Helpers
(every kind `assign` starts) keep the engine's default.

The status bar shows the context gauge, `ctx 42% (84k/200k)` (yellow from 70%,
red from 90%, `~` for an estimate), and the session's cost, `$0.42`: each
model call's tokens are priced per model from `[pricing]` overrides, a
LiteLLM endpoint's `/model/info`, or LiteLLM's community price list (cached
for 7 days in the user cache directory, `$XDG_CACHE_HOME/rkat-tui` or the
platform's; `--no-pricing-fetch`, or `fetch = false` under `[pricing]` in
your `tui.toml`, skips the download). That list is the one request
rkat-tui makes to a host other than your LLM endpoints:
`https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json`,
at most once a week. The daemon counts the tokens, whether or not a
client is attached: every model call of the agent under the model the
provider accounted it to, every helper's (also listed per helper), every
classifier call under the classifier's model, and every compaction (its
estimated input plus the summary). A turn that ran while you were
detached, a schedule's or a file watch's, is in the totals the next time
you attach, every client of the agent shows the same totals, and they
survive a daemon restart. `/usage` prints the breakdown. The readout is an
estimate, not a bill: prices come from the sources above (`~` when a model
has no price or the price is a stale estimate), and a compaction's input
is the engine's estimate of the history it summarized.

## Instructions and memory

The system prompt of a new session is built from, in order:

1. the base prompt: the user-global `SYSTEM.md` replaces Meerkat's default
   when it is non-empty and the realm config sets no `system_prompt` /
   `system_prompt_file` of its own;
2. the workspace `AGENTS.md` (or `.rkat/AGENTS.md`), loaded by Meerkat, and
   the skill inventory;
3. the coding persona (working style only: tool behaviour is described by
   the tools themselves), then the user-global `AGENTS.md`, every `AGENTS.md`
   (or `.rkat/AGENTS.md`) from the git root down to the workspace's parent
   directory, and the workspace `AGENTS.local.md` (or
   `.rkat/AGENTS.local.md`, meant to stay uncommitted), each under
   `# Instructions (from <path>)`. Each file is capped at Meerkat's
   `AGENTS.md` limit (32 KiB) and all of them together at 96 KiB. The git
   root is the nearest directory with a `.git` directory or file; your home
   directory is never one;
4. the notes index (below), when there are notes, labelled as untrusted,
   model-written context rather than instructions.

Instruction files and notes are re-read for every new session (`/clear`,
`/new`). A resumed session (`-c`, `/resume`, and the rebuilds a model switch
makes when it moves the compaction threshold) keeps the instructions it was created with: Meerkat
does not reassemble a resumed session's system prompt, so the TUI passes the
session's persisted instructions back unchanged (the persona appears once).
Edited instruction files apply from the next new session; `/remember`
and `/memory` reach the live one.

**After an upgrade.** rkat-tui's own sections (the coding persona, what
an agent is told of the other agents and of its helpers, the tools it can
reveal, and a helper's instructions) are a build's text. When the daemon
seats an agent on a conversation an earlier build began (at its start,
`/resume`, an attach to a conversation whose agent quit) and before
anything reaches a helper restored from an earlier build, it compares
those sections in the conversation's system prompt with this build's and,
when they differ, replaces them in place (the rest of the prompt, your
instruction files included, stays as it was). The agent's view shows one
line, "the agent's instructions were brought up to this build's" (to the
next client to attach when none is), and the audit log an
`instructions_updated` line. It happens once per upgrade: a later start
finds them current and changes nothing. The earlier text does not stay
beside the new one, and a compaction keeps the new one.

**`/memory`** opens a list of the three instruction files, each with its
path and its size (or "not written yet"); Enter edits the highlighted one
as the typed forms do, and a file that cannot be edited from here (the
daemon's workspace is not on this machine) says why. **`/memory
workspace`** opens the workspace `AGENTS.md` (the file `/remember` writes),
**`/memory user`** the user-global `AGENTS.md` and **`/memory system`** the
user-global `SYSTEM.md` in your editor: `$VISUAL`, else `$EDITOR`, else
`vi`, split on whitespace so `code --wait` works (a variable holding only
blanks is refused with a notice). rkat-tui leaves the alternate screen, runs
the editor on its terminal, and takes the screen back when it exits (keys
typed meanwhile are dropped). A missing `SYSTEM.md` is created with a
one-line comment header, which alone counts as empty (the default prompt
stays). When an `AGENTS.md` changed, its new content (capped like a loaded
file) is appended to the live session as system context under
`# Instructions (updated from <path>)`, as `/remember` does, so the next
model call reads it; `SYSTEM.md` applies from the next new session. It
works while a turn runs too.

**`/remember <text>`** (or a single-line `#text`; `##…`, multi-line input and
`\#…` are sent as prompts) appends `- <text>` to the workspace `AGENTS.md`
(created with an `# AGENTS.md` header if missing) and appends it to the live
session as durable system context under `# Remembered instruction`, so the
model reads it from its next request; it is fully applied, as part of
`AGENTS.md`, from the next session on.

**Notes** are small Markdown files the agent keeps across sessions with
`note_save`, `note_read`, `note_list` and `note_forget`. Workspace notes live
in the workspace's state directory (`<state>/notes/`), user notes (personal preferences that
follow you into every project) in `<config-dir>/notes/`; each directory has a
generated `MEMORY.md` index. The system prompt carries only a capped index,
labelled as untrusted context an earlier session wrote; the agent reads full
notes on demand. Reads are tier 1, workspace writes tier 2, user-note writes
go to the classifier.

**Semantic memory** is Meerkat's own: when a compaction discards messages,
they are indexed into the realm's semantic index (`<realm>/memory/`), and the
agent finds them again with `memory_search` (tier 1). The index is opened once
at start-up; when it cannot be opened, the session runs without
`memory_search` and a start-up notice says so.

**MCP servers** come from the realm config's `[[tools.mcp_servers]]`, the
project's `.rkat/mcp.toml` and the user's `~/.rkat/mcp.toml` (`rkat mcp add`
writes them), loaded once at start-up. They connect in the background (the
model sees a pending notice until they are ready); `--wait-for-mcp` waits for
them before the first turn. MCP tools are tier 3 unless a rule allows or
denies them by name. The project's `.rkat/mcp.toml` and the workspace
realm's `config.toml` may have come with the repository, so their servers
(which run with your privileges) start only once you trust them (`/trust`
or `--trust-project`; see
[What runs as the host](#how-approval-works)). Otherwise a start-up notice
lists the servers it did not start.

### Context and compaction

Compaction is Meerkat's native auto-compaction: before a model call the
runtime summarizes older history once the context crosses the session's
threshold, keeps the recent turns, and indexes what it discarded into
semantic memory. rkat-tui only chooses the threshold, from the context
window of the model's profile (the catalog, or the self-hosted alias the
endpoint listing keeps current):

- `compact = "native"` (the default): Meerkat's own threshold policy decides
  when to compact; rkat-tui sets no override. The gauge is advisory: it warns
  once when the context crosses `compact_warn_percent` (75%) and after every
  turn from `compact_auto_percent` (85%) on.
- `compact = "warn"`: the size trigger is off (Meerkat still compacts to
  recover a request the provider refused as too large) and the gauge warns as
  above; compaction happens only on `/compact`.
- `compact = "auto"` (or `--auto-compact`; `--no-auto-compact` forces
  `warn`): the threshold is `compact_auto_percent` of the model's context
  window, and the runtime compacts before the first model call that would
  cross it. A model switch or resume that moves the threshold rebuilds the
  idle session; a model list arriving in the background never does (the new
  window applies at your next switch).

For a model whose window is unknown the thresholds are 120k (warn) and 150k
(auto) tokens. `compact_auto_percent` below `compact_warn_percent` is raised
to it, with a notice.

`/compact status` shows the policy and threshold, and the prepare
instruction (below) with its threshold and whether it is armed.
**`/compact`** (or `/compact now`, after at least one turn) compacts the
conversation by itself. During a turn it does not wait: the request goes
at once and the engine compacts before the run's next model request ("the
context will be compacted before the agent's next model request"). The
session is not rebuilt and its threshold does
not change. Meerkat compacts only at the check before a run's next model
request and has no call that compacts an idle session, so rkat-tui asks
it to compact (`mob/compact_member`) and then starts that run itself with
a short prompt of its own (`[rkat-tui /compact] …`). The run costs the
summary and one short model call after it; the model sees the summary,
that prompt and its one-word reply, which stay in the conversation, and
rkat-tui shows neither (live or restored): only the compaction's lines,
and `compacting… Ns` in the status bar while the compaction runs (any
compaction, also the engine's own). When the check finds nothing to fold
the transcript says "nothing was compacted" with the reason. A prompt
typed while that run goes is held (*will send after this turn*) and sent
when it ends; one typed while the request is still on its way starts
an ordinary turn, which compacts first instead.

Every compaction (the engine's own, `/compact`, or one the agent asked
for with `compact_ready`) is one folded line in the transcript:
`✓ compacted: 379 → 68 messages (3.2k summary tokens) ▸`. A click on it
or Ctrl-O shows the summary the agent goes on from (read from the
conversation's record once the compaction is committed); nothing else
of the summary is drawn. A conversation opened again (`/resume`, `-c`,
an attach) shows the same line, `✓ compacted: the earlier conversation,
summarised ▸` (the figures are not stored), with the summary. `/export`
writes it under its own heading.

`/context` prints the gauge (tokens used of the window, the percentage,
and whether either is an estimate), the policy `/compact status` prints, and
what the context holds as the daemon reports it: the system prompt's size,
the message counts and the tool definitions every model request carries,
in bytes and tokens per family (the engine's built-in, comms, memory, mob
and schedule tools, rkat-tui's file, note, skill and watch tools and its
`tool_search` under `catalog`, each MCP server's), and the tools still
hidden until a search reveals them ([Tools an agent
reveals](#tools-an-agent-reveals)); the skills directories with their
skills and whether the agent's inventory names them ([Skills](#skills)); and the instruction files on disk (`SYSTEM.md`, the workspace
and parent `AGENTS.md`, `AGENTS.local.md`). The daemon reads a
conversation's tool definitions while its agent is idle, so a brand-new
conversation says "not read yet" until its first turn ends. Sizes in
tokens are estimates (four bytes a token).

`/compact <instructions>` compacts the same way. Claude Code uses such text
to focus the summary; Meerkat's compaction takes no instructions yet, so
rkat-tui says so in a warning and compacts without them.

Nothing is added to the conversation after a compaction. The agent's
instructions (and each helper's) carry a short section, "After a
compaction", that says what to do then: carry on with the task where it
left off rather than report the compaction or wait for you; earlier
details are found with `memory_search` (said only when semantic memory is
on); call `contacts` and retire the helpers it no longer needs. That a
compaction just happened, the model sees in Meerkat's own summary
message. A note appended after each compaction would be a system row,
which every compaction keeps, so a long conversation would collect one
per compaction; the instructions are one replaceable section that
compaction keeps and an upgrade refreshes. A conversation an earlier
build compacted keeps the notes that build added (they are its history);
its instructions are brought up to this section on its next seat.

#### Preparing for a compaction

A compaction keeps a summary and the recent turns; what the agent had in
mind and wrote nowhere is gone. `[auto] compact_prepare` is an
instruction the agent gets first, for example "update PLAN.md with where
you are and what is next":

```toml
[auto]
compact_prepare = "update PLAN.md with where you are and what is next"
compact_prepare_at = "70%"   # or tokens: 300000; default 70%
```

When the context of a workspace agent's conversation reaches
`compact_prepare_at` (tokens, or a share of the start-up model's window;
of 200k when the window is unknown), the daemon asks the agent, steered
into its run, and leaves it to pick the moment: "Your context is at 300k
of 1M tokens and needs compacting soon. At the next good point in your
work (finish the step you are in; do not start a large new piece of work
first): <your instruction>. Then call `compact_ready`, which compacts the
context. After it, carry on with the task as if nothing had happened: do
not stop, sum up or wait for the user." The transcript says "context at 300k: asked the agent to prepare for
compaction", and from then until `compact_ready` or the automatic
compaction `/compact status` says "compaction requested of the agent at
300k tokens; it prepares when it reaches a stopping point". If the model
call that crossed it ended the turn, the agent is idle: it is not woken,
it is asked at the start of its next turn. When the agent calls
`compact_ready` (its one signal: there is no separate "starting" call)
the daemon requests the compaction (as `/compact` does) and the tool
answers "compaction requested: it happens before your next step. Carry on
with your task now; do not end your turn to report the compaction."
(with semantic memory on, "Details from before it stay searchable with
memory_search." follows); the usual compaction notices follow. The agent
is meant to go on working as if nothing had happened: the request, the
tool's answer and its instructions all say so, so your instruction need
not. The compaction keeps the run under way as it was, so the model's
next request holds the summary, the request and the tool's answer, last. The agent may call `compact_ready` on its own too: it
compacts unless the context is below 20% of the window (then nothing is
compacted and it is told why).

What bounds the agent's choice of moment: if the context grows past the
midpoint between `compact_prepare_at` and the automatic compaction's
threshold (or by a further 10% of the window when nothing compacts
automatically, `compact = "warn"`) without `compact_ready`, it gets one
reminder ("Compaction happens automatically at 800k tokens whether or not
you have prepared: do it now: <your instruction>, then `compact_ready`.";
the transcript says "reminded the agent to prepare for compaction now"),
never a second for the same crossing; and the automatic compaction
itself comes at its threshold regardless.

It fires once per crossing and is armed again when the context is below
the threshold (after the compaction, or in a new conversation). The
automatic compaction stays as it is: an agent that ignores the
instruction is compacted at the usual threshold. If `compact_prepare_at`
is at or above that threshold (the engine's own, 80% of the window, in
the default mode; `compact_auto_percent` with `compact = "auto"`), a
start-up notice says the instruction would never run first and what to
change. Helpers and forks get no instruction and have no `compact_ready`.
The keys may come from your own `tui.toml`, the realm's (what
`/compact prepare` writes), `--set` (`--set 'auto.compact_prepare=update
PLAN.md'`, `--set auto.compact_prepare_at=60%`), or a project's
`.rkat/tui.toml` where the command line does not set them, as the other
compaction keys. The threshold reads as tokens (`300000`, `300k`,
`1.2m`) or a share of the window (`"65%"`).

**From inside the TUI.** Each form is a shorthand for `/set` of
`auto.compact_prepare` and `auto.compact_prepare_at` (see
[Settings](#settings)): written for this workspace to the realm's
`tui.toml`, or with `--user` to your own; when the value in force
changes it applies at once, without a restart, and arms the trigger of
every agent against what is now set:

- `/compact prepare <text>` sets the instruction; `/compact prepare off`
  turns it off for this workspace, also when your own `tui.toml` has one
  (written as an empty `compact_prepare`, so it holds across restarts);
  `/compact prepare default` removes the workspace's and your own
  `tui.toml`'s applies again at once (or none).
- `/compact prepare at <N | Nk | N%>` sets the threshold (`at 300k`, `at
  1.2m`, `at 65%`); `/compact prepare at default` removes the
  workspace's and the lower layers' applies at once (70% when none says).
  Something after `at` that starts like a number but is no threshold is
  refused with the accepted forms and an example; `at` followed by words
  stays part of an instruction ("at the next stopping point, …").
- `/compact prepare at 65% <text>` sets both, the threshold first;
  `/compact prepare default all` resets both.

`/compact prepare` alone prints what is set, its state and the three
forms. The confirmations say what [every change](#settings) says, and
what the agents are now asked and where ("the agents are asked at 65% of
the window (about 650k tokens for <model>) to: "update PLAN.md""); `/compact
status` names where each value in force comes from: from this
workspace, from your tui.toml, from the command line, from the project's
.rkat/tui.toml, or the default. Off says what `default` would bring back
("off for this workspace (your tui.toml has one: /compact prepare
default uses it)"). A threshold at or above the automatic compaction's
is applied with the start-up notice's warning. A value from the command
line or the project's file is over the workspace's: a change is written
but not applied, and the answer names the layer that wins.

## Skills

A skill is a named piece of instructions Meerkat lists in the system
prompt under `<available_skills>`. An agent lists skills from three
places:

1. **Yours**: `~/.rkat/tui/skills/` (`<config-dir>/skills/` with
   `--config-dir`). Keep a skill here once and every workspace's agents,
   and their helpers, list it. It is in the config directory,
   so neither the agent's shell nor its file tools can read or change it;
   the daemon reads it when it builds a session and when the agent opens a
   skill. A missing directory is no error.
2. **Repositories** in the realm config (`~/.rkat/tui/workspaces/<id>/config.toml`),
   Meerkat's own `[[skills.repositories]]` (a directory, a Git
   repository, an HTTP source), listed after yours.
3. **The project's** `<workspace>/.rkat/skills/`, last. It is read
   whenever it exists, as the project's `AGENTS.md` is: a skill is text,
   not something that runs, and `/trust` does not cover it (it covers the
   MCP servers and hooks a repository brings).

A skill directory holds one directory per skill, named by the skill:
lowercase letters, digits and single dashes (`code-review`, not
`Code_Review`). Each holds a `SKILL.md` that starts with YAML frontmatter
between two `---` lines: `name` (the directory's name, exactly) and
`description` (one line the agent sees in the inventory) are required;
`version`, `metadata` (a map of strings) and `requires_capabilities` are
optional, and any other key must be namespaced (`myteam.owner`). The
text after the frontmatter is the skill's body. A skill whose
frontmatter does not parse, or whose `name` differs from its directory,
is left out (the daemon's log names it). Put skills directly under the
skills directory: Meerkat finds a skill nested deeper but cannot load it.

```text
~/.rkat/tui/skills/
└── code-review/
    └── SKILL.md
```

```markdown
---
name: code-review
description: How I want a change reviewed before I merge it.
---

Read the whole diff first. Check error handling and tests before style.
Say what you would change and why, most important first.
```

**Precedence.** Meerkat names a skill by its source and its name, so two
sources may each have a `code-review`. A project skill with the name of
one of yours is not listed: yours is, and a request for the project's
gets yours. A cloned repository cannot stand in for your skill. Skills of
other names from every place are listed side by side.

**Settings.** Skills are on unless the realm config says
`[skills] enabled = false`, which turns all of them off, yours included.
If you list your directory in `[[skills.repositories]]` yourself, that
entry is used as it is, not doubled.

**What the agent sees.** Every agent's and helper's system prompt has
the inventory, one entry per skill with its source and name and the
`description` from its frontmatter:

```text
<available_skills>
  <skill source_uuid="00000000-0000-4b11-8111-000000000001" skill_name="shell-patterns">
    <description>Background job patterns with shell and job management tools</description>
  </skill>
  …
  <skill source_uuid="6f1c2a9e-4b37-4d05-9c1e-7a2b5d8f3e01" skill_name="code-review">
    <description>How I want a change reviewed before I merge it.</description>
  </skill>
</available_skills>
```

Meerkat's own embedded skills come first (12 of them for an agent), then
yours (always under the source id above), the repositories' and the
project's. Each entry costs every model request about 120 bytes plus its
name and description, so keep the description to one line. The
inventory names skills only while there are at most `[skills]
inventory_threshold` of them; above that it says only how many each
source has (`<collection source="…" count="3">`). Meerkat's default
threshold is 12, which its own skills already fill, so rkat-tui raises it
to 32 when the realm config leaves it at that default (it is never
written): room for 20 of your and the project's skills. Set
`inventory_threshold` in the realm config to choose otherwise (a value
of 12 counts as unset).

**How the agent opens a skill.** With `skill_read`: given a skill's
name it returns that skill's `SKILL.md` body (yours when the project has
one of the same name; `source` picks between two other sources that
share a name), at most 32 KiB of it; without a name it lists every skill
with its source and description, which is how the agent finds them when
the inventory shows only counts. An unknown name gets an error naming
the skills there are. `skill_read` reads through Meerkat's skill sources,
never a path, so the agent still cannot reach your skills directory with
its shell or file tools. It is read-only and allowed without asking in
every permission mode, plan mode included; helpers have it too. The
agent's instructions say that skills in `<available_skills>` are opened
with `skill_read`. Meerkat's own `browse_skills` and `load_skill` stay
off: the engine offers a host no switch for them (an engine ask).

**Check** with `/context`: it names your skills directory and the
project's (when there is one) with how many valid skills each holds, and
whether the agent's inventory names all its skills or only counts them;
or it says that skills are off.

## Keys and commands

**Claude Code parity.** The keys, the commands and the approval prompt
follow Claude Code's wherever both have the feature: Enter queues while the
agent works and Ctrl-X Ctrl-S sends now, Esc interrupts, Shift+Tab switches
the mode, `?` shows the keys, `/permissions`, `/usage`, `/context` and
`/compact` mean what they mean there, and the approval prompt offers the
same three numbered choices. Where rkat-tui differs or lacks a feature, and
why, is in [`docs/ux-parity.md`](ux-parity.md), which compares the two
feature by feature.


`/help` (alias `/?`), `/quit` (aliases `/exit`, `/q`), `/detach`, `/agents [name]`, `/audit [n]`, `/schedules [pause|resume|delete <name>]`, `/daemon stop`, `/daemon start`, `/clear` (alias `/new`), `/resume [ref]`,
`/permissions [blocked|rules|list|add allow|deny <tool> [pattern]|rm <n>]` (`remove` for `rm`; alone, it opens the dialog of the calls the gate recently blocked and of the rules, grants and statements in force; see [The permissions dialog](#the-permissions-dialog)),
`/mode [default|accept-edits|plan|auto]`, `/auto [on|off|resume|stats|check [all|<n>|<case id>…]]` (see [Checking your classifier model](#checking-your-classifier-model)), `/model [[endpoint:]id|refresh]`,
`/remember <text>`, `/memory [workspace|user|system]`, `/init`, `/compact [now|status|prepare [at <threshold>|default] [<text>|off|default]|<instructions>]`, `/context`, `/status`, `/export [file]`, `/usage` (aliases `/cost`, `/stats`), `/copy [N]`, `/trust`,
`/helpers [list [all]|expand|fold <label>|all]` (alias `/tasks`). `/status` prints the
rkat-tui version, the model and its endpoint, the session id, the workspace
and realm directories, the shell confinement, the permission mode, the
realm `tui.toml`, the trust state of each project input `/trust` covers
(`.rkat/mcp.toml`, `.rkat/config.toml`; "trusted for this run" under
`--trust-project`), and
the helper depth limit. `/export [file]` writes the transcript as plain text
(a `## user`, `## assistant`, `## tool …`, `## notice` header per block;
folded reasoning and collapsed tool output keep only their headers, as on
screen) to `file` (relative to the workspace; owner-readable only), by
default `export-<session id>-<unix time>.txt` in the workspace's state
directory (the notice names the file).
It is rkat-tui's own write, like the history file, not a tool call. `/init` sends a
fixed prompt as an ordinary turn, shown in the transcript: inspect the
project and write or update the workspace `AGENTS.md` with the build and
test commands, the layout and the conventions, under 200 lines. The agent
writes it with its file tools, so the usual approval, protected-path and
sealed-path rules apply. The transcript is replaced only once the new
session is live.

#### Commands during a turn

`/model`, `/clear` (`/new`), `/resume`, `/init`, `/agents <name>` (and
the agent picker's switch and new agent) and `/rehome <ref>` need the
agent idle. Typed while a turn runs (or a `!` command, or a compaction),
each waits in the queue with the messages, shown as its command text with
"when the turn ends", and runs once the agent is idle (the daemon's idle
note, not a guess), its result or failure reported then as usual. The
queue keeps the order you typed: `/model x` followed by a message
switches first and sends the message on the new model (a message typed
after a waiting command waits too, instead of joining the running turn).
↑ on an empty input takes the newest one back into the editor, as it does
a queued message. `/model`, `/resume` and `/agents` without an argument
open their picker at once; the choice waits. `/compact` does not wait
(see [Context and compaction](#context-and-compaction)). Quitting or
detaching with commands still waiting drops them, with a notice naming
them.

#### A side question: `/btw`

`/btw <question>` asks about this session without touching the agent's
run: while the agent works (also inside a long command) or when it is
idle. It is not held for the turn's end and never sent to the agent. The
daemon makes one model call of its own, on the agent's current model and
endpoint, with no tools; it reads the session's stored history (past a
budget of about 60 KB the first user message and the most recent part,
with a line saying the middle was left out), the running turn as your
transcript shows it (the stored history has a run only once it ended),
and the name of the tool the agent is running and how long it has run.
It is told to answer briefly from that record, that it cannot act, and
to say so when the answer is not there (for example the output of a
command that has not finished). The answer shows as a dim `↳ btw:` block
under the question: a side note, not the agent's turn. Nothing is written
to the agent's session, so the agent never sees the question or the
answer; `/export` keeps the note, labelled as a side question, but
showing the conversation again (a re-attach, `/resume`) does not have it.

While one is pending the status bar says `btw: asking… Ns (Esc cancels)`,
and the first Esc cancels that question only; the next Esc is the usual
interrupt. One `/btw` at a time: a second while one is pending is refused
with a line. A failure (no model client, the provider's error, no answer
within a minute) is a line in the block. Its tokens count under the
session's usage and show in `/cost`. In a helper's full view it still
asks about the agent this client works with, and says so.

#### List dialogs

The lists of `/permissions`, `/schedules`, `/agents`, `/wire`,
`/unwire`, `/mode`, `/memory`, `/settings` and `/helpers` share their keys:
↑/↓ (or k/j) move, each row's keys act on it, Esc, `q` or Ctrl-C close.
A list stays open after what you do in it, and closes only on those keys:
its rows come again in place (from the daemon, once its answer is there;
`/helpers` follows its helpers as their state changes, a stopped one's end
included; `/memory` once the editor closes), the highlight on the same
item wherever it moved, on the row now in its place when the item is gone.
Only the acts that take you somewhere else close it: a mode chosen in
`/mode`, Enter on a helper in `/helpers` (its full view), and switching
to an agent or starting a new one in `/agents`; so do choosing a model in
`/model` and a conversation in `/resume`. A helper's full view also stays
after its Ctrl-C and `y`, showing the helper's end. Rows wrap
to the dialog's width: the highlighted row in full, every other at most
two lines of its text and two of its note, ending in "…". A key that asks
first (retire, delete, remove, unwire, `/permissions`' `s`, `a` and `A`) puts its
question on a line of its own in the warning colour, naming what will
happen and its keys ("Delete schedule nightly? It never fires again. y
yes · n or Esc no"). While it is up only `y` (do it), `n` and Esc (cancel
it; a second Esc closes the dialog) act; Enter, the arrows and every other
key leave the question up and say "press y to confirm, n or Esc to
cancel". What was done is stated under the rows ("retired agent
main-ab12", "deleted schedule nightly") and in the transcript, and the
list shows the change.

Typing `/` at the start of a one-line input opens a menu of the commands
whose name or alias starts with what follows (case-insensitive) right above
the input, each as `/name usage — summary`, up to eight rows at a time
(the list scrolls with the highlight). ↑/↓ choose a row (not history while
the menu is open). No menu opens over an entry ↑/↓ recalled from the prompt
history until you edit it (moving the cursor is no edit), so ↑/↓ go on
through the history past a recalled `/helpers` or `… @src/main.rs`; the
first edit opens it, and ↑/↓ are then the menu's. Tab puts the highlighted command and a space in place of
the word, Enter does the same for a partial name and sends a complete one
(with its arguments) as before, and Esc closes the menu for that word;
typing reopens it. `//` never opens it, and nor does a word nothing
matches. The parser and `/help`'s command list are generated from one
table (`COMMANDS` in `src/ui/commands.rs`), which the menu reads too.

`@` mentions a workspace file or directory. While the cursor ends a word
that starts with `@` (anywhere in the input; `a@b` is not a mention), the
same menu lists up to eight workspace paths: those that start with what
follows the `@`, then those whose name contains it (case-insensitive),
directories with a trailing `/`. The paths are the ones the `glob` tool sees
(`.gitignore` honoured, hidden files included), minus the sealed `.git` and
`.rkat` and the realm and user config directories; the list is built when
the menu opens (about 4 ms for this repository, 63 ms for 20 000 files in a
debug build, about 11 ms in a release build). Tab or Enter inserts the
highlighted path; a file gets a space after it and closes the menu, a
directory keeps it open on what is inside; Enter on a fully typed file name
sends. When the prompt is sent, every `@path` naming a file inside the
workspace is attached after the prompt as `<file path="…">` with its
content (UTF-8 text, at most 64 KiB each with a `[truncated]` line, 256 KiB
in all; a binary file or one past the budget is a one-line `<file path="…"
note="…" />`), and a directory as a `<directory path="…/">` listing of its
entries (at most 200). The transcript shows the prompt as typed with a dim
`+ N files (…)` line under it; a sealed path is never read and gets a
warning instead; a word that names nothing (`@someone`) stays plain text. What
an attached file says is data, not your words: the approval classifier
records only what you typed as your request, and the attachments as a
`[context]` line naming the files (their content is not shown to it), so
an instruction planted in a mentioned file cannot pass for your consent.

**Shell mode.** An input whose first line starts with `!` is a shell
command: `!ls -la` (or `! ls -la`; the rest of the input, later lines
included, is the command). The editor title says so while you type it.
Enter runs it **as you, with no sandbox**: your shell (`$SHELL`, else
`/bin/sh`), the workspace as working directory and your whole
environment, as any program you start from your terminal would run, so
`!git push` or `!cargo publish` work with your credentials. It is not
the agent's shell: `--confinement` and the curated environment apply to
the agent's commands only. There is no approval prompt, since you typed
it. It may run for `[shell]
user_timeout_secs` (default 120; `--set shell.user_timeout_secs=N`; a
project file may only lower it), and Esc or Ctrl-C cancels it; whatever it
started is killed when it ends. Its output, stdout and stderr together in
arrival order and capped at 64 KiB (`[output truncated at 64 KiB]`), shows
as a `$ <cmd>` block with the exit code, folded and unfolded like tool
output (a click, or Ctrl-O for all), and joins the session as durable system context
(`[shell run by the user] $ <cmd>`, the output, `(exit N)`) through the
same call `/remember` uses: the model reads it on its next call, and no
turn starts for it. While the agent works, a `!` command waits as a queued
`$ <cmd>` line and runs when the turn ends, before the queued messages are
sent (several run in order); messages typed while a command runs wait for
it the same way, and ↑ takes a waiting command back into the editor. `!!`
at the start is sent as text, and `\!` sends a literal `!`.

The keys, as `?` on an empty input lists them (the keys part of `/help`;
the table below is the same list, which the source keeps in `KEYS_HELP`
in `src/ui/commands.rs`, pinned to the keymap by a test):

| Key | What it does |
|---|---|
| `?` (empty input) | Show the keys; any key closes them (with text, `?` is typed). The idle, empty editor title says `? keys` |
| Enter | Send; while the agent works, queue the message and deliver it at the next step (a steer, see [Sessions](#sessions)) |
| Shift+Enter, Alt+Enter, Ctrl+J | Insert a newline; so does a `\` typed right before Enter at a line end (the backslash is dropped) |
| Ctrl-C | Interrupt the turn (as Esc), else clear the input, else press twice to quit |
| Esc | Interrupt the turn (the agent's own run: its helpers keep working); your messages the agent has not read come back to the input; again within 1 s it says when the next forces it, later it forces that run, and after the run ended it only says the turn is already stopped (see [Sessions](#sessions)). The first Esc or Ctrl-C after a mouse selection only clears it. In a running helper's full view: ask whether to stop that helper (`y` stops it, `n` or Esc keeps it) |
| Esc Esc (idle, with text) | Clear the input and save it to the prompt history (↑ recalls it) |
| Up (empty input) | Take queued messages back into the editor |
| Ctrl-D | On an empty input, quit on a second press within 3 s (see [Sessions](#sessions) and [Detach and reattach](#the-workspace-daemon)); with text, delete forward |
| Ctrl-O | Show everything in full, or fold it all back: every tool call's arguments, output and diff, every reasoning block, every job outcome, in the transcript and an open helper view; while on, new ones arrive shown (a click toggles one until the next Ctrl-O). Expanding or folding, by Ctrl-O, a click or `/helpers`, keeps the line at the bottom of the screen where it is: what is above it grows or shrinks upwards, and folding again brings the view back as it was |
| Ctrl-T | Show or hide the [task checklist](#task-checklist) |
| Ctrl+Up/Down | Select a helper in the panel under the input (newest first); Enter on an empty input opens its full view: its task and later inputs with who gave them, its work and its answer, under a title with its kind, state and model (Esc returns at once, after a pending `/btw` takes the first; Ctrl-C asks whether to stop a running helper, `y` stops it (the view stays and shows its end), `n`, Esc or Ctrl-C keep it and stay; PgUp/PgDn scroll it) |
| Ctrl-R | Search the prompt history, the workspace's then the global one (Ctrl-R again: older; Enter accepts; Esc cancels) |
| Ctrl-X Ctrl-S | Send now: interrupt the turn and send your waiting messages, then the input, as the next run (sends normally when idle; see [Sessions](#sessions)) |
| Ctrl-G, Ctrl-X Ctrl-E | Edit the input in `$VISUAL`, else `$EDITOR`, else `vi` (also while a turn runs) |
| Up/Down, Ctrl-P/N | Move between lines (wrapped rows first), then recall prompt history; an entry recalled and not edited (a `/command` or `@path` among them) opens no completion menu, so the walk goes on past it |
| Ctrl-A/E, Alt+B/F | Line start/end, word left/right (Ctrl/Alt+Left/Right too) |
| Ctrl-K/U, Ctrl-W, Alt+D | Kill to the line end (at a line end, the newline) or start, the word before or after; Ctrl/Alt+Backspace = Ctrl-W |
| Ctrl-Y, Alt+Y | Yank the last killed text; Alt+Y right after a yank cycles to older kills |
| Ctrl-S | Stash the input (put it aside, clear); on an empty input, restore the stash (Ctrl-X Ctrl-S still sends now) |
| Ctrl-_ | Undo the last input edit (Ctrl-7 or Ctrl-Shift-- in some terminals) |
| PgUp/PgDn, Shift+Up/Down | Scroll the transcript |
| Ctrl-L | Clear the screen and redraw it |
| Ctrl-Z | Suspend to the shell (Unix; `fg` resumes) |
| Shift+Tab | Cycle the permission mode: manual → accept edits → plan → auto (see [Permission modes](#permission-modes)) |
| Alt+P | Pick a model (as `/model`) without clearing the input |
| `/` (one-line input) | The command menu (above) |
| `!` (first character) | Shell mode (above) |
| `@` (ending a word) | The path menu (above) |
| Home/End | Scroll to the top or bottom when the input is empty (Ctrl+Home/End always) |

In the approval prompt: `1` or `y` allows once, `2` or `a` allows and adds
the rule the row shows, `3`, `n` or Esc denies (type what the agent should
do instead and Enter sends it with the denial), Up/Down, `j`/`k` and Enter
move and pick, and `s` stops the turn, or on a helper's prompt that helper
(Ctrl-C stops the open prompt and the others of its caller).

Details the table leaves out:

- **The editor.** The input wraps at its width and grows up to six rows,
  then scrolls to the cursor. Killed text goes onto a kill ring of 16
  entries (consecutive kills join one entry). Undo takes a word of typing,
  a run of deletes, a kill, a yank, a paste or a clear (Ctrl-C included)
  at a time, up to 64 steps; sending a prompt starts afresh. Stashing over
  a stash moves the older one to the prompt history.
- **Long pastes.** A paste of more than one line, or of one line over 80
  characters, shows in the input as one chip, `[Pasted text #N, C lines]`
  (`C chars` for one long line), numbered from 1 for each message (a
  chip killed or stashed before a send keeps its text, and its number
  counts on). The chip
  edits as one character: the cursor crosses it in one step, Backspace,
  Delete and the kill keys remove it whole, and undo and yank bring it
  back; text typed around it stays. When the input leaves the editor (a
  message, a steer, a `!` line, a command's argument, the external editor)
  each chip is replaced by its text in place, so the agent, the
  transcript, the prompt history and the unsent-message store get the
  real content. A short one-line paste is inserted as text, and the
  dialogs' one-line fields take pastes as before. A character of
  Unicode's supplementary private use planes (15 and 16, where the chips
  live) that comes into the input any other way, typed, pasted short,
  recalled or restored, becomes U+FFFD.
- **The external editor** (Ctrl-G) edits a private temporary file in the
  realm directory's `drafts/`; the edited text replaces the input when the
  editor exits successfully (trailing newline dropped; Ctrl-_ restores the
  old draft), and a failed run keeps the draft with a notice. The session
  goes on meanwhile and its output appears when the editor closes.
- **Tables in answers** fit the transcript's width with nothing cut off.
  When a table's columns are too wide, the widest shrink (none below six
  columns) and their cells wrap within the column, so a row takes several
  lines; a table with too many columns for that is shown stacked, one
  `header: cell` line per cell. Resizing the terminal lays it out again.
- **Ctrl-Z** gives the terminal back and stops rkat-tui with SIGTSTP; `fg`
  repaints the screen. The agent runs in the daemon, so its work goes on
  while the TUI is stopped.

The mouse is captured by default: the wheel scrolls and a click expands or
collapses one tool call (Ctrl-O does all of them). Dragging selects text
and copies it on release (double click: a word, triple click: a line;
`/copy` copies the latest answer, `/copy N` the Nth latest). Hold Shift for native
selection (the terminal's own, past rkat-tui and tmux alike), start with
`--no-mouse` for one run, or set `ui.mouse = false` in `tui.toml` (`[ui]`
section; read when the TUI starts) to leave the mouse to the terminal for
good; `--no-mouse` wins over every file.

| Mouse | What happens |
|---|---|
| drag, double or triple click | selects and copies: the terminal's clipboard and its primary selection; inside tmux also tmux's paste buffer |
| middle-click | pastes what rkat-tui copied last in this session into the input at its cursor (as a paste: lines kept, nothing sent; into a dialog's text field when one is open); with nothing copied yet it says so |
| Shift+drag, Shift+middle-click | the terminal's (or tmux's) own selection and paste, past rkat-tui |
| `ui.mouse = false` or `--no-mouse` | everything is left to the terminal and tmux |

A middle-click inside rkat-tui cannot paste the terminal's primary
selection or tmux's buffer: the terminal reports the click, not its
selection, and the one way to ask for that (an OSC 52 query) is refused
or prompted for by most terminals, so rkat-tui never sends one.

Copying uses OSC 52 only, so the terminal must support it: kitty, foot,
WezTerm, Alacritty, iTerm2 and xterm (with `allowWindowOps`) do; GNOME
Terminal and other VTE-based terminals do not. rkat-tui writes one
sequence for the clipboard (`52;c`) and one for the primary selection
(`52;p`, middle-click paste), so a terminal without a primary selection
still gets the clipboard. Over ssh the sequences travel back to your
terminal like any other output; nothing on the remote side needs to know
about the clipboard.

Inside tmux (`$TMUX` set) rkat-tui writes both plain sequences to tmux
itself, then both again in tmux's passthrough. The plain ones come first,
so a `$TMUX` with no tmux in between (one passed on over ssh) still sets
your terminal's clipboard and primary selection:

- With `set -g set-clipboard on` tmux makes the copy a paste buffer
  (`prefix ]`, and a middle-click in tmux with its mouse on, paste it) and
  sets your terminal's clipboard the way its own copy mode does. This is
  the setting to use, and it is what works when tmux's own mouse
  selection already reaches your clipboard. tmux's default, `external`,
  ignores a program's clipboard sequence.
- With `set -g allow-passthrough on` (tmux 3.3 or later) the passthrough
  copies reach your terminal directly, also under `set-clipboard
  external`; tmux's buffer is not set then.
- A tmux on your own machine with rkat-tui on a remote host behind ssh
  (no `$TMUX` on the remote side) gets the plain sequences, which
  `set-clipboard on` handles the same way.

The first copy inside tmux says what tmux needs (``copied N chars (inside
tmux this needs `set -g set-clipboard on` (or `allow-passthrough on`) in
tmux.conf; Shift-drag selects with the terminal instead)``), later ones
`copied N chars (through tmux)`; rkat-tui cannot read tmux's options, so the
notice says the copy was written, not that it arrived. Outside tmux it says
`copied N chars (terminal must support OSC 52)`, and `copy failed:
<error>` when the sequence could not be written. Under GNU screen (`$STY`)
the sequences go out in the small pieces screen passes through. A copy is
cut at 100 KiB of encoded text (the notice says so).

### Task checklist

The agent keeps a task list with its `task_create`/`task_update` tools
(they need no approval). Ctrl-T shows it under the input, before the helper
panel: one line per task, `☐` pending, `◐` in progress (bold), `☑` done
(dim), the most recently updated last. Five lines at most: with more tasks,
the four updated last and `… and N more`. Nothing shows while the session
has no tasks; the idle editor title says `Ctrl+T shows the tasks` once it
has some. The list is the engine's own task store (the realm's `tasks.db`,
scoped to the session), read when a session opens and after each task call,
so a resumed session gets its list back and a new one starts empty. Ctrl-T
saves the choice as `[ui] checklist = "shown"` (or `"hidden"`, the default)
in the realm `tui.toml` (`/set ui.checklist shown --user` sets yours);
the daemon reads the list and saves the choice, and every attached client
follows it.

## Where things live

A workspace's state lives outside it, in a directory of its own under the
config directory: `~/.rkat/tui/workspaces/<id>/` (`<config-dir>/workspaces/<id>/`
with `--config-dir`), where `<id>` is random, drawn on the workspace's first
start. It holds the realm (sessions, config with the endpoint connections
and model profiles, `tui.toml` written by the UI, the approval issuer's
`approvals.json`, process custody, semantic index), the conversation log,
the per-conversation usage records the daemon writes (`tui_usage/`, owner-only), input history,
unsent messages, workspace notes (`notes/`), the attach cursors, the
external editor's draft files while Ctrl-G edits them (`drafts/`), logs
(`logs/rkat-tui.log`), `/export`'s default files and the audit log. The
directories are owner-only. Nothing of it is in the workspace, so neither
the agent's shell nor a cloned repository can reach it; what stays in the
workspace is what the project owns: `.rkat/tui.toml`, `.rkat/mcp.toml`,
`.rkat/config.toml` (hooks) and `AGENTS.md`, untrusted until `/trust` as
before.

Each state directory has a `workspace.toml` naming the workspace it belongs
to (its canonical path, its directory name, its `origin` git remote, when
it was made and last used). That file is the only thing that says which
workspace owns which state: nothing in the workspace is read for it. Two
of them naming the same workspace refuse the start, naming both; remove or
re-home one. `rkat-tui --list` lists every workspace's state.

**Moving or renaming a workspace** needs one re-home: one state and one
daemon serve each workspace, and the state's `workspace.toml` names it.
There are two ways.

- *Push*, from inside the TUI: `/rehome <directory>` moves this
  workspace's state to that (existing) directory. The daemon is stopped
  and started again there, and the same `rkat-tui` reattaches to the same
  conversation without quitting: a short reconnect, with a notice naming
  the new workspace. The agent's shell, its file tools, `!`, `@` mentions
  and the project's `.rkat/` files are the new directory's from then on
  (the workspace is fixed when a daemon starts, so it is a restart, not a
  switch). Other TUIs attached to the daemon are disconnected; started in
  the old directory again, they get a fresh state. The old directory is
  not touched.
- *Pull*, after a move: the first start in the moved directory begins
  with fresh, empty state and offers the old one in a notice (it is not
  adopted on its own). `/rehome <id or old path>` in that TUI adopts it:
  the empty state is removed, the daemon restarts on the adopted state
  and the TUI continues its latest conversation. `rkat-tui --rehome <id
  or old path>` does the same from the shell, without starting anything.

A daemon running on the state that moves is stopped (its agents come
back when it starts in the new place). A re-home is refused, changing
nothing, while a turn runs on a daemon it would stop (interrupt it or
wait), and when the destination already has state that holds anything: a
conversation with a turn of yours, or a daemon with another TUI attached.
State that cannot be read counts as holding something. An argument that
reads both ways (an existing directory that is also some state's
workspace) is refused naming both readings; `/rehome from <ref>` and
`/rehome to <directory>` say which. A directory that would contain the
state, or lie inside it, is refused. After any re-home, `rkat-tui -c` in
the new directory continues the latest conversation. `/rehome` alone
lists this workspace's state and the state whose workspace is gone.

A `.rkat/tui/` inside the workspace is never read (one a clone carries
or the agent's shell writes marks nothing).

The workspace is `--workspace <dir>` (`-w`), else the nearest ancestor of the
current directory that has state (the search stops at the git root and never
picks your home directory), else the current directory. Running from your
home directory requires an explicit `--workspace`. A workspace has exactly
one state directory, its own.

The agent's confined shell must not reach the state directory: the daemon
refuses to start when the confinement would grant it or a directory
holding it. `-w ~` with the default config
directory is such a case (the workspace holds `~/.rkat/tui`), and so is a
`--confinement-write` (or `[confinement] extra_write`) of `~/.rkat`; pass
`--config-dir <dir outside the workspace>`, or drop the grant.

User-global files live in one config directory: `--config-dir <dir>`, else
`~/.rkat/tui` (under Meerkat's own user config root `~/.rkat`;
`<--user-config-root>/.rkat/tui` when that flag is given). It holds
`tui.toml`, `AGENTS.md`, `SYSTEM.md`, `notes/`, `history` and the host trust
records (`trust/`). `$XDG_CONFIG_HOME` does not decide it. The price-list cache lives
in the user cache directory (`<config-dir>/cache` when `--config-dir` is
given).

**The audit log** is `audit.jsonl` in the workspace's state directory, so
it follows a re-home; owner-only, written by the daemon alone. It holds one JSON line per
approval decision (time, agent, helper, session, run, tool, the deciding
argument whole, tier, `allow` or `deny`, who decided: `rule:<id>`,
`classifier`, `human:<connection>`, `mode`, `policy`, `unattended`,
`timeout`, `review-failed`, and for a denial why) and one per change of authority (mode
switch, rule added or removed, `/trust`, an agent
retired, daemon start and stop). It is out of the agent's reach: its
confined shell is never granted the directory (the daemon refuses to
start otherwise) and its file tools refuse to read it.
Past 50 MB it is renamed `audit.1.jsonl` (one older generation is kept).
`/audit [n]` shows its last lines for your agent; `rkat-tui --audit`
prints the whole log, the older generation first.

**When the disk fills.** The workspace's state directory holds the agent's
conversation databases, so a full (or failing) filesystem under it stops
the agent from saving its work:

- The turn that hits it fails with one line: `the disk that holds
  rkat-tui's state (<the state directory>) is full or failing, so the
  agent's step was not saved; free space on that filesystem, then send
  your message again`. The engine's own error text goes to the daemon's
  log and the audit log (`storage_failure`), not the transcript. The
  step the agent was on when the write failed is lost; what it committed
  before stays.
- The engine then refuses the agent's input until its registration is
  reloaded. The daemon does that itself on your next message (and on its
  own notes to the agent), with the same conversation, and sends the
  message once more; you see `the agent was reloaded after a storage
  failure; the step it was on when the failure came was not saved`. No
  daemon restart is needed.
- If the reload fails too (the disk is still full), the turn says so:
  `the agent could not be reloaded after a storage failure (the disk may
  still be full) …; free space on the filesystem that holds <the state
  directory>, then send again`.
- The daemon reads the free space at its start and every 5 minutes; when
  it falls below 200 MB each agent's clients get a warning, once per
  crossing.

A helper refused the same way (a daemon note to it) is reloaded the same
way; the reload is in the daemon's log and the audit log
(`storage_reload`).

## The workspace daemon

`rkat-tui` runs the agent in the workspace's `rkat-tui-mob` daemon and is
its client: with no daemon running for the workspace it starts one, with
one running it attaches to it. The agent lives in the daemon (a durable
mob member: mob `tui`, member `main-<id>`), keeps working when you
`/detach`, and any number of TUIs can attach to it. A daemon hosts one
agent per `rkat-tui` you start beside it (see [Several agents](#several-agents)).

### Which daemon, which agent

A start answers two separate questions. `--attach` says which daemon
(by default this workspace's); `-c` or `--resume` says to join an agent
already in it, and without them you get a new agent there:

| | joins an agent | starts a new agent |
|---|---|---|
| this workspace's daemon | `rkat-tui -c` (or `--resume <ref>`) | `rkat-tui` |
| another daemon | `rkat-tui --attach=<dir or HOST:PORT> -c` | `rkat-tui --attach=<dir or HOST:PORT>` |

`-c` joins the agent that was active last, `--resume <ref>` the agent
that owns that conversation, and `--resume` alone the one active last,
with the conversation picker open. A new agent beside others says so in
its first notice ("started a new agent (main-ab12); 2 others work here,
2 idle without a client: /agents picks one").

Agents you left behind cost nothing while idle, but each keeps its
schedules and file watches, which can wake it. `/agents` lists them,
switches this TUI to one and retires an idle one nobody works with.
`kill <pid>` and `/daemon stop` keep every agent for the daemon's next
start; `/quit` as the last TUI, answered with terminate, retires them
all (their conversations stay for `/resume`).

The flags that choose the daemon:

| Flag | Meaning |
|---|---|
| (none), `--attach`, `--attach=auto` | The workspace's daemon; start it when none answers |
| `--attach=existing` | The workspace's daemon, only when it runs; refuse otherwise |
| `--attach=DIRECTORY` | Attach to the running daemon of the workspace at or above `DIRECTORY` (never starts one; replaces `-w`) |
| `--attach=HOST:PORT` | Attach to a daemon reached at that address (a tunnel), with `--attach-token-file`; a loopback address only, unless `--attach-insecure` |
| `--list` | List every daemon that has an endpoint file, then exit (also `rkat-tui-mob --list`) |
| `--audit` | Print the workspace daemon's audit log, then exit (see "Where things live") |
| `--classifier-check[=quick\|all\|N]` | Measure the daemon's classifier model on the built-in cases, print the report, exit 0 within the targets, 3 outside, 1 incomplete (see "Checking your classifier model") |

**Attaching to another workspace.** `--attach=DIRECTORY` (a value with a
`/`, or starting with `.` or `~`) finds that workspace as rkat-tui finds
its own (the nearest ancestor that has state, else the directory), reads its endpoint file and attaches; with no daemon there it
refuses before the TUI opens. Everything this TUI does locally then
happens in the daemon's workspace, not the directory you started in: `!`
commands run there, `@` mentions resolve there, `/export`, `/remember` and
`/memory` write there, and the input history is that workspace's.

**Attaching through a tunnel.** The daemon only ever listens on
loopback. To use one on another machine, forward a local port to it and
bring its token along:

```bash
# on the remote host: the daemon's address and token are in its endpoint file
rkat-tui --list                      # workspace, 127.0.0.1:DAEMON_PORT, pid, …
# on this machine
ssh -N -L 127.0.0.1:4100:127.0.0.1:DAEMON_PORT host &
scp host:.local/state/rkat-tui/<key>/mob.endpoint ~/.rkat/devbox.token
chmod 600 ~/.rkat/devbox.token
rkat-tui --attach=127.0.0.1:4100 --attach-token-file ~/.rkat/devbox.token
```

The token file holds the token alone or is a copy of the daemon's
endpoint file (`$XDG_STATE_HOME/rkat-tui/<key>/mob.endpoint`, see "Finding
the daemon"; `<key>` is the one `--list` shows the file under). It is
refused when its group or other users can read or write it (anything
but `chmod 600` or stricter): the token is the whole access control. Before the TUI opens rkat-tui attaches as a probe and reads the
daemon's status, which names its workspace and host. When an endpoint
file on this machine names that daemon (the token you gave, for that
workspace) and the workspace exists here, the TUI runs on that workspace
as `--attach=DIRECTORY` does; the same host name and path alone are not
enough, since two machines can share both. Otherwise the features that read
or write local files are refused with the reason: `!` commands, `@`
mentions (sent as typed), `/remember`, `/memory` and `/export` to a path
(`/export` alone writes into this machine's own workspace state). Prompts,
approvals, modes, rules, `/model`, `/new`, `/resume`, `/cost`, `/context`
and the pickers work as usual.

The token itself never travels: the daemon first proves it holds it (an
HMAC over two fresh nonces), and only then does the TUI prove it does the
same way, so a program listening on a port the daemon left behind gets
nothing it could attach with, and the TUI says the process there did not
prove it is the daemon. What follows the attach (your prompts, the
agent's output, the approvals) travels unencrypted, so `--attach=HOST:PORT` accepts a
loopback address only (`127.0.0.1`, `::1`, or a name resolving to one),
which is what the `ssh -L` recipe above gives: the network sees only the
encrypted tunnel. Any other address is refused with that recipe before
the token file is read; `--attach-insecure` attaches there anyway, for a
network you trust end to end. `--attach=DIRECTORY` and the
workspace's own daemon are unaffected.

The value needs `=`: `rkat-tui --attach "fix the test"` attaches and sends
the prompt.

**Listing daemons.** `rkat-tui --list` prints one line per daemon whose
endpoint file exists, tab-separated in a fixed order: the workspace, the
address, `pid N`, `up <uptime>`, then the attached clients and the
number of agents; under it, indented by a tab, one line per agent: its
name, `running`, `idle` or `unknown` (the daemon could not read its
state), its conversation, its clients and `detached`
when it runs without one. A file whose address refuses the connection is
`stale`, and is removed when its process is gone (`stale (file removed)`);
one whose address does not answer in time is `unknown (<why>)` and kept,
since a slow daemon is not a dead one. A daemon does not start beside such
a file either: it says so, and if no daemon runs, remove the file.
Listing attaches to each daemon with its token as a probe, which is not
counted as a client; the token is never printed. `rkat-tui-mob --list
--endpoint-file <path>` lists only that file.

**Finding the daemon.** The daemon writes its endpoint file (its loopback
address, a per-daemon token, its pid, the workspace and when it started) to
`$XDG_STATE_HOME/rkat-tui/<key>/mob.endpoint`, or
`~/.local/state/rkat-tui/<key>/mob.endpoint` when `XDG_STATE_HOME` is not
set; `<key>` is the first 32 hex digits of the SHA-256 of the workspace's
canonical path. The directory is created with mode 0700, the file with
0600, and the file is removed when the daemon shuts down. The TUI reads
it, connects, and attaches with `rkat/attach` before anything else (the
daemon proves it holds the token first, then the TUI does; the token
itself is never sent); then it speaks Meerkat's own JSON-RPC methods. A file whose address
no longer accepts is stale. `rkat-tui-mob --endpoint-file <path>` writes it
elsewhere.

The token is the only thing that keeps other programs out: whoever holds it
can answer approval dialogs, switch the permission mode and add rules. That
is why the file is not in the workspace: the agent's confined shell can
read the whole workspace (and the temp dir, most of the Cargo home and the cache
directory) and reach loopback. When the shell's confinement would grant
the state directory (for example a workspace that contains
`~/.local/state`, such as your home directory), the daemon writes the file
to `$XDG_RUNTIME_DIR/rkat-tui/<key>/mob.endpoint` instead (same modes); the
TUI and `--list` look in both places, in that order. The temp dir is never
used: the shell is granted it. When neither place is safe (or
`XDG_RUNTIME_DIR` is unset, or `--endpoint-file` names a granted path) the
daemon refuses to start; point `XDG_STATE_HOME` or `--endpoint-file`
elsewhere then. The agent's file tools never read either place (they are
sealed for reads too, without asking the classifier). With
`--no-confinement` the agent's shell runs as you and can read the file like
anything else of yours.

**Start on demand.** With `--attach` (auto) and no live endpoint, the TUI
starts `rkat-tui-mob` (the one next to the `rkat-tui` binary, else from
`PATH`) with the engine flags of this command line (workspace, model,
endpoints, `--set`, rules, confinement, trust, realm flags), detached from
the terminal: through `setsid` when it is on `PATH` (its own session, so
closing the terminal does not stop it), else in its own process group (a
shell that hangs up every process group of the terminal's session would
then stop it too; bash and zsh do not), stdin closed, its output appended to
`<realm>/logs/rkat-tui-mob.stderr.log`. The status bar shows `daemon
starting` until the endpoint file appears (up to 60 s); a daemon that exits
first, or never comes up, is reported with the log's path. You can also run
it yourself: `rkat-tui-mob -w . --model …`.

**Start-up notices.** What the daemon's start had to say (its shell's
confinement, rules it could not load, ignored project settings, semantic
memory being off) shows when a TUI first attaches to it, and again after
it restarted.

**Status bar.** While the agent is busy, its activity leads the bar
(`⠋ running 12s`, `⠋ classifying…`, `⠋ compacting… 4s`, or what it waits
for after a silence, see **Working or waiting**), so that a narrow
terminal, which cuts the bar's end, keeps it. Then comes the connection,
but only when it is not the normal attachment: `connecting`, `daemon
starting`, `reconnecting`, a failure, or `daemon stopped`. Attached, the
bar names neither the daemon's address, the agent nor the conversation:
`/status` prints each on its own line (`daemon: 127.0.0.1:<port> (pid
…)`, `agent: main-<id>`, `conversation: <id> (short <last eight>)`). A
daemon built from another commit than this TUI shows `daemon build
<version>+<commit>` there and says so once in the transcript (`/daemon
stop` and attach again runs the new build).

**A daemon started with other confinement flags.** The agent's shell
runs in the daemon, under the confinement the daemon was started with
(`/status` shows it); a TUI attaching with other `--confinement` flags
does not change it. To run the agent's shell with other flags, stop the
daemon (`/daemon stop`) and attach again with them.

**When the attach fails.** The segment says why, when the TUI tries
again, and where the daemon's output is, for example `daemon exited (exit
status: 3): rkat-tui-mob: create endpoint directory …: Read-only file
system · retry 2s · log ~/.rkat/logs/rkat-tui-mob.stderr.log`. A daemon that
exits before serving is quoted from what that start wrote to the log:
its own error lines, else its last lines (at most three, each cut at 300
characters; the error at start-up shows all of them, the segment the
last). A failed first
attach is retried three times with a growing wait (each try starts the
daemon again when this TUI may start it), then left with `/daemon start
retries`: fix what the log says and type `/daemon start`, which also
starts a daemon again after `/daemon stop`. Prompts typed meanwhile wait
for the retries and fail as not attached once the TUI gives up.

**Detach and reattach.** `/detach` leaves and keeps this TUI's agent
running: the daemon and a running turn go on, and the daemon marks the
agent detached (a later `/quit` then asks before stopping the daemon).
Closing the terminal (SIGHUP), losing the connection or a crash of the TUI
is a detach too when it was the agent's last TUI: the terminal is
restored, and the agent keeps running. After the TUI closes it says on
the terminal whether the agent and the daemon still run. `/quit` (also
Ctrl-D, and Ctrl-C twice) means you are done with your agent, and decides
from the daemon's state (`rkat/status`):

| Situation | `/quit` |
|---|---|
| Other TUIs work with this agent | Leaves; the agent and the daemon stay for them, and it says so |
| Other TUIs are attached to other agents | Retires this agent (a running turn is interrupted; its conversation stays for `/resume` and `--resume`); the daemon stays for them |
| This TUI is the last one, started the daemon, and no agent was left detached | Stops the daemon; every agent retires |
| The last TUI, and an agent was left detached, this TUI did not start the daemon, or the daemon cannot say (it restarted, or did not answer) | Asks: detach (keep this agent running, the default) or terminate (stop the daemon and retire every agent; their conversations stay); when a turn is running, terminate says it interrupts it; Esc stays |

Messages not yet delivered are saved either way, and while a turn runs or
messages are queued `/quit` first asks for a second press.
"Started" means this TUI launched the daemon (`--attach` auto with none
running): it hands the daemon a start token and presents it on attach. A
daemon you ran yourself, or one a TUI restarted after it died, has no
starter, so `/quit` asks. When the connection drops (the daemon restarted, say),
the TUI re-reads the endpoint file and reconnects with backoff. A daemon
that died and left a stale endpoint file is started again (auto mode, at
most every 30 s); one that shut down cleanly (no endpoint file) is waited
for. On a reattach to the same daemon the TUI resumes the agent's event
stream right after the last event it showed (the stream's `{epoch, seq}`
cursor), so it shows only what happened since, once; a turn that is still
running shows as running (Esc interrupts it). Attaching for the first time,
or to a daemon that restarted since (its events are numbered anew), the TUI
shows the conversation from its start, as `/resume` does, and follows the
live stream from there. Where the stream stopped is remembered per
agent in `attach-<agent>.cursor` in the workspace's state directory (`attach.cursor` for the `main`
of a daemon from before several agents; with the stream's epoch and a
digest of the daemon's token, never the token). An agent seated for this
TUI (a new one, or one seated again on its conversation) shows its
conversation from the start.

**Stopping the daemon.** `/quit` stops the daemon this TUI started (the
table above) with `rkat/shutdown`; the choice "terminate" does the same.
When the daemon does not confirm (the connection dropped), the TUI reports
it stopped only once its endpoint file is gone, and otherwise says it may
still be running (check with `--list`). `/daemon stop` stops it from any
attached TUI, also with `rkat/shutdown` (keeping its agents): a client
always asks the daemon and never signals a process. A TUI that is not
attached cannot ask, and says so: `rkat-tui --list` shows the daemon and
its pid, and `kill <pid>` (SIGTERM) stops it by hand. Either way the
daemon removes its endpoint file and shuts down; its sessions stay in the
realm. After `/daemon stop` (or SIGTERM) the next
start restores the agents, marked detached; after a terminate they were
retired, and the next start seats a fresh first agent.

**What works:** prompts and streaming output, the agent's file changes as
diffs (in every attached TUI, as they are made; a TUI that attaches later
sees the tool calls, as a resumed conversation does), messages typed mid-turn
(steers, from any attached TUI), Esc (stops the run and the helpers it
started; a second Esc a second later force-cancels the member), the model
picker and `/model` (this TUI's endpoints, configured ones included: the
daemon switches the session in place, with the endpoint's
auth binding and `[thinking]` setting; not while a turn runs; the session
keeps the model), the context gauge,
token counts, `/status`, `/export`, `/cost` (helpers included, kept per
conversation), `/copy`, `/context` (what the daemon reports of the conversation and its tools), `/compact` (compacts at once through a short run of its own; the
✓ compacted line and the gauge confirm it) and `/compact status` (the
policy), `/remember` and `/memory` (the files are edited here; the agent
reads the new content from its next model call), the Ctrl-T task checklist, approvals, the permission
modes, `/permissions`, `/trust` and `!` commands. The token
counter shows the conversation's totals on attach and after `/new` or
`/resume`.

**Helpers.** The agent starts helpers with `assign` (fresh or forked,
single-use or kept); the helpers run in the daemon behind the
same gate and show here: a `[helper · model]` header, its
progress and result, the running count, its usage under `/cost`. Every
attached TUI shows them. A fork (`context: "fork"`, through the engine's
own `fork_off`) starts from the conversation so far, works on the
task it is given and runs behind the same gate (its calls ask you, naming
it), shows as a helper and stops with Esc. Helpers at the
`[helpers] max_depth` limit cannot fork.

**Conversations.** `/new` starts a fresh conversation for this TUI's
agent, `/resume` lists this workspace's conversations (newest first, with
their size, first message and, for another agent's, its name) and
switches back to one of this agent's, showing its transcript; `/resume
<ref>` takes `last` (this agent's most recent conversation other than
this one), `~N` (counting those), an id, an id prefix, or the short id
the picker, `/agents`, `/status` and the notices show (an id's last
eight characters: ids start with the time, so conversations made within a
minute share their first ones). A conversation
belongs to the agent that ran it (the engine seats a session only under
the identity that created it): `/resume` of another agent's says where it
is (`/agents <name>` when that agent works, `rkat-tui --resume <id>` when
it quit). A switch retires the agent's member and seats it again on the
other session; every TUI on that agent follows it. A
`/model` switch carries over to a `/new` conversation. Neither runs while a
turn does, whichever TUI started it: the daemon refuses the switch until
the turn ends or is interrupted. The daemon lists the conversations in
`conversations.json` in the workspace's state directory.

**Approvals.** The daemon's agent and its helpers run behind the same
approval gate: tiers, your rules, the classifier
(same endpoint and model), the permission mode and the escalation timeout.
When a call needs you, every attached TUI shows the same dialog (a helper's
call names the helper); answer it in any of them. The first answer counts;
the other TUIs close their dialog and say it was answered from another
client. A request nobody answers in time is denied.

Each request is a durable decision of the engine: the daemon pushes it to
every attached TUI as it opens (`decision/notice`, and to a TUI that
attaches later while it is open), and a TUI answers it with
`decision/decide`. Who answered is never what the client says: the daemon
records every answer as made by one person, the daemon's principal, which
you set with `principal = "<name>"` under `[daemon]` in your own
`tui.toml` (the user-global one or the realm's; a project file's `[daemon]`
is ignored with a notice); the default is your login name. Interrupting the
turn, or a request nobody answers in time, withdraws it and closes the
dialogs. Requests a daemon left open when it stopped are withdrawn when it
starts again (their calls never ran); a turn the restarted daemon resumes
asks again.

**Modes and rules.** Shift+Tab, `/mode` and `/auto resume` switch the
daemon's mode: one mode for the daemon, shown in every attached TUI's
status bar (refreshed on attach and after every turn). `/permissions list`,
`/permissions add allow|deny <tool> [pattern]` and `/permissions remove <n>` act on the
daemon's rules and are saved in the realm rules file. Deny
rules (and writes into `.git/` or `.rkat/`) are also the agent's recorded
application tool policy: the daemon refuses to rebuild the agent without it.

**`!` commands** run in this TUI's process as you, with no sandbox (see
"Shell mode"), and their output goes to the agent's context.

**Leaving mid-turn.** When the TUI leaves and the daemon keeps running
(`/detach`, or a `/quit` that detaches), the agent's turn goes on in the
daemon, and so do the messages you typed mid-turn that the daemon took. A
`/quit` that stops the daemon interrupts the turn. Either way, messages
this TUI still held (queued, not yet sent) are saved to
`<realm>/tui_unsent.txt`, and the next start says so.

**`-c` and `--resume` at start.** They choose the agent, in whichever
daemon `--attach` chose (see [Which daemon, which agent](#which-daemon-which-agent)).
With neither, you get a new agent. `-c` (and `--resume last`) joins the
agent that was active last: attaching is all it does. `--resume <ref>`
joins the agent that owns that conversation (seated again on it, under
its name, when that agent quit; switched to it with `rkat/resume` when
that agent moved on to another conversation since; a prompt on the
command line goes to it); `--resume` alone joins the agent active last
and opens the picker.

**Limits for now:**

- There is no rewind: the engine refuses a transcript fork of a mob
  member's session, and its member forks seat the copy under a new
  identity; the design of a same-identity rewind is merged upstream
  (lukacf/meerkat#2126), its implementation not scheduled yet.
- `/compact now` acts on a running agent: right after a daemon start the
  agent's session is cold, and the engine does not wake a cold session
  for a compaction (by design), so it refuses it until the agent has run.
  The supported path is to send a message first. (`/model` wakes the
  agent itself, without running a turn, and switches.)
- `/trust` records the project files in the daemon's trust record: their
  hooks run from the agent's next conversation (`/new`, `/resume`) or a
  daemon restart. The agent's MCP servers are the ones its daemon mob was
  created with; a `.rkat/mcp.toml` trusted later does not reach it.
- Until the agent's first run after a daemon start, the gate knows only
  rkat-tui's own file and note tools; other tools (the shell included) are
  reviewed as foreign tools until then. A helper is judged by those and the
  engine's own tools until its first run has ended, and by its own tools
  after that (an `assign` helper that finished in its first run has only
  the one).

## Several agents

One daemon serves a workspace, and it hosts one agent per `rkat-tui` you
start there: several agents can work in the same directory at once, each
with its own user, conversation, model and helpers, and they know about
each other.

**Names.** Each agent is a member of the daemon's mob named `main-<id>`
(four letters and digits, fixed for the agent's life): `/agents`,
`--list` and `/status` name it. A daemon from before several agents keeps its one agent, `main`.

**Starting.** The first `rkat-tui` in a workspace starts the daemon and
works with its first agent. Any later start with neither `-c` nor
`--resume` gets a new agent, whichever daemon `--attach` names, and its
first notice says so, counts the others and how many of them are idle
without a client, and points to `/agents` ("started a new agent
(main-ab12); 2 others work here, 2 idle without a client: /agents picks
one"). `-c` joins the agent that was active last instead (the one that
last ran a turn or took a client), and then the notice names the others
and what they do ("main-abc is running, main-def is idle");
`--resume <ref>` joins the agent that owns that conversation, or seats
that agent again on it when it quit. Your approvals, rules, permission mode, notes and
semantic memory are the workspace's and so shared by every agent; each
TUI shows only its own agent's transcript, helpers, file diffs, approval
requests and classifier notes.

**`/agents`.** `/agents` opens a picker of the agents, like `/model`
and `/resume` do. Each row has the agent's name, running, idle or
unknown, its conversation (short id and first message), how many TUIs
are attached, `detached` for one left running without a TUI, and `*` and
`this rkat-tui` on yours, which is highlighted first. ↑/↓ (or `j`/`k`)
move, Esc or `q` closes it, and:

- Enter switches this TUI to the highlighted agent (chosen while a turn
  of yours runs, the switch waits for its end: [Commands during a
  turn](#commands-during-a-turn)): the agent you leave keeps running, and
  the TUI shows the other's conversation from its start. `/agents <name>`
  does the same without the picker.
- Enter on the last row, `+ new agent`, starts a new agent for this TUI
  and switches to it, as a plain `rkat-tui` would; the agent you leave
  keeps running.
- `d` retires the highlighted agent after a confirmation ("Retire
  main-ab12? Its conversation stays in /resume. y yes · n or Esc no"; see
  [List dialogs](#list-dialogs)). Only an idle agent nobody works
  with can be retired this way: the picker, and the daemon after it,
  refuse one that is running (or whose state cannot be read), one with a
  TUI attached, and your own (`/quit` retires that), saying why. A
  retired agent leaves as on `/quit`: its schedules are deleted, the
  others are told it left, and its conversation stays in `/resume` (and
  `rkat-tui --resume <id>` seats it again). The picker stays open: once
  the daemon confirms, it lists the agents again without the retired one,
  the highlight on its neighbour, so several can be retired in a row; a
  refusal shows its reason on the picker.

**Wired.** Every agent is wired to every other one as a peer: `send_message`
and `reply_to_peer` reach them.

**`contacts`.** One tool tells an agent or a helper whom it can message
and what each is, instead of the engine's `peers` (helpers, agents and
wires mixed, with no kind or state) and `list_members` (which left out
fresh kept helpers); neither of those is offered. It takes no arguments
and answers one compact JSON object, its groups left out when empty:
`you` (its own name, and a helper's kind), `parent` (a helper's),
`your_helpers` (each with its kind, `fresh · kept` and the like, `running`
or `idle for 2h`, the first line of its task, `connected_to` when
`connect_helpers` joined it with another, and its `peer_id`),
`helpers_note` (`2 of your helpers have ended in this conversation`, or
`you hold no helpers now`), `connected_with` (the helpers its parent
connected it with, and whose they are), `agents_of_this_workspace`
(`working`, `idle, a client attached`, `idle, no client attached`, as the
daemon's list words them), `wired_agents` (`/wire`'s agents of other
workspaces, with the workspace) and `others` (a member of a mob the agent
made). Every entry carries the `peer_id` `send_message` takes (the engine's
canonical id; a name does not do). Its entries are exactly the peers the
engine lets the caller send to, sorted by what the daemon knows of each;
a helper the agent holds but cannot reach yet says `not reachable yet`. It
is always allowed, and a helper at the depth limit has it too.
`mob_list_members` still lists a mob the agent made with `mob_create`.

The engine's request and response tools
(`send_request`, `send_response`) are not offered to agents or helpers: on
this engine release a response without a result stops the requesting
conversation for good, so they ask and answer with messages, and answer a
request that still reaches them (from an agent of another workspace whose
daemon runs an earlier build) with `send_message` too. An agent's helpers are wired
to it, not to the workspace's other agents: a fresh `assign` helper
(rkat-tui sets `auto_wire_parent` on every helper it shapes) and a fork
(the engine seats it without a trust edge; the daemon wires it to its
parent once it is seated) each see their parent in `contacts` and can
message it, and it them. A member of a
mob the agent created (`mob_spawn_member`) is not wired to it either (the
engine wires a parent only within one mob), and `wire_members` is refused
in the workspace's mob (only the host wires it).

**Connecting helpers.** Helpers are wired to their parent, not to each
other. An agent (or a helper that started helpers of its own) connects
two or more of its own helpers with `connect_helpers`, naming them as the
call that started each one did (`{"helpers": ["scout", "tester"]}`):
every pair of them is wired both ways, so each sees the others in
`contacts` and can message them. Any kind counts (all four ways of `assign`, and
`mob_spawn_member` members of mobs the agent created), in one mob or
across mobs: an `assign` helper can be connected
with a member of a mob the agent created, or members of two such mobs
with each other. Each newly connected helper
is told once, in a note it reads at its next step, whom it can reach now
and how; the call answers which pairs are new and which were connected
already. A name that is not the caller's own running helper (another
agent's helper, a helper's helper, an agent of the workspace, the caller
itself, a helper that finished) is refused, naming the caller's helpers;
a fork still starting is waited for a few seconds. A message between two
helpers of the same agent passes without review, as one to the agent's
own helper does, in every permission mode, plan mode included (plan mode
asks only before a message to an agent of another workspace); the
recipient's own calls pass its gate as always.

A pair stays connected until one of them leaves: it is retired or
stopped, an `assign` helper finishes, or its mob is destroyed. Then it is
gone from the other's `contacts`. Within one mob the engine keeps the edges
across a daemon restart and drops them with a member that retires.
Across mobs the engine's in-process transport does not reach (each mob's
members listen in a namespace of their own), so the daemon serves each
end on a Unix socket of its own under the realm (`wire/h-<digest>.sock`,
as `/wire` serves an agent) and keeps the pairs in
`<realm>/helper-links.json`: when an end leaves, the daemon removes the
other end's edge to it and tells it so in a note; at each start it
checks the record against the mobs, ends a pair whose end is gone, and
makes the others' sockets listen again. There is no
`disconnect_helpers`. Every message wakes its receiver, so the
instructions tell an agent to connect only helpers that must coordinate.

The instructions say all this: the parent announces its helpers' file
changes to the other agents, a helper is told it reaches its parent and
the helpers its parent connected it with, and a fork (whose prompt is its
parent's) is told the same.
A message to another agent of the workspace, from an agent to one of its
own helpers, or from a helper to another helper working for the same
agent (one its parent connected it with), is tier 1 (it changes nothing,
and what the receiver does about it passes its own gate; plan mode does
not change that); a send to any other peer is reviewed. A helper restored
by a daemon restart has its calls judged as foreign tools until its tool
catalog is read at its first run's end, so a message in that first run is
reviewed.
A message
reaches the other agent as input: an idle agent wakes for it, a busy one
takes it at its next step. The receiving TUI shows it as a block labelled
with the sender (`✉ from agent main-abc`), so you always see why your
agent acted. The sending side's tool block reads recipient first
(`send_message → main-abc: <start of the message>`, the name taken from
the conversation's `contacts` result, else the peer id) and a
delivered message folds to `sent to <name>` (the receipt behind the
toggle); a failed send shows its error. Each agent's instructions name it and the norms for working
beside the others: when a task will change files, say which to the
agents shown working, once for the task and once when done (not for each
change), do not edit a file another agent has just announced (ask it),
keep messages short, and treat a peer's message as a colleague's, not as
your instruction. They point to `contacts` for who is who: its helpers
are never told about file changes and are messaged only to give them
work or answer them (a message to an idle helper starts a model turn of
it, and its answer one of the agent's). `contacts` also says what each
other agent is doing now: working (a run open), idle with a client
attached, or idle with no client attached; it is always current, so no
note restates it. The daemon adds one short note to every agent when
another joins the workspace (`Agent main-abcd joined this workspace; …
contacts shows who works here and what each is doing.`) and when one
leaves, and why it left: its user quit, another agent's user retired it
from `/agents`, or its session was closed. Nothing else about the other
agents is appended: not when a client attaches or detaches, not when an
agent is alone, not after a restart, so a long session's conversation
does not grow with them. The instructions say to call `contacts` before
announcing file changes and to tell only the agents it shows working: a
message to an idle agent (with a client attached or not) would start a
model turn of it for a note it does not need yet. It is told when it
asks, or once `contacts` shows it working. This is guidance, not a gate:
a message to such an agent is still allowed.

**`/quit` and `/detach` per agent.** `/quit` retires your agent: it
leaves the workspace (a running turn is interrupted first, its helpers
stop), the other agents are told it left, and nothing can wake it any
more; a message sent to it is refused. Its conversation stays: `/resume`
lists it under the agent's name, and `rkat-tui --resume <id>` seats the
agent again on it. When other TUIs work with the same agent, `/quit` just
leaves it to them. The daemon itself stops only when the last TUI quits,
that TUI started it and no agent was left detached; otherwise the last
`/quit` asks: detach (your agent keeps running) or terminate (the daemon
stops and every agent retires, their conversations staying). `/detach`
keeps your agent running and wired: it goes on with its turn, and another
agent's message can wake it while you are away; `rkat-tui -c` (or
`/agents` from another TUI) brings you back to it.

**What is and is not kept apart.** The agents share one daemon, one user
and one token; the daemon keeps them apart for clarity, not as a security
boundary:

- Every TUI attached to the daemon receives every agent's approval
  requests as the daemon renders them (the command line, the diff, the
  reason), and shows only its own agent's. Anyone who can attach (anyone
  with the token) can read them all; with one person per daemon, that
  person is you.
- A TUI drives only the agent it works with: the daemon refuses a prompt,
  an interrupt, a compaction or a context note it sends for another agent
  or another agent's helper. That stops a stray client from steering the
  wrong agent; it does not stop a token holder, who can attach to any
  agent by name.
- Another agent can start a turn on yours that nobody prompted: a peer
  message wakes an idle agent, and the turn runs while you are away (a
  `/detach`ed agent too). Such a turn is **unattended** while no TUI is
  attached to your agent: in auto mode the classifier still judges what
  it would judge, and runs what it allows and denies what it blocks, but
  anything that would ask you (what the mode or a protected file would
  ask you about, a paused auto mode, an action too large to review, a
  review that could not be completed) is denied. Reads, allowlisted
  commands and the in-project edits your mode allows without asking run
  as always. Your agent reads in the tool result that the action needs
  you attached, and the next TUI that attaches to it shows a warning for
  each such denied action ("wanted to run `curl …`; it was denied"); ask
  again then to allow it. Attaching in the middle of such a
  turn makes its later calls reviewed as usual. A turn you started stays
  yours after you `/detach`: it is reviewed as usual. The classifier
  never sees a peer's text as your words either.

## Wiring agents across workspaces

Each workspace has its own daemon, realm and agents, and an agent knows
only the agents of its own workspace. `/wire` connects your agent to an
agent of another workspace on the same machine, so the two can message
each other: an API project's agent and its client project's, say.

**`/wire`.** `/wire` opens a picker of the agents of every other
workspace whose daemon runs (found as `rkat-tui --list` finds them),
only their top-level agents, never helpers or forks. Each row reads
`<workspace>/<agent>  working|idle|unknown  <n> client(s)|no client`,
with the workspace's path under it, and `· wired` on one yours is
wired to already. Enter wires the agent this `rkat-tui` works with to
the highlighted one; the picker stays open, the row then marked wired. `/wire <workspace>/<agent>` does the same without
the picker (the workspace by its directory's name or its path, and a
workspace alone when one agent works there). To wire to an agent of a
workspace whose daemon is not running, start `rkat-tui` there first.

Running `/wire` is your consent, for both directions: there is no
further question in either workspace. Your `rkat-tui` holds your
authority on both daemons (it reads their endpoint files as `--list`
does), asks each for its agent's card (its workspace, name, comms key
and socket), and asks the other daemon, then yours, to trust and route
to the other agent. If the second side fails, the first is undone, and
you are told which side failed and why ("the daemon of /home/me/api is
an older rkat-tui-mob without cross-workspace wires: stop it (/quit
there) and start rkat-tui there again with this version"). Neither
daemon trusts the other daemon: each trusts one named agent's key.
Wiring two agents that are wired already changes nothing and says so;
an agent of your own workspace is refused (they are wired to each other
already). `/wire` from a TUI attached with `--attach=HOST:PORT` to a
daemon on another machine is refused: wires join daemons of one machine
through local sockets.

The two halves are two calls, not one step: an `rkat-tui` that dies
after the other daemon wired and before yours did (killed, or its
terminal closed) leaves a one-sided wire. The other workspace's
`/agents` and `/unwire` picker then show it wired to your agent, yours
show nothing, and its messages to your agent are not delivered (your
daemon does not know its key and drops them). `/wire` to it again from
your side completes the wire; `/unwire` there removes the half instead.

**Names.** Both workspaces call their agents `main-<id>`, so across a
wire an agent is named `<workspace>/<agent>` (the workspace's directory
name): `contacts` lists it so, the receiving TUI labels its messages
`✉ from agent api/main-ab12`, the `send_message` header names it, and
the agents' notes use it.

**What the agents are told.** When a wire is made, each agent gets a
note naming the other, its workspace's path, that it is another
project's agent with its own files, rules and user, that a message to
it starts a model turn there when it is idle, and not to announce its
file edits to it (the announcements are for the agents of its own
workspace). The list of the other agents each agent gets also names the
agents of other workspaces it is wired to, whose state is not known
there. `/unwire` tells each side the wire is gone.

**Messages.** A message is the engine's peer message, as between two
agents of one workspace: an idle agent wakes for it and takes a model
turn (its user pays for it), a busy one takes it at its next step. A
`send_message` to a wired agent of another workspace is tier 1, as one
to an agent of your own workspace (it changes nothing, and what the
receiver does about it passes its own gate); a send to any other peer id
is still reviewed. In plan mode a send to an agent of another workspace
is put to you: that workspace has its own permission mode, and plan mode
covers what your agent sets in motion there too. In a turn no client
watches it is denied, and the agent is told to have you leave plan mode
or send the message yourself. Sends to your own workspace's agents and
to your agent's helpers stay tier 1 in plan mode. A message carries no authority: the receiving agent's
calls go through its own workspace's gate and sandbox, and a turn it
starts while no client is attached there is unattended (only what that
workspace's classifier allows runs; nothing that would ask its user).

**When the other side is down.** The wire stays. A send fails at once
(`… IO error: No such file or directory` or `Connection refused`): the
other workspace's daemon is not running, and starting `rkat-tui` there
brings it back. Its agent comes back idle with the same key, and the
first message to it wakes it; nothing has to be wired again, and the
same holds when your daemon restarts. A message sent while the other
side is down is not kept for later.

**A new conversation.** An agent's comms key belongs to its
conversation: `/new` and `/resume` give it another one. Your daemon
makes your side of each wire again at once, and your `rkat-tui` sends
the agent's new card to the other daemons. When one of them is not
running, you are told ("api/main-ab12 does not know this agent's new
conversation (its daemon is not running): /wire api/main-ab12 again
once rkat-tui runs there"), and until then messages between the two are
not delivered.

**`/unwire`.** `/unwire` opens a picker of your agent's wires (each with
`working`, `idle`, `its daemon is not running`, or `retired there (the
wire is stale)`); Enter, then `y`, removes one, and the picker stays
open without it. `/unwire
<workspace>/<agent>` does it directly. Your side is removed always, the
other side when its daemon runs; otherwise it goes when that agent
retires or someone runs `/unwire` there. `/agents` shows each agent's
wires (`· wired to api/main-ab12`), and so does `rkat/status`. An agent
that retires (`/quit`, the `/agents` retire key, the daemon's
terminate) loses its wires; the other side keeps a stale one, which
its `/unwire` picker marks and removes.

**Not covered.** Only top-level agents are wired, not helpers or forks;
only agents of daemons on the same machine; one pair per `/wire`, no
groups. The socket's path must fit a Unix socket address (107 bytes on
Linux, 103 on macOS): a realm under a very long `--config-dir` gets no
socket, and `/wire` names the path and the limit.

## Timers and watches

### Schedules

An agent can set itself a timer: the engine's schedule tools
(`meerkat_schedule_create`, `_get`, `_list`, `_update`, `_pause`,
`_resume`, `_delete`, `_occurrences`) are among its tools, hidden until it
searches for them ([Tools an agent reveals](#tools-an-agent-reveals)): its
persona tells it to call `tool_search` with family `schedule`. A schedule is
one-shot (`"once"`, at a time) or recurring (`"interval"`, every N
seconds; `"calendar"`, cron-like fields in a time zone), and its target
is the agent's own session (`"type": "current_session"`) with a prompt.
The daemon fires it: the agent's turn runs with that prompt, as if
someone had typed it, and your TUI shows the prompt as a block labelled
`⏰ scheduled: <name>`. A schedule follows its agent across `/new`,
`/resume` and daemon restarts.

**Approval.** Reading schedules (`_get`, `_list`, `_occurrences`) is
tier 1. Creating, changing, pausing, resuming and deleting one is
reviewed: the classifier judges it in auto mode, and manual mode asks you
with the schedule in a line first (`schedule nightly, once at
2026-11-11 02:00:00Z, runs current_session: run the checks`), then the
whole arguments. Helpers get no schedule tools.

**What a fired schedule can do.** A fired schedule is a turn no user
started. While a TUI is attached to the agent, its calls are reviewed
exactly like a turn you typed (the classifier in auto mode, you in
manual mode). While **nobody is attached** (you `/detach`ed, or the
daemon restored the agent and nobody joined it), the turn is
**unattended**, the rule of [Several agents](#several-agents): reads,
allowlisted commands and the in-project edits your mode allows without
asking run; in auto mode a command the classifier would judge is judged
and runs when it allows it; anything that would ask you (a protected
file, whatever manual mode would ask, a paused auto mode, a review that
could not be completed) is denied, the agent reads that it needs you
attached, and the next TUI to attach shows a warning per such denied
action. So a nightly `cargo test` runs unattended when it is allowlisted
(`--trust-project-builds`, or an `[[allow]]` rule) or the classifier
allows it; a nightly `git push` to the default branch is blocked, and in
manual mode everything reviewed waits for you.

**`/schedules`.** Opens a list of this agent's schedules: name, when it
fires, the next occurrence, `active` or `paused`, how many times it fired
and what its runs cost (at the cost readout's prices; the daemon
attributes a run to the schedule that fired it). Enter or `p` pauses the
highlighted schedule, or resumes it when paused; `d` asks, and `y`
deletes it. The list shows the change at once; Esc, `q` or Ctrl-C close
it. With no schedules it says the agent sets them itself.
`/schedules pause <name>`, `resume` and `delete` (by name or id) do the
same without the list. These are your own actions: no approval is asked.
`--list` shows how many schedules each agent has; the audit log records a
fired run as `run_triggered` with the schedule's name (its decisions
carry the same run id), and your `/schedules` actions as
`schedule_changed`.

**Detach, quit, stop.** Schedules survive `/detach`: that is what detach
means, and the agent's fired turns then run unattended. `/quit` that
retires the agent (and terminate, which retires every agent) deletes the
agent's schedules first: nothing wakes a retired agent, so nothing is
left to fire for it. The `/quit` dialog says how many schedules the agent
has, and the farewell names those deleted. While the daemon is stopped
nothing fires. When it starts again, an occurrence that came due in
between is skipped as misfired once it is more than 30 seconds late (the
engine's default misfire policy); a schedule created with
`"misfire_policy": {"type": "catch_up_within", "window_seconds": N}`
fires an occurrence that is at most N seconds late instead.

**Limits.** Once revealed, the schedule tools' definitions travel with
every model request of that conversation: about 38 KB (roughly 9k
tokens; `meerkat_schedule_create` and `_update` are 15 KB each), which a
model with a small context window feels; a search for one tool by its
name reveals just that one. The engine's schedules are the workspace realm's: an agent's
`meerkat_schedule_list` shows every agent's schedules, and an agent can
target another agent's session; those calls are reviewed like any other
schedule change. `/schedules` shows only schedules whose target is your
agent.

### Tools an agent reveals

Every tool definition an agent declares rides on every model request. So
two families stay out of a conversation's requests until the agent asks
for them: the eight schedule tools (38.5 KB) and the engine's agent mob
tools (`council`, `mob_create`, `mob_spawn_member`, the `mob_profile_*`
tools and the rest, 17 tools, about 26 KB). Every tool stays enabled; what changes is only which ones a
request declares. A fresh conversation's request carries about 55 KB of
tool definitions instead of 122 KB (roughly 14k tokens instead of 30k).

`tool_search` finds them: free text (every word must appear in a tool's
name or description; exact tool names find just those) and/or a family
(`schedule` or `mob`). It answers each match's name and one-line summary
and reveals exactly those tools for the conversation: from the agent's
next step on they are declared with their full schemas and it calls them
directly. Tools nobody searched for cost nothing. The agent's persona
names both capabilities in a line each, so a model knows to search; the
`mob` line also says what its main tools do (`mob_create` and
`mob_spawn_member` start a team that stays until retired, a `council`'s
summary arrives later in the transcript).
`tool_search` is tier 1 in every mode, plan mode included (it only changes
what the next request declares); a revealed tool's calls pass the gate as
that tool, exactly as if it had always been declared, and the audit log
and the transcript name the tool itself.

A conversation keeps what it revealed across a daemon restart, a `/model`
switch, a compaction and `/resume` (the daemon keeps it in the
workspace's state, `tui_tools.json`); `/new` starts with nothing revealed.
Each agent's conversations are their own: what one agent reveals, another
does not see. A model that calls a hidden tool by name without searching
gets the engine's "tool not found" (the search tool's description says
so); it searches and calls again. Helpers get neither hidden family nor
`tool_search`: a helper at the depth limit has no schedule or mob tools at all,
and a helper allowed to nest keeps the engine's mob tools as they are.
`/context` lists what the requests carry and, separately, what is hidden,
by family. Other families cannot be hidden this way: the comms tools
(messages to the other agents) and the engine's mob operator tools
(`retire_member`, `member_status` and the rest) are mounted by the engine
with no slot a host can wrap, so their definitions ride on every request.

This is rkat-tui's own stand-in for the engine's deferred tool catalog
(`tool_catalog_search` / `tool_catalog_load`), which does not engage for
the daemon's agents on Meerkat 0.9.14 (lukacf/meerkat#2180); when it
does, rkat-tui moves to it (`docs/evidence.md`, "Search and reveal").

### File watches

`watch_paths {"patterns": ["src/**"], "once": false}` asks to be told
when workspace files change; `unwatch` (with a watch's `id`, or none for
all) drops watches. Both are tier 1: a watch changes nothing, and what
the agent does about an event passes the gate as usual. The daemon's own
process watches the workspace (inotify through the `notify` crate), not
the agent's confined shell. Changes are batched until none came for
250 ms and reach the agent as one message, `files changed: a, b and 3
more` (at most 50 paths named), into its running turn at the next step,
or, when it is idle, as a turn of its own, which is then a turn no user
started (unattended while nobody is attached, as above). Your TUI shows
the message labelled `◉ files changed`, and `/status` lists the agent's
live watches.

A watch lives until the turn that made it ends; with `"once": true` it
lives until its first event instead (past the turn: that event starts a
turn), and goes after it. `unwatch` and retiring the agent drop watches
too. Changes the agent's own `write_file` and `edit_file` make are not
reported back to it (the file tools tell the daemon what they wrote; a
temporary file created and renamed within one batch is never reported);
changes its shell makes are, since nothing tells those apart. Patterns
are globs relative to the workspace, matched against the changed file's
path: `src/**`, `*.toml`, `target/report.json`.
