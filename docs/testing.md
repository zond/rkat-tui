# Test protocol

How to verify a change to `rkat-tui` before it is committed. CI
(`.github/workflows/ci.yml`) runs exactly the gate below on Rust 1.97.0, so a
green gate locally is a green push.

## The gate

Run from the repository root, in this order, and check every exit code:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --lib --bins -- -D warnings
cargo nextest run --all-features
RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links" cargo doc --no-deps --all-features --document-private-items
```

A failure anywhere means the change is not done. Never commit on red, and
never "fix" a red step by loosening a lint or deleting a test. `cargo fmt
--all` (without `--check`) fixes formatting; the others are fixed by
changing code.

The last step catches broken intra-doc links. The rustdoc is written for
maintainers and links private items freely, so it documents them
(`--document-private-items`); rustdoc allows `private_intra_doc_links` in
that mode by default, and `-D warnings` would deny it again, hence the
`-A`. A module whose own docs link its submodules must not also carry an
outer `///` doc at its declaration: rustdoc then resolves the module's
links from the parent (see `daemon` and `client` in `src/lib.rs`).

Warm, the whole gate takes a few minutes. The first build after a toolchain
or dependency change takes well over ten. Plain `cargo` is correct here: this
repository is not the Meerkat workspace, so none of its wrappers or lane
variables apply. `.cargo/config.toml` already sets the test-thread stack and
the SQLite flags the engine needs; do not override `RUST_MIN_STACK` or
`LIBSQLITE3_FLAGS`.

## Dependency checks (CI only)

CI's `deny` job runs `cargo deny check` over `deny.toml`: RustSec
advisories (vulnerabilities, unmaintained and yanked crates), the licence
allow-list and the sources (crates.io only). It is not a step of the gate
above, which stays five steps; run it locally with `cargo deny check`
(cargo-deny 0.18 or later, which fetches the advisory database) after a
dependency change. An advisory with no fixed version this tree can take
goes into `deny.toml`'s `ignore` list with the date and what lifts it,
never silently; one with a fix in semver range is fixed with `cargo update
-p <crate>`.

## While iterating

Scope the run to what changed, then run the full gate once at the end:

```bash
cargo nextest run --all-features --lib                 # unit tests only
cargo nextest run --all-features --test client_turns     # one integration binary
cargo nextest run --all-features -E 'test(resume)'     # tests whose name contains "resume"
cargo nextest run --all-features --no-fail-fast        # see every failure at once
```

Clippy must be run with `--all-targets --all-features`; a narrower
invocation misses the test targets and the `test-support` code.

The second clippy lane builds the library and the binaries without
`test-support` (the crate's only feature). Under `test-support` every
internal module is public (the paired `pub mod` / `pub(crate) mod`
declarations in `src/lib.rs`), so an item nothing calls is never reported
there; in this lane the modules are crate-private and an unused item or
re-export is dead code, which `-D warnings` refuses. An item only tests use
(a test seam such as `Daemon::start`, a getter a test asserts on, a
wrapper with a fixed clock) is declared under
`#[cfg(any(test, feature = "test-support"))]`; an item nothing uses is
removed.

## Where tests live

| Kind | Where | Notes |
|---|---|---|
| Unit | `#[cfg(test)]` modules next to the code in `src/` | Pure logic: lexer, rules, keymap, input editing, completion, approval routing |
| Integration | `tests/*.rs` | Drive the internal modules through the `test-support` feature (the crate's dev-dependency on itself turns it on) |
| Headless harness | `tests/client_turns.rs`, `tests/compact.rs`, `tests/mobs.rs`, `tests/attach*.rs`, … | The one rig: the real `rkat-tui-mob` daemon started in the test process with only the LLM scripted, and attach clients driven through the UI's `Effect`/`Msg` channels. `tests/common/rig.rs` has `Setup`, `Attached`, `Running`, `Client`, `AllowAll`, the shared `ScriptedClassifier` and `stored_session`; `tests/common/mod.rs` has the scripted LLM (`ScriptedLlmClient`, `Step`), `Workspace` and the command lines (`args_for`, `args_with`). No keys, no network |
| Daemon wire | `tests/daemon.rs` | The raw JSON-RPC wire below the attach client (handshake, the proxy's method allow-list, endpoint files) |
| Rendering | `tests/render.rs` | Draws the UI into a ratatui `TestBackend` and asserts on rows; there are no snapshot files to update |
| Confinement | `tests/confinement.rs` (and the confined rows of `tests/daemon.rs`) | Probes the host sandbox; prints `SKIPPED: <reason>` and passes where the backend is unavailable (nested namespaces, Ubuntu's user-namespace restriction), unless `RKAT_TUI_REQUIRE_SANDBOX=1`, which turns the skip into a failure |
| Live | `tests/e2e_classifier.rs`, `--lib e2e_list_models` | `#[ignore]`; need provider keys. Not part of the gate |

The live lanes skip a provider whose key is not in the environment:

```bash
ANTHROPIC_API_KEY=… OPENAI_API_KEY=… GEMINI_API_KEY=… cargo test --test e2e_classifier -- --ignored
ANTHROPIC_API_KEY=… OPENAI_API_KEY=… GEMINI_API_KEY=… cargo test --lib e2e_list_models -- --ignored
```

Every behaviour change ships with a test in the matching place. A UI change
gets a `tests/render.rs` row assertion or an `App::update` unit test; a
client or daemon change gets a test on the daemon rig (`ScriptedLlmClient`
for the model); an approval change gets a routing test in `src/approval/`
plus, if it touches the service gate, a case in
`tests/approval_service.rs`. A key binding change updates `KEYS_HELP` in
`src/ui/commands.rs`, which is hand-kept: the unit test
`keys_help_names_every_keymap_action_and_nothing_else` fails until the
key reference and the keymap agree.

## The tests leave the user's state directory alone

The daemon's endpoint files live in the user's state directory
(`$XDG_STATE_HOME/rkat-tui/<key>/`). No test writes there: the rig's
daemons write theirs beside the test's workspace (`endpoint_file_for`),
and the one test that needs the default place, because the temp dir is
granted to its confined shell
(`the_agent_shell_runs_confined`), puts it under a state directory
of its own in Cargo's target dir (`DaemonOptions::with_state_home`), which
no grant covers. A daemon's stop removes its endpoint file and the empty
directory it was in. CI points `XDG_STATE_HOME` at an empty directory for
the test job and fails it if anything is left there; a new test that
starts a daemon on the default place fails that step.

The same holds for the user's config directory, which holds every
workspace's realm (`~/.rkat/tui/workspaces/<id>/`): every scope a test
resolves names `--config-dir` (or `--user-config-root`) of the test's
own. The rig puts it beside the workspace, never in it
(`common::config_dir_for`; a realm the confined shell could reach refuses
the start), and the tests whose shell is granted the temp dir put it under
Cargo's target dir with their endpoint file (`allow_shell_detected`).

## The sandbox rows in CI

CI runs the confinement rows for real: its test job lifts the runner's
AppArmor restriction of unprivileged user namespaces
(`kernel.apparmor_restrict_unprivileged_userns=0`), gives the home an
`.ssh` directory to hide, and sets `RKAT_TUI_REQUIRE_SANDBOX=1`, so a row
that would print `SKIPPED:` (no backend, no `rustc`, no Cargo registry, no
`~/.ssh`) fails instead (`common::sandbox_skipped`). A row that really
ran takes seconds; one that skipped takes a fraction of one. Locally,
without the variable, the rows keep skipping where the host cannot
confine; set it to prove that they ran:

```bash
RKAT_TUI_REQUIRE_SANDBOX=1 cargo nextest run --all-features --test confinement --test daemon
```

## Known flaky tests

These tests were flaky on CI and have been hardened. If one fails again, the
output now says why; report it rather than retrying silently:

- `parallel_escalations_share_one_timeout_including_the_queue`
  (`src/approval/engine.rs`): the paused tokio clock auto-advances while a task
  waits on `spawn_blocking`, so budget checks must happen after every await.
- `workspace_state_survives_a_rename` (`tests/client_turns.rs`; since the
  realm moved out of the workspace, its successors
  `a_moved_workspace_starts_anew_and_is_offered_its_old_state` and the
  re-home tests share its `move_workspace`): copies and
  removes the workspace right after quit and names any file that vanished
  mid-copy (`copy …: No such file`) or was left behind (`left behind: …`).
  The realm's databases are no longer in the copied tree.
  Its hosted failure after the daemon migration was a real shutdown race:
  the daemon's stop returned while engine store connections were still
  open (the schedule store's worker closes its connection at a 250 ms idle
  bound after the last handle is gone), and that close removed
  `sessions.sqlite3-wal` and `-shm` from under the copy. The daemon's
  shutdown now joins its own tasks and waits until the process holds no
  database file of the realm open (`daemon::settle_stores`), so the tree is
  quiet when quit returns. A failure now means a database stayed open past
  that wait (the daemon logs "may still be open at exit").
- `a_watched_change_reaches_the_agent_in_its_turn` (`tests/watches.rs`):
  failed once on the two-core runner (run 43388269247, "the agent's own
  write was not reported"). The daemon learned of the agent's own
  `write_file` only from its diff, which a separate task forwarded after
  the rename, and judged it against the batch's flush time with a 2 s
  window. A batch flushed before that task ran (the 250 ms debounce), or
  flushed late on a starved runtime, reported the agent's own write. The
  file tools now mark the path before they write (`with_write_mark`, pinned
  by `the_write_mark_comes_before_the_file_changes`) and a change is judged
  by when it was seen (`an_own_write_flushed_late_is_still_the_agent_s`).
  No bound was raised.

Paused-clock tests (`#[tokio::test(start_paused = true)]`) count a task
blocked in `spawn_blocking` as idle, so the virtual clock jumps; re-check a
deadline after reacquiring any lock.

## Running from inside rkat-tui

The gate runs from the harness's confined shell: the workspace, the temp
dir and the Cargo home are writable, the rustup home, `/etc/alternatives`
and `/usr/libexec` are readable, the network is open for `cargo fetch`, and
each launch has its own pid namespace with a read-only `/proc` of its own
(so `$$` is 2 under a small init and no host process is visible; the
mount is best effort, so a host that refuses it runs the command without
`/proc`, and a Rust toolchain then cannot find `/proc/self/exe`). `cargo check`, `cargo test` and `cargo
nextest` are tier-1 commands when the harness was started with
`--trust-project-builds` (or `auto.trust_project_builds = true` in
`tui.toml`; the default is `false`), so they run without an approval
prompt; otherwise the classifier reviews each build command, or manual
mode asks you. The confinement tests print `SKIPPED:` lines when the
sandbox refuses to nest; that is a pass.

A `no /proc/self/exe available` or `Operation not permitted` from cargo
inside the harness means the binary predates the sandbox fixes of
2026-11-07; rebuild it.

## Before committing

1. The full gate is green (every command, exit code 0).
2. `git status` shows only the files the change needs; no stray `target/`,
   scratch or snapshot files.
3. `CHANGELOG.md` has a line under `## [Unreleased]` for a user-visible change (this repository keeps one).
4. The commit message says what changed and why, and ends with the
   `Co-Authored-By:` trailer for the agent that wrote it.

## Manual smoke (a person at a terminal)

Not automated; worth doing after changes to keys, rendering or the approval
flow. Start `cargo run -- --no-auto` in a scratch git repository (it starts
the workspace's daemon and attaches to it):

1. Type a prompt, press Enter; while the turn runs, type another line and
   press Enter: it shows as queued and is delivered at the next boundary.
   Up on an empty input takes it back into the editor; Esc interrupts the
   turn and sends what is queued next.
2. `/` opens the command menu; `@` offers files; `!ls` runs a shell
   command and shows its output in the transcript.
3. Ask for a file edit: the approval prompt appears; `n` with a comment
   denies it and the comment reaches the model.
4. Shift+Tab cycles manual, accept edits, plan, auto in the status bar.
5. Ctrl+T shows the task checklist; Ctrl+O toggles the latest tool output;
   `?` opens the key reference; Ctrl+R searches history.
6. `/help` says there is no rewind; `/resume` lists the workspace's
   conversations. A `!` command runs as you (`!echo $HOME` shows your
   home, `!git status` works with your git config).
7. `/detach` leaves with the daemon running; `rkat-tui -c` reattaches
   to the same agent and conversation. `/quit` then asks whether to
   detach or stop the daemon (it stops without asking when this
   `rkat-tui` started the daemon, is the last one attached and no agent
   was left detached); after a stop, `rkat-tui -c` resumes the
   conversation in a fresh daemon.
8. A second `rkat-tui` in the same directory gets an agent of its own and
   names the first; `/agents` lists both. Ask one agent to tell the other
   something: the other's TUI shows the message labelled with the
   sender, and the other agent answers it. `/quit` in the second TUI
   retires its agent; the first one is told it left.

## Releasing a 0.0.x

The 0.0.x line is the maintainer's dogfood line on crates.io: each
`0.0.(n+1)` carries the fixes from using `0.0.n`.

1. Set `version = "0.0.N"` in `Cargo.toml` (`cargo check` updates
   `Cargo.lock`), and move `## [Unreleased]` in `CHANGELOG.md` into a new
   `## [0.0.N] - <date>` under an empty `## [Unreleased]`.
2. Run the gate, then `cargo publish --dry-run --locked`; commit and push.
3. The maintainer tags and publishes:
   `git tag -a v0.0.N -m "rkat-tui 0.0.N" && git push origin v0.0.N`,
   then `cargo publish --locked`.
   The tag's push runs `.github/workflows/release.yml`, which builds the
   archives `cargo binstall` fetches and attaches them to the tag's
   GitHub release; when a run fails, fix the workflow on `main` and start
   it by hand for the same tag (`gh workflow run release.yml -f
   tag=v0.0.N`).
4. On the other machine: `cargo install rkat-tui --locked` (`--force` over
   an older one), `rkat-tui --version`, and record what goes wrong as
   issues in the repository.
