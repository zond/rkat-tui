# Stored state

Every file and directory rkat-tui reads or writes outside the workspace's
sources, as of the first release. From that release on these formats are
a contract: a workspace's state written by a tagged build must keep
opening with every later build, and a change that would break that needs
the owner's explicit yes (`CLAUDE.md`, "Stored state"). The engine's own
files inside a realm are Meerkat's contract, listed at the end.
`src/state_fixtures.rs` opens the copies of these files in
`tests/fixtures/state-0.0.1/`, as the first release wrote them, with the
readers of the current build: that test is the guard.

"Atomic" means `paths::write_atomic`: a temporary file in the same
directory, synced, renamed over the target; `Private` is mode 0600, and a
target reached through a symlink below the caller's trusted root is
refused.

## Where the directories are

| Name | Resolved as |
|---|---|
| `<config>` | `--config-dir`; else `<--user-config-root>/.rkat/tui`; else `$HOME/.rkat/tui` (`$XDG_CONFIG_HOME` is not used; no home: `HomeError::NoConfigDir`). `src/bootstrap/dirs.rs` |
| `<cache>` | `<--config-dir>/cache`; else `<--user-config-root>/.cache/rkat-tui`; else `$XDG_CACHE_HOME/rkat-tui`; else the platform cache dir + `rkat-tui` (`~/.cache` on Linux, `~/Library/Caches` on macOS); else `<config>/cache`. `src/bootstrap/dirs.rs` |
| `<state>` | `$XDG_STATE_HOME` when absolute, else `~/.local/state`, then `rkat-tui/<key>/`, `<key>` the first 32 hex digits of SHA-256 of the canonical workspace path; `$XDG_RUNTIME_DIR/rkat-tui/<key>/` when the agent's confined shell would be granted the state dir; `--endpoint-file` names the file itself. Not moved by `--config-dir`. `src/daemon/endpoint.rs` |
| `<realm>` | `<config>/workspaces/<id>/`, `<id>` 16 random hex digits drawn once per workspace, directories 0700. Which workspace owns a realm is its `workspace.toml` alone (`src/bootstrap/realm_home.rs`). |

## `<config>` (default `~/.rkat/tui`)

| Path | Format | Written by | Losing it loses |
|---|---|---|---|
| `tui.toml` | TOML, `config::file::RulesFile` (global layer) | the user only | user settings |
| `AGENTS.md` | Markdown (user-scope instructions) | the user (`/memory user` opens an editor) | user data |
| `SYSTEM.md` | Markdown, replaces the base system prompt | the user; the client creates a stub (plain write) | user data |
| `notes/<slug>.md` | Markdown, one user-scope note each (`memory::NoteStore`) | daemon (`note_save`, `note_forget`), the user | user data |
| `notes/MEMORY.md` | Markdown index of the notes | daemon, atomic | nothing (rebuilt on every note write) |
| `history` | text, one escaped prompt per line, at most 1000 (`ui::history`) | client: appends (0600), rewrites atomically | prompt history |
| `trust/<key>.toml` | TOML, `bootstrap::trust::RecordFile { workspace, files: path → sha256 }`; `<key>` = first 32 hex digits of SHA-256 of `realm\0<id>` | daemon (`/trust`), atomic Private | project trust (`/trust` again) |
| `workspaces/` | one realm per workspace (below) | both | everything below |
| `workspaces/.lock` | empty file, exclusive `flock` while realms are created or re-homed | both | nothing |

## `<cache>`

| Path | Format | Written by | Losing it loses |
|---|---|---|---|
| `pricing-cache.json` | JSON `pricing::CacheFile { fetched_at, models }`, kept 7 days | client, atomic (mode default) | nothing (fetched again) |

Model lists are kept in memory only.

## `<state>` (`~/.local/state/rkat-tui/<key>/`)

| Path | Format | Written by | Losing it loses |
|---|---|---|---|
| `mob.endpoint` | one-line JSON `daemon::endpoint::Endpoint { address, token, pid, workspace, started_at }` | daemon (temporary file, fsync, rename; 0600 in a 0700 directory), removed at its stop; clients read it | nothing (a running daemon is no longer found by its clients; they start another, which the realm lease refuses until the first stops) |

`--attach-token-file` names a file of the user's that the client only
reads (refused when group or others may read it).

## `<realm>` = `<config>/workspaces/<id>/`: rkat-tui's files

| Path | Format | Written by | Losing it loses |
|---|---|---|---|
| `workspace.toml` | TOML `realm_home::Manifest { path, name, git_remote?, created, last_used }` | both (`last_used`; `/rehome`), atomic Private | the link from the workspace to its realm (the realm is orphaned) |
| `tui.toml` | TOML `RulesFile`, the realm layer; rkat-tui writes rules, `[auto] mode`, `[ui] checklist` (`config::persist::RealmFile`) | daemon, atomic Private; the user may edit | rules and settings |
| `conversations.json` | JSON `daemon::conversations::Log { sessions: [Entry { session_id, agent, started_at, last_used }] }`; read strictly (a bad file stops the daemon's start) | daemon, atomic Private | the `/resume` list (the sessions themselves stay in the engine's database) |
| `audit.jsonl`, `audit.1.jsonl` | JSON lines `daemon::audit::Line`; rotated at 50 MiB, one generation kept | daemon appends (0600, no symlink); clients read (`/audit`, `--audit`) | the audit record |
| `tui_usage/<session>.toml`, `tui_usage/schedule-<id>.toml` | TOML `daemon::usage::Record` (totals per model, the last call's context); an unreadable file is set aside as `*.corrupt-<secs>` | daemon (written behind, at run ends and stop), atomic Private | cost and usage history |
| `tui_tools.json` | JSON `daemon::deferral::reveal::Stored { revealed: session → [tool] }` | daemon, atomic Private, before a search answers | each conversation's revealed tools (it searches again). Goes when the engine's deferral replaces rkat-tui's (lukacf/meerkat#2180) |
| `tui_helpers.json` | JSON `mobs::bindings::FileFormat { version: 1, mobs, members }` | daemon, atomic Private | which session each helper works for after a restart (helpers then refused) |
| `notes/<slug>.md`, `notes/MEMORY.md` | workspace-scope notes, as in `<config>` | daemon, atomic Private | user data |
| `attach.cursor`, `attach-<agent>.cursor` | one-line JSON `client::catchup::Cursor { session_id, sequence, epoch?, daemon? }` | client, atomic Private | nothing (the client replays instead) |
| `tui_history` | as `<config>/history`, this workspace's prompts | client | prompt history |
| `tui_unsent.txt` | text, blocks starting `--- unsent message ---` | client appends | unsent messages |
| `drafts/draft-*.md` | the external editor's temporary file | client, removed after the edit | nothing |
| `export-<session>-<secs>.txt` | plain-text transcript (`/export` without a path) | client, atomic Private | the export |
| `logs/rkat-tui.log`, `logs/rkat-tui-mob.log`, `logs/rkat-tui-mob.stderr.log` | text logs | client; daemon; the daemon's stdout/stderr (opened by the client that starts it) | nothing |

## `<realm>`: Meerkat's files (rkat-tui points the engine at the realm)

Paths from `meerkat_store::realm_paths_in`; opened by the daemon only
(`open_realm_persistence_in`, `start_realm_lease_in`). rkat-tui writes
into `config.toml` through the engine's config runtime (writer lock,
generation check).

| Path | What |
|---|---|
| `realm_manifest.json` | the engine's realm manifest (backend, origin) |
| `config.toml`, `config_state.json`, `config_state.lock` | the realm's config document and its generation sidecar and writer lock. rkat-tui writes the configured endpoints' connections (`[realm.rkat-tui-endpoints]`), the self-hosted servers and their model aliases (`[self_hosted]`). Holds endpoint settings, never a key |
| `sessions.sqlite3` | every conversation (the transcripts) |
| `runtime.sqlite3`, `jobs.sqlite3` | runtime state (inputs, decisions), detached jobs; the schedules are a domain of the engine's SQLite stores |
| `mobs/mob--<id>.db`, `mobs/realm_profiles.db`, `mobs/realm_forked_participants.db` | the mobs: the agents (members), helpers, councils |
| `tasks.db` | the checklist (`SqliteTaskStore`) |
| `memory/` | the semantic-memory index |
| `leases/`, `hosting/`, `delivery/` | the realm lease and per-session locks |

## In the workspace

rkat-tui reads, never writes on its own: `.rkat/tui.toml` (the project
layer: deny rules and `[auto]` only), `.rkat/mcp.toml` and
`.rkat/config.toml` (run only when their digest is in the trust record),
`AGENTS.md` / `.rkat/AGENTS.md` up the tree and `AGENTS.local.md`, and
`.git/config` (the remote recorded in the manifest). `/remember` appends a
line to the workspace's `AGENTS.md` (user data). A `.rkat/tui/` directory
in a workspace is never read.
