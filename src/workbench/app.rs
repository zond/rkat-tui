//! The UI both runners start: [`AppSetup`] is everything about it that
//! comes from the configuration, [`build_app`] the one place it becomes an
//! [`App`]; [`open`] and [`drive`] are the terminal session around it
//! (input histories, the prompt argument, waiting for the runner on quit).

use std::path::{Path, PathBuf};
use std::time::Duration;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::SavedUnsent;
use crate::bootstrap::RuntimeScope;
use crate::config::UiSettings;
use crate::error::TuiError;
use crate::paths::ProjectRoot;
use crate::ui::{App, CompactionPolicy, Effect, History, Msg, StatusInfo, UiChannels, run_ui};

/// What the UI takes from the configuration, the same for both runners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSetup {
    /// The context-compaction policy of the `tui.toml` layers.
    pub compaction: CompactionPolicy,
    /// The `[ui]` settings of the `tui.toml` layers.
    pub ui: UiSettings,
    /// Whether the `!` shell runs confined (the title says the sandbox).
    pub shell_confined: bool,
    /// The workspace (`@` completion).
    pub workspace: ProjectRoot,
}

/// The UI on `setup`, with this status and these input histories.
#[must_use]
pub fn build_app(
    setup: &AppSetup,
    status: StatusInfo,
    history: &[String],
    global_history: &[String],
) -> App {
    App::new(status)
        .with_compaction_policy(setup.compaction)
        .with_helper_view(setup.ui.helpers)
        .with_checklist_view(setup.ui.checklist)
        .with_bell(setup.ui.bell)
        .with_shell_confined(setup.shell_confined)
        .with_workspace(setup.workspace.clone())
        .with_history(history.to_vec())
        .with_global_history(global_history.to_vec())
}

/// The UI ready for the terminal, with the histories it saves to.
pub(crate) struct Opened {
    app: App,
    histories: Vec<History>,
    drafts: PathBuf,
}

/// Build the UI on `setup` with the realm's and the user's input
/// histories, and submit `prompt` (the command line's) as if typed, so the
/// UI shows it and knows a turn runs.
pub(crate) fn open(
    setup: &AppSetup,
    status: StatusInfo,
    scope: &RuntimeScope,
    prompt: Option<String>,
    effect_tx: &mpsc::UnboundedSender<Effect>,
) -> Opened {
    let realm_root = scope.realm_root();
    let history = History::load(realm_root.join("tui_history"));
    let global_history = scope
        .global
        .as_ref()
        .map(|global| History::load(global.history_file()));
    let mut app = build_app(
        setup,
        status,
        history.entries(),
        global_history
            .as_ref()
            .map(History::entries)
            .unwrap_or_default(),
    );
    if let Some(prompt) = prompt {
        let mut first = app.update(Msg::Paste(prompt));
        first.extend(app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))));
        for effect in first {
            let _ = effect_tx.send(effect);
        }
    }
    Opened {
        app,
        histories: std::iter::once(history).chain(global_history).collect(),
        drafts: realm_root.join("drafts"),
    }
}

/// Run the UI until the user quits, then wait (up to `limit`) for the
/// runner to wind down and say where it saved the unsent messages.
///
/// # Errors
///
/// When the terminal cannot be driven.
pub(crate) async fn drive(
    opened: Opened,
    channels: UiChannels,
    effect_tx: mpsc::UnboundedSender<Effect>,
    mouse: bool,
    runner: JoinHandle<Option<SavedUnsent>>,
    limit: Duration,
) -> Result<(), TuiError> {
    let Opened {
        app,
        histories,
        drafts,
    } = opened;
    let ui_result = run_ui(app, channels, effect_tx, mouse, histories, drafts).await;
    match tokio::time::timeout(limit, runner).await {
        Ok(Ok(Some(saved))) => eprintln!(
            "rkat-tui: {} unsent message(s) saved in {}",
            saved.count,
            saved.path.display()
        ),
        Ok(Ok(None)) => {}
        Ok(Err(err)) => tracing::warn!(error = %err, "runner task failed"),
        Err(_) => tracing::warn!("runner did not shut down in time"),
    }
    ui_result.map_err(TuiError::from)
}

/// Route tracing to a log file inside the realm so nothing writes to the
/// terminal while the UI owns it.
pub(crate) fn init_logging(realm_root: &Path) -> Result<(), TuiError> {
    let log_dir = realm_root.join("logs");
    std::fs::create_dir_all(&log_dir).map_err(|e| TuiError::io("create log directory", e))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("rkat-tui.log"))
        .map_err(|e| TuiError::io("open log file", e))?;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // A second init (tests) is not an error worth failing the run for.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file))
        .try_init();
    Ok(())
}
