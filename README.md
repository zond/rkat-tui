# rkat-tui

An interactive terminal coding harness built on the Meerkat agent engine. It
hosts its sessions in-process on Meerkat's runtime-backed session path (the
one `rkat`, REST and RPC use) and gates every tool call through an
**auto-mode** approval model: routine actions run without interruption,
risky ones are judged by a reasoning-blind classifier, and you are asked only
after repeated blocks. The shell runs inside an OS sandbox by default.

```bash
cargo run -- "fix the failing test"
rkat-tui -c                       # continue the latest session of this workspace
rkat-tui --resume ~2
rkat-tui --model gpt-5.5 --classifier-model gpt-5.5
rkat-tui --no-auto                # tier-3 actions are denied instead of classified
rkat-tui --model llama3:8b        # bare ids are looked up in every endpoint's model list
rkat-tui --endpoint-def litellm=openai=http://localhost:4000,key_env=LITELLM_KEY --model litellm:gpt-5.5
rkat-tui --allow 'shell=cargo nextest run*' --set auto.compact=auto
rkat-tui --no-confinement         # run the shell unsandboxed (see "Confinement")
```

## Sessions

Every session lives in a `PersistentSessionService` over the realm's stores,
with a `MeerkatMachine` owning input admission, interruption and resume. The
transcript, the model and the cost totals survive restarts: `-c` continues
the workspace's latest session, `--resume <ref>` or `/resume [ref]` any other
(`last`, `~N` for the N-th most recent, an id or id prefix; `/resume` or
`--resume` alone opens a picker; the session service's listing, so an
archived session is not offered), `/clear` (`/new`) starts a fresh one.
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
steers still waiting for a boundary are abandoned first, then the current run
is stopped (steers that joined it are abandoned, not re-run, and a call in
flight is cancelled and answered as *outcome unknown*), outstanding inputs
are cancelled and running helpers are cancelled. Every steered message the
interrupt abandoned then starts the next turn at once, oldest first, shown in
place as your messages; your draft stays in the editor.

*↑ on an empty editor* takes queued messages back without stopping the run:
the messages the UI still holds and every steer the runtime still holds
queued (not yet joined to the run) are abandoned and come back to the editor,
oldest first; the turn runs on (a tool in flight finishes and its result
reaches the model). A steer that already joined the run cannot be pulled
back: the transcript then says your messages already reached the agent, and
↑ recalls prompt history again. Typing alone never interrupts:
a message sent while a tool runs waits for that tool's result and joins the
turn after it.

**How a turn ends.** A turn ends when the runtime reports every input it
admitted terminal, and the UI says how in typed terms: completed, failed
(the first failure wins), interrupted (only when the runtime reports the
inputs cancelled), *pending* (the turn waits for an external tool result
rkat-tui does not provide) or *outcome unknown* (the runtime cannot say what
happened to an input; never resubmit it blindly). A steered message counts
as delivered only when the runtime reports it written at a boundary or its
input consumed.

Ctrl-D on an empty editor quits on a second press within 3 s (the editor
title asks for it); `/quit` quits at once when idle. While a turn runs or
messages are pending both warn first; pressing again within 3 s
interrupts the turn and appends every message the model never received to
`<workspace>/.rkat/tui/tui_unsent.txt` (the next start says so).

## How approval works

Every tool call of every session (and of every helper, see below) passes the
approval gate, installed as the session build's `ToolDispatchAdmission`, so
resumed sessions and helpers carry it too. First match wins:

| Tier | What | Review |
|---|---|---|
| 0 | A user **deny** rule matches, or `write_file` / `edit_file` (or any target of a patch) is a **sealed** path: the project's `.rkat/` (the realm's `config.toml`, rules, MCP servers, sessions) and every `.git` inside it, and, wherever they lie, the git directories governing the project (an enclosing repository's `.git` when the workspace is a subdirectory, a worktree's git dir and the common dir its `commondir` names, with the hooks), the realm directory (`--realm`/`--state-root`: `config.toml`, approvals, `tui.toml`) and the user config directory (`~/.rkat/tui`: global rules, trust records). Neither the classifier nor an allow rule opens a sealed path, and reading one is not sealed; a shell command that writes there is still judged by the classifier | denied |
| 1 | Read-only tools (`read_file`, `glob`, `grep` inside the project, task tools, `memory_search`, `note_list`/`note_read`, `blob_inspect`, skill browsing, `shell_jobs`, read-only `mob_*` queries, `delegate`, `fork`), read-only shell commands (`cat`, `ls`, `rg`, `git status/diff/log`, `cargo check/test` when builds are trusted, …), narrow user **allow** rules | allowed |
| 2 | `write_file` / `edit_file` where every target is inside the project and not protected (`.rkat/`, `.git/` and any `.git` file, `.env*`, …); workspace notes | allowed |
| ask | `write_file` / `edit_file` (or any target of a patch) is a **protected** path in the project: `.env*`, `.envrc`, `build.rs`, `.cargo/`, `rust-toolchain*`, `AGENTS.md`, `.claude/`, `.gitmodules`, `.gitattributes`, `tui.toml`, `.vscode/`, `.idea/`. Never the classifier, whatever auto mode's state: the dialog names each path and what it is (`protected path: .env.local (secrets); the policy always asks for these`). A user allow rule for the path (what the dialog's "allow and add a rule" adds) allows it, a deny rule denies it | asks you, every time |
| 3 | Everything else: other shell commands, MCP tools, web search, comms sends, mutating mob tools, out-of-project paths, user-level notes | classifier |

A tool counts as a builtin, file or note tool only when its definition's
provenance says so: an MCP tool or one of unknown origin with the same name
goes to the classifier. The engine's `apply_patch` and `brain_swap` builtins
are denied to every TUI session by its tool access policy (the TUI owns
file writes, with diffs, and model switching).

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
state and counters. Every escalation is recorded through the realm's own
approval issuer (`approvals.json`); when that record cannot be written the
action is denied.

**What runs as the host and when it is trusted.** The gate and the sandbox
cover the agent's tool calls. Local MCP stdio servers and command hooks are
not tool calls: rkat-tui starts them itself, with your privileges and
outside the shell's confinement. Three workspace files can define them: the
project's `.rkat/mcp.toml` (MCP servers), the project's `.rkat/config.toml`
(`[hooks]`, which Meerkat discovers at the workspace) and, when the realm
lives in the workspace (the default), the realm's `.rkat/tui/config.toml`
(`[[tools.mcp_servers]]` and `[hooks]`). Any of them may have come with a
cloned repository, and on Linux a confined shell can write `.rkat/`, so
each runs nothing until you trust it: `/trust` (alias `/mcp trust`)
records the SHA-256 of every one of them, as it is now, in the host trust
record under `~/.rkat/tui/trust/` (outside the workspace), and
`--trust-project` (alias `--allow-project-mcp`) trusts them for one run
without recording. A file that is absent, or whose digest matches, is
trusted; rkat-tui's own writes into the realm config (endpoints, model
profiles) keep it trusted, but only when it was trusted before the write.
Until then its MCP servers are not started and its hooks do not run (each
project hook is overridden by a disabled entry of its id, for helpers too),
and a start-up notice names each file, why it is not trusted (never
trusted, or changed since), and what it did not start. `/status` shows
each file's state. Your own `~/.rkat/config.toml`, `~/.rkat/mcp.toml` and
`~/.rkat/tui` are always trusted.

## Confinement

`--confinement <mode>` decides how the shell (foreground commands,
background jobs, monitors) runs:

| Mode | Read and write | Read only | Network |
|---|---|---|---|
| `workspace` (default) | the workspace, the temp dir, the Cargo home, the user cache dir, each `--confinement-write` | the rustup home, `/usr/{local,share,include}`, `/opt`, `/etc/{ssl,pki,ca-certificates}`, each `--confinement-read`; the files `/etc/{resolv.conf,hosts,nsswitch.conf,host.conf,gai.conf,services,protocols,passwd,group,gitconfig}` | allowed |
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

- the workspace, temp dir, Cargo home and cache dir: builds, `cargo fetch`,
  scratch files;
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
one of theirs. A project `.rkat/tui.toml` and an untrusted realm file may
not widen the sandbox: their `[confinement]` is ignored with a notice.

A realm `[confinement]` that confines the shell (`shell`, else `all`) is the
**floor**: the command line may only narrow it. `--confinement workspace`
(the default) installs the realm's requirement as it is, `--confinement
strict` installs it with the network denied, and `--confinement off` /
`--no-confinement` refuses to start, naming the realm's policy; so do
`--confinement-read`/`--confinement-write` and the `tui.toml` grants (they
would widen it: add the
directories to the realm's `[confinement.shell]` instead). A realm that
states `trusted_host` (or nothing) for the shell leaves the command line in
charge.

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

## Helpers

The agent can hand self-contained subtasks to helper agents with `delegate`
(Meerkat mobs: each helper is a mob member). A helper runs on the session's
model and endpoint (a model only an endpoint lists included), with the
TUI's file tools and a helper persona, does one turn and returns its final
text. Its tool calls pass the **delegating
session's** gate (same rules, counters and Stop; escalations name the
helper), so delegating never unlocks an action. The transcript shows each
helper as one `[label · model]` line, the status bar counts running helpers,
and an interrupt cancels them.

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
`/helpers` (or `/tasks`) lists the helpers, `/helpers expand|fold <label>|all` unfolds or
folds them. `[ui] helpers = "expanded"` in `tui.toml` starts every header
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

`fork` hands a side quest to a child that starts with the session's own
conversation as context: the TUI renders the committed conversation (user
and assistant text, tool calls and results, `/remember` context; long
values clipped, the oldest entries dropped past 48 KiB) into the child's
prompt, followed by the task and the expected output. Every entry starts
with its role (`[user]`, `[tool result]`, ...) and every further line of its
content is indented with `  | `, so a tool result cannot forge a user line.
The child has the builtins and the shell exactly when the session has them
(its persisted tooling overrides). The child is a helper
in every other respect (the same gate, persona, file tools, nesting under
`[fork-N · model]`, status bar, `/usage` and interrupt), cannot ask the user,
and returns its final answer (at most 16 KiB) as the tool result. Only the
session's own agent has `fork`; helpers and fork children do not, so forks
do not nest.

The engine's own `fork_off` stays denied to every session and helper
through the tool access policy: a fork child seats through the engine's
persisted-fork path, which never consults the TUI's spawn customizer, so it
would run without the approval gate (and without the persona, the follower
and the curated environment), and `fork_off` refuses a caller that is not a
mob member, which the session is not. A durable fork of the session's
transcript (exact context instead of rendered text) is refused while the
session's own turn runs, which is when a tool call asks for it.

## Rules

Rules live in TOML, layered (later settings win): the user-global
`<config-dir>/tui.toml`, then `<realm>/tui.toml` (written by `/permissions add`),
then the command line, then `<project>/.rkat/tui.toml`. The project file may
only add deny rules and `[auto]` settings; its `[endpoints]`, `[pricing]` and
`[confinement]` are ignored with a notice. Broad allow rules (blanket shell, interpreters, package-manager run
commands) are dropped when auto mode starts.

**Trusting the realm file.** The realm `tui.toml` lives in the workspace and
could have arrived with a repository. rkat-tui keeps a **host trust record**
outside the workspace (`<config-dir>/trust/<workspace key>.toml`) with the
SHA-256 of each workspace file it wrote itself or you trusted. The realm
`tui.toml` counts as yours only when it is a regular file reached through no
symbolic link and its digest matches the record; otherwise it can only deny,
its `[endpoints]`, `[pricing]`, `[confinement]` and `[auto]` are ignored,
and a notice says why. `/permissions add` and the escalation dialog update the record; `/permissions
trust` shows a summary of the file as it is now and records its digest.

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
| `auto.enabled` | `--no-auto` (= `false`) | `true` |
| `auto.classifier_model` | `--classifier-model <id>` | per-dialect default of the start-up endpoint |
| `auto.trust_project_builds` | `--trust-project-builds` / `--no-trust-project-builds` | `false` |
| `auto.compact` (`"warn"`/`"auto"`) | `--auto-compact` / `--no-auto-compact` | `"warn"` |
| `auto.compact_warn_percent` | `--set auto.compact_warn_percent=N` | `75` |
| `auto.compact_auto_percent` | `--set auto.compact_auto_percent=N` | `85` |
| `auto.classifier_tools` | `--set auto.classifier_tools=false` | `true` |
| `[[allow]]` / `[[deny]]` `tool`, `arg` (glob) or `literal` | `--allow` / `--deny <tool>[=<glob>]` (repeatable) | none |
| `endpoints.<name>.dialect`, `.base_url`, `.api_key_env`, `.priority` (an inline `.api_key` is refused at start-up) | `--endpoint-def <name>=<dialect>=<url>[,key_env=VAR]` | |
| `helpers.max_depth` (levels of helpers; a project file may only lower it) | `--set helpers.max_depth=N` | `1` |
| `ui.helpers` (`"folded"`/`"expanded"`: how helper headers start) | `--set ui.helpers=expanded` | `"folded"` |
| `shell.user_timeout_secs` (how long a `!` command may run; a project file may only lower it) | `--set shell.user_timeout_secs=N` | `120` |
| `pricing."<model id>".input`, `.output`, `.cached_input`, `.cache_write` (USD per 1M tokens) | `--set 'pricing."<id>".input=15'` | fetched prices |

Other flags: `--model`/`-m <[endpoint:]id>`, `--endpoint <name>`
(`--provider` for the built-ins), `--resume [ref]`, `-c`/`--continue`,
`-w`/`--workspace`, `--config-dir`, `--user-config-root`, `--realm`,
`--isolated`, `--realm-backend`, `--state-root`, `--confinement`,
`--confinement-read`, `--confinement-write`, `--no-confinement`, `--no-mouse`, `--wait-for-mcp`, `--trust-project` (alias `--allow-project-mcp`),
`--no-pricing-fetch`.

## Models, endpoints and cost

An **endpoint** is a named place to send requests in one of Meerkat's wire
dialects: `anthropic`, `openai` (also LiteLLM and other OpenAI-compatible
proxies) or `gemini`. A dialect names the protocol, not who serves the
model. The built-in endpoints exist when their key resolves (realm config,
provider environment variable, or the `rkat` token store). More come from
`[endpoints.<name>]` in `tui.toml`.

Configured endpoints are **written into the realm's own `config.toml`**
(through the realm's `ConfigRuntime`: the writer lock and generation check
`rkat config`, REST and RPC write under, retried when another writer got in
between) as connections of the `rkat-tui-endpoints`
realm section: a backend with the endpoint's URL, an auth profile reading
the environment variable `api_key_env` names (the document records the
variable's name, never a key) and a binding. Keys come from the environment
only: an inline `api_key` in `[endpoints.<name>]` (or through `--set`)
refuses the start (exit 2) with a message naming `api_key_env`. Models
only an endpoint knows get a `[models.<id>]` profile there, kept current
from the endpoint's listing. A session's binding therefore resolves for
`rkat`, REST and RPC on the same realm too. One `[models.<id>]` profile
names one provider, so the same unknown model id on endpoints of two
dialects is refused.

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

The status bar shows the context gauge, `ctx 42% (84k/200k)` (yellow from 70%,
red from 90%, `~` for an estimate), and the session's cost, `$0.42`: each
model call's tokens are priced per model from `[pricing]` overrides, a
LiteLLM endpoint's `/model/info`, or LiteLLM's community price list (cached
for 7 days in the user cache directory, `$XDG_CACHE_HOME/rkat-tui` or the
platform's; `--no-pricing-fetch` skips the download). Helpers' usage and
compactions (their estimated input plus the summary) count too. `/usage`
prints the breakdown. The readout is an estimate, not a bill, and says how
far it can be trusted: exact; `~` **approximate** when part of it is
estimated (a compaction's input, or usage the session's own record has that
the TUI did not see: the totals are reconciled with the session's
authoritative usage on resume); `~` and a "lower bound" note in `/usage`
when some helper usage could not be observed at all.

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
`/new`). A resumed session (`-c`, `/resume`, and the rebuilds `/compact`
and model switches make) keeps the instructions it was created with: Meerkat
does not reassemble a resumed session's system prompt, so the TUI passes the
session's persisted instructions back unchanged (the persona appears once).
Edited instruction files apply from the next new session; `/remember`
and `/memory` reach the live one.

**`/memory`** opens the workspace `AGENTS.md` (the file `/remember` writes),
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
in `<workspace>/.rkat/tui/notes/`, user notes (personal preferences that
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
window of the model's profile (the catalog, or the realm's `[models.<id>]`
entry the endpoint listing keeps current):

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

`/compact status` shows the policy and threshold. Meerkat has no on-demand
compaction call, so **`/compact`** (or `/compact now`; idle only, after at
least one turn) *rebuilds the session so its next model call compacts*: the session is
rebuilt with a one-turn threshold of one token, and when the next turn ends
it is rebuilt on the policy's threshold again. The rebuild is all the TUI
can report; the compaction notices confirm the compaction itself. A long
tool loop in that turn may compact again after Meerkat's cadence (3 model
calls by default). A prompt typed during the rebuild is admitted as an
ordinary queued input once it is done. The `✓ compacted` notice appears
when the session next runs (Meerkat publishes it once the semantic index
took the discarded messages).

`/context` prints the gauge (tokens used of the window, the percentage,
and whether either is an estimate), the policy `/compact status` prints, and
what the TUI knows of the context: the system prompt's size, the message
counts, the instruction files on disk (`SYSTEM.md`, the workspace and parent
`AGENTS.md`, `AGENTS.local.md`) and the number of tools rkat-tui registers
itself. Sizes in tokens are estimates (four bytes a token); the engine's
built-in and MCP tools are not visible to the TUI and are labelled unknown.

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
why, is recorded in [`docs/ux-parity.md`](docs/ux-parity.md) (the audit
behind these controls; its table describes the state before the change).


`/help` (alias `/?`), `/quit` (aliases `/exit`, `/q`), `/clear` (alias `/new`), `/resume [ref]`,
`/permissions [list|add allow|deny <tool> [pattern]|rm <n>|trust]` (alias `/rules`; `remove` for `rm`),
`/auto [on|off|resume]`, `/model [[endpoint:]id|refresh]`,
`/remember <text>`, `/memory [user|system]`, `/init`, `/compact [now|status|<instructions>]`, `/context`, `/status`, `/export [file]`, `/usage` (aliases `/cost`, `/stats`), `/copy [N]`, `/trust` (alias `/mcp trust`),
`/helpers [expand|fold <label>|all]` (alias `/tasks`). `/status` prints the
rkat-tui version, the model and its endpoint, the session id, the workspace
and realm directories, the shell confinement, the auto-mode state, whether
the realm `tui.toml` is trusted, the trust state of each project input
`/trust` covers (`.rkat/mcp.toml`, `.rkat/config.toml`, the workspace
realm's `config.toml`; "trusted for this run" under `--trust-project`), and
the helper depth limit. `/export [file]` writes the transcript as plain text
(a `## user`, `## assistant`, `## tool …`, `## notice` header per block;
folded reasoning and collapsed tool output keep only their headers, as on
screen) to `file` (relative to the workspace; owner-readable only), by
default `.rkat/tui/export-<session id>-<unix time>.txt` in the workspace.
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
warning instead; a word that names nothing (`@someone`) stays plain text.

**Shell mode.** An input whose first line starts with `!` is a shell
command: `!ls -la` (or `! ls -la`; the rest of the input, later lines
included, is the command). The editor title says so while you type it.
Enter runs it with the agent's own shell setup: the same program (the
shell the `shell` tool resolves), the workspace as working directory, the
same confinement (`--confinement`, the realm floor, the extra grants) and
the same curated environment, so provider keys stay outside; under
`--no-confinement` it runs plain, as the agent's commands then do. There is
no approval prompt, since you typed it. It may run for `[shell]
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

`?` on an empty input opens a panel with the keys part of `/help` (the
editor title shows `? keys` while idle and empty); `?`, Esc or any other
key closes it without being typed, and with text in the input `?` is just
typed. Enter sends (steers while a turn runs); Ctrl+Enter sends now (while a
turn runs it interrupts the turn and sends the input next, like Ctrl-X
Ctrl-S); Shift+Enter, Alt+Enter or Ctrl-J insert a newline, and so does a
`\` typed right before Enter at the end of a line (the backslash is
dropped); Esc or Ctrl-C interrupts a running turn and the queued
messages are sent next (Ctrl-C twice quits when idle); when idle, Esc twice on a draft clears it
and saves it to the prompt history (↑ brings it back); ↑ on an empty editor takes queued
messages back, else recalls prompt history (Ctrl-P/Ctrl-N are ↑/↓); Ctrl-R searches history (workspace,
then global); Ctrl-X Ctrl-S interrupts and sends the input; Ctrl-O shows or
hides detail (the last reasoning block and the last tool output together);
Ctrl+↑/↓ select a helper in the panel under the
input and Enter on an empty input opens its full view (Esc returns);
PgUp/PgDn and Shift+↑/↓ scroll; Home/End jump when the editor is empty;
Ctrl-L clears the screen and redraws it; Shift+Tab switches auto mode off
and on (the same as `/auto off` and `/auto on`); Alt+P opens the model
picker (as `/model`) and leaves the draft in the editor; Ctrl-Z suspends
rkat-tui to the shell (on Unix: it gives the terminal back and stops with
SIGTSTP; `fg` resumes it and repaints the screen, while the session's work
was paused with the process); Ctrl-G or Ctrl-X Ctrl-E opens the input
in your editor (`$VISUAL`, else `$EDITOR`, else `vi`, as `/memory` does)
through a private temporary file in the realm directory's `drafts/`, and the
edited text replaces the input when the editor exits successfully (trailing
newline dropped, Ctrl-_ restores the old draft); a failed run keeps the
draft with a notice. It works while a turn runs; the session goes on
meanwhile and its output appears when the editor closes.
Long lines wrap at the width of the input field (which grows up to six
rows, then scrolls to the cursor), and ↑/↓ move by wrapped rows before
reaching history.

The editor has the readline keys: Ctrl-A/E and Alt+B/F (or Ctrl/Alt+←/→)
move; Ctrl-K kills to the line end (at a line end, the newline), Ctrl-U to
the line start, Ctrl-W (or Ctrl/Alt+Backspace) the word before, Alt+D the
word after. Killed text goes onto a kill ring of 16 entries (consecutive
kills join one entry); Ctrl-Y yanks the newest and Alt+Y right after a yank
replaces it with the next older one. Ctrl-_ (Ctrl-7 or Ctrl-Shift-- in
some terminals) undoes input edits: a word of typing, a run of deletes, a
kill, a yank, a paste or a clear (Ctrl-C included) at a time, up to 64
steps; sending a prompt starts afresh. Ctrl-S stashes the input (puts
it aside and clears the editor) and, on an empty editor, brings the stash
back, so a quick question can go first; stashing over a stash moves the
older one to the prompt history. Ctrl-S right after Ctrl-X still completes
the send-now chord.

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

## Where things live

All state for a workspace lives inside it, under `<workspace>/.rkat/tui/`:
the realm (sessions, config with the endpoint connections and model
profiles, `tui.toml` written by the UI, the approval issuer's
`approvals.json`, process custody, semantic index), the per-session cost
records (`tui_cost/`, owner-only), input history, unsent messages, notes,
the external editor's draft files while Ctrl-G edits them (`drafts/`) and
logs
(`logs/rkat-tui.log`). Nothing is keyed by the absolute path, so a renamed
or moved workspace still continues with `-c`. A `.gitignore` keeps the
directory out of commits.

The workspace is `--workspace <dir>` (`-w`), else the nearest ancestor of the
current directory that already has `.rkat/tui/` (the search stops at the git
root and never picks your home directory), else the current directory.
Running from your home directory requires an explicit `--workspace`.
`--realm`, `--isolated` and `--state-root` opt into the shared realm model
that `rkat` uses instead.

User-global files live in one config directory: `--config-dir <dir>`, else
`~/.rkat/tui` (under Meerkat's own user config root `~/.rkat`;
`<--user-config-root>/.rkat/tui` when that flag is given). It holds
`tui.toml`, `AGENTS.md`, `SYSTEM.md`, `notes/`, `history` and the host trust
records (`trust/`). `$XDG_CONFIG_HOME` no longer decides it. Earlier
releases used `~/.config/rkat-tui` (`$XDG_CONFIG_HOME/rkat-tui`;
`<--user-config-root>/.config/rkat-tui`; `%APPDATA%\rkat-tui` on Windows):
while `~/.rkat/tui` does not exist and such a directory does, it is used as
is, with a one-line start-up notice naming the new default and
`--config-dir`. Nothing is moved; move the whole directory (the trust
records included, whose names do not depend on where the directory is) to
switch. With `--workspace ~` the workspace's own state directory is
`~/.rkat/tui` too: rkat-tui refuses to start (exit 2) when the config
directory and the realm directory are the same or one holds the other,
naming both, so pass `--config-dir <other dir>` there. The price-list cache lives
in the user cache directory (`<config-dir>/cache` when `--config-dir` is
given).

## Testing

The default lane needs no keys, no network and no sandbox (unit tests plus
headless runner tests over the real runtime-backed service with a scripted
LLM):

```bash
cargo test
```

The integration tests drive internal modules through the crate's
`test-support` feature, which the workspace turns on through the crate's
dev-dependency on itself. From the unpacked published package, run them with:

```bash
cargo test --features test-support
```

The confinement tests probe the host's sandbox backend directly (a confined
`/bin/true` launch) and skip themselves with a `SKIPPED:` line naming the
reason where it is unavailable (for example Ubuntu's user-namespace
restriction). The name-resolution test resolves `localhost` inside the
sandbox and, where the host has an `/etc/resolv.conf` (a regular file or a
link) and itself resolves `example.com`, that name too; otherwise its DNS
half prints `SKIPPED:`.

The live lanes are ignored by default and skip a provider whose key is not in
the environment:

```bash
ANTHROPIC_API_KEY=… OPENAI_API_KEY=… GEMINI_API_KEY=… cargo test --test e2e_classifier -- --ignored
ANTHROPIC_API_KEY=… OPENAI_API_KEY=… GEMINI_API_KEY=… cargo test --lib e2e_list_models -- --ignored
```

