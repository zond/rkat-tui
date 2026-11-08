# Test protocol

How to verify a change to `rkat-tui` before it is committed. CI
(`.github/workflows/ci.yml`) runs exactly the gate below on Rust 1.97.0, so a
green gate locally is a green push.

## The gate

Run from the repository root, in this order, and check every exit code:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-features
```

A failure anywhere means the change is not done. Never commit on red, and
never "fix" a red step by loosening a lint or deleting a test. `cargo fmt
--all` (without `--check`) fixes formatting; the other two are fixed by
changing code.

Warm, the whole gate takes a few minutes. The first build after a toolchain
or dependency change takes well over ten. Plain `cargo` is correct here: this
repository is not the Meerkat workspace, so none of its wrappers or lane
variables apply. `.cargo/config.toml` already sets the test-thread stack and
the SQLite flags the engine needs; do not override `RUST_MIN_STACK` or
`LIBSQLITE3_FLAGS`.

## While iterating

Scope the run to what changed, then run the full gate once at the end:

```bash
cargo nextest run --all-features --lib                 # unit tests only
cargo nextest run --all-features --test integrator     # one integration binary
cargo nextest run --all-features -E 'test(rewind)'     # tests whose name contains "rewind"
cargo nextest run --all-features --no-fail-fast        # see every failure at once
```

Clippy must be run with `--all-targets --all-features`; a narrower
invocation misses the test targets and the `mob` and `test-support` code.

## Where tests live

| Kind | Where | Notes |
|---|---|---|
| Unit | `#[cfg(test)]` modules next to the code in `src/` | Pure logic: lexer, rules, keymap, input editing, completion, approval routing |
| Integration | `tests/*.rs` | Drive the internal modules through the `test-support` feature (the crate's dev-dependency on itself turns it on) |
| Headless runner | `tests/integrator.rs`, `tests/fork.rs`, `tests/compact.rs`, … | A real runtime-backed session with a scripted LLM (`tests/common/mod.rs`: `ScriptedClient`, `Step`, `args_for`, `Workspace`); no keys, no network |
| Rendering | `tests/render_snapshot.rs` | Draws the UI into a ratatui `TestBackend` and asserts on rows; there are no snapshot files to update |
| Confinement | `tests/confinement.rs` | Probes the host sandbox; prints `SKIPPED: <reason>` and passes where the backend is unavailable (nested namespaces, Ubuntu's user-namespace restriction) |
| Live | `tests/e2e_classifier.rs`, `--lib e2e_list_models` | `#[ignore]`; need provider keys. Not part of the gate |

The live lanes skip a provider whose key is not in the environment:

```bash
ANTHROPIC_API_KEY=… OPENAI_API_KEY=… GEMINI_API_KEY=… cargo test --test e2e_classifier -- --ignored
ANTHROPIC_API_KEY=… OPENAI_API_KEY=… GEMINI_API_KEY=… cargo test --lib e2e_list_models -- --ignored
```

Every behaviour change ships with a test in the matching place. A UI change
gets a `render_snapshot` row assertion or an `App::update` unit test; a
runner or session change gets a scripted-client test; an approval change gets
a routing test in `src/approval/` plus, if it touches the service gate, a
case in `tests/approval_service.rs`.

## Known flaky tests

Two tests were flaky on CI and have been hardened. If either fails again, the
output now says why; report it rather than retrying silently:

- `parallel_escalations_share_one_timeout_including_the_queue`
  (`src/approval/engine.rs`): the paused tokio clock auto-advances while a task
  waits on `spawn_blocking`, so budget checks must happen after every await.
- `workspace_state_survives_a_rename` (`tests/integrator.rs`): removes the
  workspace right after quit and names any file left behind
  (`left behind: …`). A leftover means something still writes after
  `Runner::quit` returned.

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

1. The full gate is green (all three commands, exit code 0).
2. `git status` shows only the files the change needs; no stray `target/`,
   scratch or snapshot files.
3. `CHANGELOG.md` has a line under `## [Unreleased]` for a user-visible change (this repository keeps one).
4. The commit message says what changed and why, and ends with the
   `Co-Authored-By:` trailer for the agent that wrote it.

## Manual smoke (a person at a terminal)

Not automated; worth doing after changes to keys, rendering or the approval
flow. Start `cargo run -- --no-auto` in a scratch git repository:

1. Type a prompt, press Enter; while the turn runs, type another line and
   press Enter: it shows as queued and is delivered at the next boundary.
   Press Esc while it is queued: it comes back into the editor.
2. `/` opens the command menu; `@` offers files; `!ls` runs a shell
   command and shows its output in the transcript.
3. Ask for a file edit: the approval prompt appears; `n` with a comment
   denies it and the comment reaches the model.
4. Shift+Tab cycles manual, accept edits, plan, auto in the status bar.
5. Ctrl+T shows the task checklist; Ctrl+O toggles the latest tool output;
   `?` opens the key reference; Ctrl+R searches history.
6. `/rewind` lists your messages; picking one forks the session and puts
   the message back in the editor; `/resume` still offers the old session.
7. `/quit` exits cleanly and `rkat-tui -c` resumes the session.
