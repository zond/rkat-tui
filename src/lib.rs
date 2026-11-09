//! `rkat-tui`: an interactive terminal coding harness built on the Meerkat agent engine.
//!
//! The binary hosts the agent in-process on the runtime-backed session path
//! (a `PersistentSessionService` driven by a `MeerkatMachine`): prompts are
//! runtime inputs, messages typed mid-turn are durable steers the runtime
//! writes into the conversation at the next step, and sessions persist in
//! the workspace (`.rkat/tui/`) for `-c` / `--resume`. Every tool call is
//! gated by an auto-mode approval model installed as each session's
//! `ToolDispatchAdmission` ([`approval`]).
//!
//! The public API is [`run`], [`Args`] and [`TuiError`] (with the error
//! types its variants carry). Everything else is internal; the
//! `test-support` feature makes the modules public for the crate's own
//! integration tests and adds [`harness`] (headless runner). It is not a
//! stable API.
//!
//! Module map:
//! - `cli` — command-line arguments
//! - `error` — the surface's error type and exit codes
//! - `paths` — project-root path resolution and protected-path policy
//! - `http` — bounded HTTP body reads
//! - `delivery` — the contract for messages steered into a running turn
//! - `overlay` — the `--set`/`--allow`/… command-line layer over `tui.toml`
//! - `pricing` — token prices for the cost readout
//! - `approval` — auto-mode approval engine (tiers, rules, shell analysis, classifier, escalation)
//! - `tools` — in-crate file tools
//! - `memory` — file-based notes
//! - `endpoints` — LLM endpoints, `endpoint:model` references, their projection into the realm config
//! - `models` — live model lists of every endpoint (never the catalog)
//! - `thinking` — `[thinking]`: Anthropic thinking per model, carried in each session's provider params
//! - `bootstrap` — realm/config/factory/service construction, provider credentials
//! - `mobs` — helpers (sub-agents) as mob members: approval gate, model binding, observation
//! - `session` — session handle: create, resume, turns, steer, interrupt
//! - `runner` — the event loop joining the session and the UI
//! - `mentions` — `@path` mentions: the workspace path index and the attachment of mentioned files
//! - `user_shell` — `!` shell mode: the user's commands under the agent shell's confinement
//! - `ui` — ratatui application
//! - `daemon` — the `rkat-tui-mob` daemon: the engine's JSON-RPC router over loopback TCP
//! - `client` — attach mode: the TUI against a running daemon (`--attach`)

/// Declare the internal modules: crate-private, public with the
/// `test-support` feature (the crate's integration tests). Without it, an
/// item or re-export nothing in the binaries uses is dead code, which the
/// default-features clippy lane (`cargo clippy --lib --bins`) refuses: an
/// item only tests use is declared under
/// `#[cfg(any(test, feature = "test-support"))]` (see `docs/testing.md`).
macro_rules! internal_modules {
    ($($name:ident),* $(,)?) => {
        $(
            #[cfg(feature = "test-support")]
            pub mod $name;
            #[cfg(not(feature = "test-support"))]
            pub(crate) mod $name;
        )*
    };
}

internal_modules!(
    approval, bootstrap, cli, delivery, endpoints, error, fork, http, memory, mentions, mobs,
    models, overlay, paths, pricing, runner, session, thinking, tools, ui, user_shell,
);

/// `rkat-tui-mob`: the workspace's mob daemon (experimental). Internal
/// like the modules above; its binary enters through [`run_daemon`].
#[cfg(all(feature = "mob", feature = "test-support"))]
pub mod daemon;
#[cfg(all(feature = "mob", not(feature = "test-support")))]
pub(crate) mod daemon;

/// Attach mode (experimental): the TUI as a client of the workspace's
/// `rkat-tui-mob` daemon. Internal like the modules above.
#[cfg(all(feature = "mob", feature = "test-support"))]
pub mod client;
#[cfg(all(feature = "mob", not(feature = "test-support")))]
pub(crate) mod client;

/// The `rkat-tui-mob` daemon's command line and entry point.
#[cfg(feature = "mob")]
pub use daemon::{DaemonCli, run as run_daemon};

/// The headless runner the integration tests drive (see
/// `runner::testing`). Not a stable API.
#[cfg(feature = "test-support")]
pub mod harness {
    pub use crate::runner::testing::*;
}

pub use approval::ApprovalError;
pub use bootstrap::confinement::ConfinementError;
pub use cli::Args;
pub use endpoints::{EndpointError, ModelRefError};
pub use error::TuiError;
pub use overlay::OverlayError;
pub use session::AdmissionError;
pub use ui::UiError;

/// Run the TUI with the given arguments: bootstrap the realm and the
/// runtime-backed service, open the start session (`-c`, `--resume` or a
/// fresh one; the prompt argument becomes its first turn) and drive the
/// terminal until the user quits.
///
/// # Errors
///
/// Returns a [`TuiError`] when there is no terminal, bootstrap fails, the
/// session cannot be opened, or the terminal cannot be driven.
pub async fn run(args: Args) -> Result<(), TuiError> {
    match bootstrap::attach_mode(&args)? {
        None => runner::run(args).await,
        #[cfg(feature = "mob")]
        Some(mode) => client::run(args, mode).await,
        #[cfg(not(feature = "mob"))]
        Some(_) => Err(TuiError::AttachUnsupported),
    }
}
