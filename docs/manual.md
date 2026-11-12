# rkat-tui user manual

The full description of rkat-tui: how sessions, approvals, the sandbox,
helpers, rules, endpoints, memory and the keys work. The
[README](../README.md) is the short version; [`testing.md`](testing.md) is
the test protocol for working on rkat-tui itself.

## Contents

- [Overview](#overview)
- [First run](#first-run)
- [Sessions](#sessions)
- [How approval works](#how-approval-works) ([Permission modes](#permission-modes))
- [Confinement](#confinement) ([Security decisions](#security-decisions), [Security model and its limits](#security-model-and-its-limits))
- [Helpers](#helpers)
- [Rules](#rules) ([Configuration reference](#configuration-reference-tuitoml))
- [Models, endpoints and cost](#models-endpoints-and-cost) ([Setting up an endpoint](#setting-up-an-endpoint))
- [Instructions and memory](#instructions-and-memory) ([Context and compaction](#context-and-compaction))
- [Keys and commands](#keys-and-commands) ([Task checklist](#task-checklist))
- [Where things live](#where-things-live)
- [The workspace daemon](#the-workspace-daemon) ([Which daemon, which agent](#which-daemon-which-agent))
- [Several agents](#several-agents)
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
   Linux and macOS). The README's Install section has the build setting
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
another (`last`, `~N` for the N-th most recent, an id or
id prefix; `/resume` or `--resume` alone opens a picker), `/clear`
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
the runtime reports it written. A steer an interrupted turn did not deliver
is sent next (see below); one a failed turn did not deliver comes back to
the editor.

*Esc while a turn runs* interrupts it, as in Claude Code, and Ctrl-C does
the same (the first Esc or Ctrl-C after a mouse selection only clears it):
the daemon stops the agent's run and its running helpers. A message sent to
the daemon meanwhile is the runtime's: the agent gets it at its next step
unless ↑ takes it back first. A second Esc a second later force-cancels
the agent's work, which also drops the messages the runtime still holds
queued; those come back to the editor.

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

**Rewind.** There is no rewind (no `/rewind`, and Esc twice on an empty
editor does nothing): the daemon cannot continue a conversation from
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

## How approval works

Every tool call of every session (and of every helper, see below) passes the
approval gate, installed as the session build's `ToolDispatchAdmission`, so
resumed sessions and helpers carry it too. First match wins:

| Tier | What | Review |
|---|---|---|
| 0 | A user **deny** rule matches, or `write_file` / `edit_file` / the engine's `blob_save_file` (or any target of a patch) is a **sealed** path: the project's `.rkat/` (its `tui.toml`, `mcp.toml`, hooks), every `.rkat` and every `.git` inside it, and, wherever they lie, the git directories governing the project (an enclosing repository's `.git` when the workspace is a subdirectory, a worktree's git dir and the common dir its `commondir` names, with the hooks), the realm directory (the workspace's state directory: `config.toml`, approvals, `tui.toml`, sessions, the audit log) and the user config directory (`~/.rkat/tui`: global rules, trust records). On a file system that resolves names ignoring case (macOS's default), every spelling counts: `.RKAT/` is the real `.rkat/` there and is sealed as well; on a case-sensitive one it is an ordinary directory. Neither the classifier nor an allow rule opens a sealed path, and reading one is not sealed (except the daemon's endpoint directories, below); a shell command that writes there is still judged by the classifier. The engine's own file tools refuse the same state again by themselves (Meerkat 0.9.12): any `.rkat` in the workspace, and the realm, the config directory, the user config document and the credential store when the command line puts them inside the workspace. The directories that hold the daemon's endpoint file and its attach token (`$XDG_STATE_HOME/rkat-tui/`, else `~/.local/state/rkat-tui/`, and `$XDG_RUNTIME_DIR/rkat-tui/`) are sealed for reads and searches too: `read_file`, `grep` and `glob` never open them, and a search above them skips them; so is the realm directory | denied |
| 1 | Read-only tools (`read_file`, `glob`, `grep` inside the project, task tools, `memory_search`, `note_list`/`note_read`, `blob_inspect`, skill browsing, read-only `mob_*` queries, `delegate`, `fork`), read-only shell commands (`cat`, `ls`, `rg`, `git status/diff/log`, `cargo check/test` when builds are trusted, …), narrow user **allow** rules | allowed |
| 2 | `write_file` / `edit_file` / `blob_save_file` where every target is inside the project and not protected (`.rkat/`, `.git/` and any `.git` file, `.env*`, …); workspace notes | allowed (manual mode asks you) |
| ask | `write_file` / `edit_file` / `blob_save_file` (or any target of a patch) is a **protected** path in the project: `.env*`, `.envrc`, `build.rs`, `.cargo/`, `rust-toolchain*`, `AGENTS.md`, `.claude/`, `.gitmodules`, `.gitattributes`, `tui.toml`, `.vscode/`, `.idea/`. Never the classifier, whatever auto mode's state: the dialog names each path and what it is (`protected path: .env.local (secrets); the policy always asks for these`). A user allow rule for the path (what the dialog's "allow and add a rule" adds) allows it, a deny rule denies it | asks you, every time |
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
model reads it and the run continues.

The classifier is a separate LLM call on the start-up endpoint. It is given
the user's messages and the agent's prior tool calls, never the agent's
reasoning or its tool outputs, so the agent cannot argue with it directly.
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
  2. Yes, allow and add a rule (shell rm -rf build)
  3. No, and tell the agent what to do instead (Esc)

Enter select · s stop the turn · Ctrl+C stop all
```

↑/↓ (or `j`/`k`) move the cursor and Enter picks the highlighted row; `1`,
`2` and `3` pick a row directly, and the letters `y`, `a` and `n` (or Esc)
do the same. `s` denies and stops the turn; Ctrl-C stops every open
prompt. A held key only moves the cursor, it never answers. Choosing row 3
(`3`, `n`, Esc, or Enter on it) opens a one-line field, "Tell the agent
what to do instead": Enter denies the call and the model's tool error reads
`denied by the user: <your text>` with an instruction to follow it (the
realm's approval record keeps the text too); an empty Enter, or Esc, denies
without a comment. In the field every letter is text (`s` included) and a
paste joins onto the line; Ctrl-C still stops everything. Row 2 names
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
comes back in it. `--no-auto` (`auto.enabled = false`) starts in manual
mode for one run, and `--set auto.mode=plan` in any mode. A project
`tui.toml` cannot choose a mode; its `enabled = false` only moves auto mode
to manual.

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

Command hooks and local MCP stdio servers are not confined. The mode is
announced at start-up. Every shell call runs in durable process custody, so
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
Unconfined commands (`off`) inherit the whole environment as before.

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
directory already granted with at least that access is refused as
redundant; a detected grant that overlaps an added one otherwise yields to
it (for example `--confinement-write /opt/foo` drops the read-only `/opt`).
Any violation refuses the start.

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
- **Unattended turns.** A turn another agent's message started on your
  agent, while no TUI is attached to it, cannot run anything that needs
  review: the action is denied, the agent is told it needs you, and the
  next TUI that attaches to the agent shows what it wanted to do (see
  [Several agents](#several-agents)). A turn you started stays reviewed
  as usual after you detach.
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
  permission mode. A read, search or write outside the project is a
  reviewed action: in auto mode the classifier decides it, in manual and
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
  agent's shell can read and change what other programs keep there. It
  cannot connect to Unix sockets found there (the sandbox admits only IP
  sockets), but TCP, loopback included, stays open in the default mode.
  *To tighten:* `--confinement=strict` removes the network; nothing
  removes the two shared directories today, so keep secrets out of them.
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
  suggests a `glob` filter (`*.rs`) as well. When `grep`'s output fills
  up, it stops searching; the lines shown are sorted by path but are not
  necessarily the first matches by path, and the truncation line says so.
- A call that goes away (the engine's tool timeout, Esc or Ctrl-C, a
  helper being cancelled) stops its work: the walk notices within an
  entry, a file being read within a megabyte, and a `write_file`,
  `edit_file` or note change that had not been made yet is not made.

## Helpers

The agent can hand self-contained subtasks to helper agents with `delegate`
(Meerkat mobs: each helper is a mob member). A helper runs on the session's
model and endpoint (a model only an endpoint lists included), with the
TUI's file tools and a helper persona, does one turn and returns its final
text. A helper the agent puts on another model still runs through the
session's endpoint, never the model's provider default (its public URL with
an environment key): a model the endpoint's dialect serves, or one the
model catalog does not know, goes there; a model of another provider is
refused, naming the configured endpoints that speak its dialect. Its tool calls pass the **delegating
session's** gate (same rules, counters and Stop; escalations name the
helper), so delegating never unlocks an action. The transcript shows each
helper as one `[label · model]` line, the status bar counts running helpers,
and an interrupt cancels them. A `delegate` call waits for its helper
three times as long as any one tool call may run: `[tools] default_timeout`
(600 s unless the realm's `config.toml` sets it) or a longer
`[tools.tool_timeouts]` entry, so 30 min by default; a helper's own tool
call that runs too long times out first and the helper reports it.
`[tools.tool_timeouts] delegate = "45m"` in the realm's `config.toml` sets
the wait itself. A `delegate` call that times out ends without the helper's
result, and the daemon then retires that helper; members started with
`mob_spawn_member` stay until they are retired or their mob is destroyed.
A `delegate` helper is a member of the mob `implicit-<session id>` of the
agent that started it, not of the workspace's mob (`tui`, where the agents
are); the agent is told so.

**Watching a helper.** While helpers work, a panel under the input lists
them, one line each, indented by nesting depth (the newest top-level helper
first, its nested helpers under it): `[scout · model] ⠋ running · 4 tool
calls · grep "fn resolve"` while it works (its last call, or the first line
of its latest text), then `done · 7 tool calls · 2.1k tokens` (or `failed`,
`cancelled`, `unmeasured`). A helper that finished stays listed until the
next turn starts. The panel takes at most six lines (beyond that, a window
around the selection and `… and N more`) and is hidden when empty.
Ctrl+↓/Ctrl+↑ move a selection in the panel (the newest first, wrapping
around). Enter on an **empty** input opens the selected helper's **full
view** in place of the transcript (Enter with text in the input sends it as
usual, and the input keeps working while the view is open): the helper's
header as the title, then everything it did in order: its prose
(markdown), its reasoning (one line), its tool calls with collapsed output
(Ctrl-O toggles the latest reasoning and tool output) and notes, and the
headers of helpers it spawned. PgUp/PgDn, Shift+↑/↓ and the wheel scroll
it; it follows new output while at the bottom. The view stays open after
the helper finishes, so its result can be read; Esc returns to the
transcript, a second Esc drops the selection.

The transcript keeps each helper's header where it was spawned, as the
record: `▸ [scout · model] done · 7 tool calls`. A click on it unfolds the
helper's blocks in place (indented, tool output still collapsed), and
`/helpers` (or `/tasks`) opens a list of this conversation's helpers, as
their headers show them (label, model, state, calls, tokens; nested ones
indented under their parent): Enter opens the highlighted helper's full
view, as Ctrl+↓ then Enter does, and `f` folds or expands its block in the
transcript; Esc, `q` or Ctrl-C close the list. `/helpers list` prints them
as a notice, and `/helpers expand|fold <label>|all` unfolds or folds them. `[ui] helpers = "expanded"` in `tui.toml` starts every header
unfolded (default `"folded"`); a resumed session shows its helpers folded.

**Bell.** When a turn ends (and no queued message starts the next one) or an
approval prompt opens while the terminal does not have the focus, rkat-tui
writes a terminal bell (BEL), which most terminals turn into a sound, a
badge or a desktop notification. Focus comes from the terminal's focus
reports; a terminal that sends none counts as focused, so it never rings.
`[ui] bell = false` in `tui.toml` (or `--set ui.bell=false`) turns it off.

Helpers nest one level deep by default, as in Claude Code: the session may
delegate, its helpers may not, so a chain of helpers each carrying a full
context (and the mob and council tool schemas, about 25k tokens in every
call) never builds up. `[helpers] max_depth = N` in `tui.toml` (or
`--set helpers.max_depth=N`; a project file may only lower it) allows N
levels: a helper at the limit gets no `delegate`, `mob_create`,
`mob_spawn_member` or `council` tool (left out of its tool list and denied
by its tool access policy), and a deeper spawn is refused.

The agent forks itself with the engine's `fork_off`: the child starts
from the conversation so far, works on the task it is given, runs behind
the same gate as every helper and shows as one (see [The workspace daemon](#the-workspace-daemon)).
A fork is turn-driven and not wired to the agent that forked it, by the
engine's design: it does its task and ends, its result reaches the
parent as the tool's outcome, and it cannot message the parent or be
messaged while it runs.

## Rules

Rules live in TOML, layered (later settings win): the user-global
`<config-dir>/tui.toml`, then `<realm>/tui.toml` (written by `/permissions add`),
then the command line, then `<project>/.rkat/tui.toml`. The project file may
only add deny rules and `[auto]` settings; its `[endpoints]`, `[pricing]`,
`[thinking]` and `[confinement]` are ignored with a notice, and so is its
`auto.classifier_model` (the model that reviews the agent's actions is
yours to pick). Broad allow rules (blanket shell, interpreters, package-manager run
commands) are dropped when auto mode starts.

**`/permissions`** opens a list of the rules in effect: each row's number,
allow or deny, tool, pattern or literal, and the layer it came from. `d`
asks, and `y` removes the highlighted rule, as `/permissions rm <n>`
does: a rule of the realm (or one added during this run) is gone for
good; one written in your global `tui.toml`, the project's
`.rkat/tui.toml` or on the daemon's command line is removed until the
daemon restarts, and the question says so. Adding stays typed:
`/permissions add allow|deny <tool> [pattern]` (the list's last line says
so). `/permissions list` prints the rules as a notice.

**The realm file.** The workspace's realm `tui.toml` lives in its state
directory under the config directory, out of the agent's and a
repository's reach: it is yours, and a hand edit applies.

```toml
[auto]
enabled = true
classifier_model = "claude-sonnet-4-6"   # optional, per-dialect default otherwise
trust_project_builds = true              # cargo build/test/check are tier 1
compact = "native"                       # or "warn" / "auto"; see "Context and compaction"
compact_warn_percent = 75
compact_auto_percent = 85
classifier_tools = true                  # stage 2 may read project files

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
names the argument. Command-line rules show as `(cli)` in `/permissions list` and
are never written to a file.

| Key | Flag | Default |
|---|---|---|
| `auto.mode` (`"default"`/`"accept-edits"`/`"plan"`/`"auto"`; `/mode` writes it to the realm file) | `--set auto.mode=plan` | `"auto"` |
| `auto.enabled` (`false` is `mode = "default"`) | `--no-auto` (= `false`) | `true` |
| `auto.classifier_model` (not from a project file) | `--classifier-model <id>` | per-dialect default of the start-up endpoint |
| `auto.trust_project_builds` | `--trust-project-builds` / `--no-trust-project-builds` | `false` |
| `auto.compact` (`"warn"`/`"auto"`) | `--auto-compact` / `--no-auto-compact` | `"warn"` |
| `auto.compact_warn_percent` | `--set auto.compact_warn_percent=N` | `75` |
| `auto.compact_auto_percent` | `--set auto.compact_auto_percent=N` | `85` |
| `auto.classifier_tools` | `--set auto.classifier_tools=false` | `true` |
| `[[allow]]` / `[[deny]]` `tool`, `arg` (glob) or `literal` | `--allow` / `--deny <tool>[=<glob>]` (repeatable) | none |
| `endpoints.<name>.dialect`, `.base_url`, `.api_key_env`, `.priority` (an inline `.api_key` is refused at start-up), `.api_style` (`"responses"`/`"chat_completions"`, `openai` dialect only) | `--endpoint-def <name>=<dialect>=<url>[,key_env=VAR][,api_style=STYLE]` | `api_style`: `"responses"` |
| `helpers.max_depth` (levels of helpers; a project file may only lower it) | `--set helpers.max_depth=N` | `1` |
| `ui.helpers` (`"folded"`/`"expanded"`: how helper headers start) | `--set ui.helpers=expanded` | `"folded"` |
| `ui.checklist` (`"hidden"`/`"shown"`: the task checklist under the input; Ctrl-T writes it) | `--set ui.checklist=shown` | `"hidden"` |
| `shell.user_timeout_secs` (how long a `!` command may run; a project file may only lower it) | `--set shell.user_timeout_secs=N` | `120` |
| `search.budget_secs` (how long one `glob` or `grep` may run before it answers with what it found; a project file may only lower it) | `--set search.budget_secs=N` | `10` |
| `search.max_entries` (how many files and directories one `glob` or `grep` may examine; a project file may only lower it) | `--set search.max_entries=N` | `500000` |
| `pricing."<model id>".input`, `.output`, `.cached_input`, `.cache_write` (USD per 1M tokens) | `--set 'pricing."<id>".input=15'` | fetched prices |
| `pricing.fetch` (fetch the community price list; a project file may only set `false`) | `--no-pricing-fetch` (= `false`) | `true` |
| `thinking."<endpoint:id or id>"` (`"adaptive"`, `{ enabled = <budget tokens> }` or `"default"`; Anthropic-dialect endpoints only, see "Thinking") | `--set 'thinking."proxy:my-alias"=adaptive'` | the engine's default |

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
keeps the catalog's row. The status bar and `/model` show the endpoint and
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
(`delegate`, `fork`) keep the engine's default.

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

`/compact status` shows the policy and threshold. **`/compact`** (or
`/compact now`; idle only, after at least one turn) *asks Meerkat to compact
at the session's next model call*; the
session is not rebuilt and its threshold does not change. The request is
all the TUI can report; the compaction notices confirm the compaction
itself, and when the check finds nothing to fold the transcript says
"nothing was compacted" with the reason. A prompt typed while the request
is on its way is admitted as an ordinary queued input once it is answered.
The `✓ compacted` notice appears when the session next runs (Meerkat
publishes it once the semantic index took the discarded messages).

`/context` prints the gauge (tokens used of the window, the percentage,
and whether either is an estimate), the policy `/compact status` prints, and
what the context holds as the daemon reports it: the system prompt's size,
the message counts and the tool definitions every model request carries,
in bytes and tokens per family (the engine's built-in, comms, memory, mob
and schedule tools, rkat-tui's file, note and watch tools and its
`tool_search` under `catalog`, each MCP server's), and the tools still
hidden until a search reveals them ([Tools an agent
reveals](#tools-an-agent-reveals)); then the instruction files on disk (`SYSTEM.md`, the workspace
and parent `AGENTS.md`, `AGENTS.local.md`). The daemon reads a
conversation's tool definitions while its agent is idle, so a brand-new
conversation says "not read yet" until its first turn ends. Sizes in
tokens are estimates (four bytes a token).

`/compact <instructions>` compacts the same way. Claude Code uses such text
to focus the summary; Meerkat's compaction takes no instructions yet, so
rkat-tui says so in a warning and compacts without them.

## Keys and commands

**Claude Code parity.** The keys, the commands and the approval prompt
follow Claude Code's wherever both have the feature: Enter queues while the
agent works and Ctrl+Enter sends now, Esc interrupts, Shift+Tab switches
the mode, `?` shows the keys, `/permissions`, `/usage`, `/context` and
`/compact` mean what they mean there, and the approval prompt offers the
same three numbered choices. Where rkat-tui differs or lacks a feature, and
why, is in [`docs/ux-parity.md`](ux-parity.md), which compares the two
feature by feature.


`/help` (alias `/?`), `/quit` (aliases `/exit`, `/q`), `/detach`, `/agents [name]`, `/audit [n]`, `/schedules [pause|resume|delete <name>]`, `/daemon stop`, `/daemon start`, `/clear` (alias `/new`), `/resume [ref]`,
`/permissions [list|add allow|deny <tool> [pattern]|rm <n>]` (`remove` for `rm`; alone, it opens a list of the rules: d then y removes the highlighted one),
`/mode [default|accept-edits|plan|auto]`, `/auto [on|off|resume]`, `/model [[endpoint:]id|refresh]`,
`/remember <text>`, `/memory [workspace|user|system]`, `/init`, `/compact [now|status|<instructions>]`, `/context`, `/status`, `/export [file]`, `/usage` (aliases `/cost`, `/stats`), `/copy [N]`, `/trust`,
`/helpers [list|expand|fold <label>|all]` (alias `/tasks`). `/status` prints the
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
It is rkat-tui's own write, like the history file, not a tool call. `/init` (idle only) sends a
fixed prompt as an ordinary turn, shown in the transcript: inspect the
project and write or update the workspace `AGENTS.md` with the build and
test commands, the layout and the conventions, under 200 lines. The agent
writes it with its file tools, so the usual approval, protected-path and
sealed-path rules apply. `/clear`, `/resume`, `/model` and `/compact` work only while
idle; the transcript is replaced only once the new session is live.

Typing `/` at the start of a one-line input opens a menu of the commands
whose name or alias starts with what follows (case-insensitive) right above
the input, each as `/name usage — summary`, up to eight rows at a time
(the list scrolls with the highlight). ↑/↓ choose a row (not history while
the menu is open), Tab puts the highlighted command and a space in place of
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
output (a click, Ctrl-O), and joins the session as durable system context
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
| Ctrl+Enter | Send now: interrupt the turn and send the input next (sends normally when idle; the same as Ctrl-X Ctrl-S) |
| Shift+Enter, Alt+Enter, Ctrl+J | Insert a newline; so does a `\` typed right before Enter at a line end (the backslash is dropped) |
| Ctrl-C | Interrupt the turn (as Esc), else clear the input, else press twice to quit |
| Esc | Interrupt the turn; queued messages are sent next. The first Esc or Ctrl-C after a mouse selection only clears it |
| Esc Esc (idle, with text) | Clear the input and save it to the prompt history (↑ recalls it) |
| Up (empty input) | Take queued messages back into the editor |
| Ctrl-D | On an empty input, quit on a second press within 3 s (see [Sessions](#sessions) and [Detach and reattach](#the-workspace-daemon)); with text, delete forward |
| Ctrl-O | Show or hide detail: the last reasoning block and the output of the last tool call (a click toggles one tool) |
| Ctrl-T | Show or hide the [task checklist](#task-checklist) |
| Ctrl+Up/Down | Select a helper in the panel under the input (newest first); Enter on an empty input opens its full view (Esc returns, PgUp/PgDn scroll it) |
| Ctrl-R | Search the prompt history, the workspace's then the global one (Ctrl-R again: older; Enter accepts; Esc cancels) |
| Ctrl-X Ctrl-S | Interrupt and send the input (sends normally when idle) |
| Ctrl-G, Ctrl-X Ctrl-E | Edit the input in `$VISUAL`, else `$EDITOR`, else `vi` (also while a turn runs) |
| Up/Down, Ctrl-P/N | Move between lines (wrapped rows first), then recall prompt history |
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
move and pick, and `s` stops the turn (Ctrl-C stops every open prompt).

Details the table leaves out:

- **The editor.** The input wraps at its width and grows up to six rows,
  then scrolls to the cursor. Killed text goes onto a kill ring of 16
  entries (consecutive kills join one entry). Undo takes a word of typing,
  a run of deletes, a kill, a yank, a paste or a clear (Ctrl-C included)
  at a time, up to 64 steps; sending a prompt starts afresh. Stashing over
  a stash moves the older one to the prompt history.
- **The external editor** (Ctrl-G) edits a private temporary file in the
  realm directory's `drafts/`; the edited text replaces the input when the
  editor exits successfully (trailing newline dropped; Ctrl-_ restores the
  old draft), and a failed run keeps the draft with a notice. The session
  goes on meanwhile and its output appears when the editor closes.
- **Ctrl-Z** gives the terminal back and stops rkat-tui with SIGTSTP; `fg`
  repaints the screen. The agent runs in the daemon, so its work goes on
  while the TUI is stopped.

The mouse is captured by default: the wheel scrolls and a click expands or
collapses a tool call's output (Ctrl-O for the latest). Dragging selects text
and copies it on release (double click: a word, triple click: a line;
`/copy` copies the latest answer, `/copy N` the Nth latest). Hold Shift for native
selection, or start with `--no-mouse`.

Copying uses OSC 52 only, so the terminal must support it: kitty, foot,
WezTerm, Alacritty, iTerm2 and xterm (with `allowWindowOps`) do; GNOME
Terminal and other VTE-based terminals do not. The sequence sets both the
clipboard and the primary selection (middle-click paste), where the
terminal supports each. Inside
tmux it is passed through, and tmux needs `set -g set-clipboard on`. The
notice says `copied N chars (terminal must support OSC 52)` once the
sequence is written, or `copy failed: <error>` when it could not be.

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
in the realm `tui.toml`; the daemon reads the list and saves the choice.

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
`timeout`, and for a denial why) and one per change of authority (mode
switch, rule added or removed, `/trust`, an agent
retired, daemon start and stop). It is out of the agent's reach: its
confined shell is never granted the directory (the daemon refuses to
start otherwise) and its file tools refuse to read it.
Past 50 MB it is renamed `audit.1.jsonl` (one older generation is kept).
`/audit [n]` shows its last lines for your agent; `rkat-tui --audit`
prints the whole log, the older generation first.

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

**Status bar.** The first segment is the connection: `connecting`,
`daemon starting`, `attached 127.0.0.1:<port>`, `reconnecting`, a
failure, or `daemon stopped`; the next one the agent this TUI works with
(`agent main-<id>`). A daemon built from another commit than this TUI
adds `· daemon build <version>+<commit>` to the attached segment and says
so once in the transcript (`/daemon stop` and attach again runs the new
build).

**A daemon started with other confinement flags.** The agent's shell
runs in the daemon, under the confinement the daemon was started with
(`/status` shows it); a TUI attaching with other `--confinement` flags
does not change it. To run the agent's shell with other flags, stop the
daemon (`/daemon stop`) and attach again with them.

**When the attach fails.** The segment says why, when the TUI tries
again, and where the daemon's output is, for example `daemon exited (exit
status: 3) · retry 2s · log ~/.rkat/logs/rkat-tui-mob.log`. A failed first
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
conversation), `/copy`, `/context` (what the daemon reports of the conversation and its tools), `/compact` (the agent's next model call compacts first; the
✓ compacted notice and the gauge confirm it) and `/compact status` (the
policy), `/remember` and `/memory` (the files are edited here; the agent
reads the new content from its next model call), the Ctrl-T task checklist, approvals, the permission
modes, `/permissions`, `/trust` and `!` commands. The token
counter shows the conversation's totals on attach and after `/new` or
`/resume`.

**Helpers.** The agent delegates with the engine's `delegate` (and seats
team members with `spawn_member`); the helpers run in the daemon behind the
same gate and show here: a `[helper · model]` header, its
progress and result, the running count, its usage under `/cost`. Every
attached TUI shows them. The agent can also fork itself with the engine's
`fork_off`: the child starts from the conversation so far, works on the
task it is given and runs behind the same gate (its calls ask you, naming
it), shows as a helper and stops with Esc. Helpers at the
`[helpers] max_depth` limit cannot fork.

**Conversations.** `/new` starts a fresh conversation for this TUI's
agent, `/resume` lists this workspace's conversations (newest first, with
their size, first message and, for another agent's, its name) and
switches back to one of this agent's, showing its transcript; `/resume
<ref>` takes `last` (this agent's most recent conversation other than
this one), `~N` (counting those), an id or an id prefix. A conversation
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
  after that (a `delegate` helper has only the one run).

## Several agents

One daemon serves a workspace, and it hosts one agent per `rkat-tui` you
start there: several agents can work in the same directory at once, each
with its own user, conversation, model and helpers, and they know about
each other.

**Names.** Each agent is a member of the daemon's mob named `main-<id>`
(four letters and digits, fixed for the agent's life): the status bar
shows it after the connection, and `/agents`, `--list` and `/status` name
it. A daemon from before several agents keeps its one agent, `main`.

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

- Enter switches this TUI to the highlighted agent (not while a turn of
  yours runs): the agent you leave keeps running, and the TUI shows the
  other's conversation from its start. `/agents <name>` does the same
  without the picker.
- Enter on the last row, `+ new agent`, starts a new agent for this TUI
  and switches to it, as a plain `rkat-tui` would; the agent you leave
  keeps running.
- `d` retires the highlighted agent after a one-line confirmation (`y`
  retires it, any other key keeps it). Only an idle agent nobody works
  with can be retired this way: the picker, and the daemon after it,
  refuse one that is running (or whose state cannot be read), one with a
  TUI attached, and your own (`/quit` retires that), saying why. A
  retired agent leaves as on `/quit`: its schedules are deleted, the
  others are told it left, and its conversation stays in `/resume` (and
  `rkat-tui --resume <id>` seats it again).

**Wired.** Every agent is wired to every other one as a peer, so each has
the engine's peer tools (`peers` lists the other agents, `send_message`
and the request and reply tools reach them; the daemon wires the agents
only, never their helpers).
A message to another agent of the workspace is tier 1 (it changes
nothing, and what the other agent does about it passes its own gate);
a send to any other peer is reviewed. A message
reaches the other agent as input: an idle agent wakes for it, a busy one
takes it at its next step. The receiving TUI shows it as a block labelled
with the sender (`✉ from agent main-abc`), so you always see why your
agent acted. Each agent's instructions name it and the norms for working
beside the others: say which files you are about to change, do not edit
a file another agent has just announced (ask it), say when you are done,
keep messages short, and treat a peer's message as a colleague's, not as
your instruction. The daemon tells each agent who works here when it
starts or switches conversation, and every agent when one joins or
leaves.

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
  attached to your agent: anything in it that would need review (what
  the classifier would judge, or what the mode or a protected file would
  ask you about) is denied, without asking the classifier. Reads,
  allowlisted commands and the in-project edits your mode allows without
  asking still run. Your agent reads in the tool result that the action
  needs you attached, and the next TUI that attaches to it shows a
  warning for each denied action ("wanted to run `curl …`; it was
  denied"); ask again then to allow it. Attaching in the middle of such a
  turn makes its later calls reviewed as usual. A turn you started stays
  yours after you `/detach`: it is reviewed as usual. The classifier
  never sees a peer's text as your words either.

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
asking still run; anything that needs review (a command the classifier
would judge, a write outside the project, a protected file, whatever
manual mode would ask) is denied without asking the classifier, the agent
reads that it needs you attached, and the next TUI to attach shows a
warning per denied action. So a nightly `cargo test` runs unattended only
when it is allowlisted (`--trust-project-builds`, or an `[[allow]]` rule);
a nightly `curl` or `git push` waits for you.

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
tools other than `delegate` (`council`, `fork_off`, `mob_create`,
`mob_spawn_member`, the `mob_profile_*` tools and the rest, 18 tools,
29 KB). Every tool stays enabled; what changes is only which ones a
request declares. A fresh conversation's request carries about 55 KB of
tool definitions instead of 122 KB (roughly 14k tokens instead of 30k).

`tool_search` finds them: free text (every word must appear in a tool's
name or description; exact tool names find just those) and/or a family
(`schedule` or `mob`). It answers each match's name and one-line summary
and reveals exactly those tools for the conversation: from the agent's
next step on they are declared with their full schemas and it calls them
directly. Tools nobody searched for cost nothing. The agent's persona
names both capabilities in a line each, so a model knows to search.
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
`tool_search`: a `delegate` helper has no schedule or mob tools at all,
and a helper allowed to nest keeps the engine's mob tools as they are.
`/context` lists what the requests carry and, separately, what is hidden,
by family. Other families cannot be hidden this way: the comms tools
(messages to the other agents) and the engine's mob operator tools
(`spawn_member`, `list_members` and the rest) are mounted by the engine
with no slot a host can wrap, so their definitions ride on every request.

This is rkat-tui's own stand-in for the engine's deferred tool catalog
(`tool_catalog_search` / `tool_catalog_load`), which does not engage for
the daemon's agents on Meerkat 0.9.13 (lukacf/meerkat#2180); when it
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
