//! Terminal ownership and the UI event loop.
//!
//! All crossterm types come from `ratatui::crossterm` so the backend and our
//! commands can never disagree on the crossterm version; the `crossterm` crate is
//! used directly only for its `event-stream` feature (`EventStream`).

use std::io::{self, Stdout, Write};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use meerkat_core::{AgentEvent, EventEnvelope};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyEventKind, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use tokio::sync::mpsc;

use super::editor::{edit_draft, editor_from_env, run_editor};
use super::render::draw;
use super::selection::{copy_notice, osc52};
use super::{App, EditorClosed, EditorTarget, Effect, History, LocalEffect, Msg, QueuedCopy};

/// Render tick interval (~30 fps).
pub const TICK: Duration = Duration::from_millis(33);

/// Whether keyboard-enhancement flags were pushed (so restore knows to pop them).
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);
/// Whether mouse capture is enabled.
static MOUSE_CAPTURED: AtomicBool = AtomicBool::new(false);
/// Whether the terminal is currently in TUI mode.
static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();

/// Errors from the UI runtime.
#[derive(Debug, thiserror::Error)]
pub enum UiError {
    /// Terminal I/O failed.
    #[error("terminal I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The integrator dropped the effect receiver.
    #[error("effect channel closed")]
    EffectChannelClosed,
}

/// Restore the terminal to cooked mode. Idempotent and safe to call from a panic hook.
fn restore_terminal() {
    if !TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = io::stdout();
    if KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if MOUSE_CAPTURED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, DisableMouseCapture);
    }
    let _ = execute!(
        out,
        DisableFocusChange,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        ratatui::crossterm::cursor::Show
    );
    let _ = disable_raw_mode();
    let _ = out.flush();
}

/// RAII owner of the terminal's TUI mode (raw mode, alternate screen, paste, keyboard flags).
#[derive(Debug)]
pub struct TerminalGuard {
    _private: (),
}

impl TerminalGuard {
    /// Enter TUI mode. `mouse` enables mouse capture (on by default: wheel
    /// scrolling and click-to-expand; native selection then needs Shift).
    pub fn enter(mouse: bool) -> io::Result<Self> {
        // From the first step on, `Drop` restores whatever was enabled if a
        // later step fails.
        let guard = Self { _private: () };
        enter_tui_mode(mouse)?;
        Ok(guard)
    }
}

/// Raw mode, the alternate screen, bracketed paste, focus reports, keyboard
/// flags and (with `mouse`) mouse capture; [`restore_terminal`] undoes it.
fn enter_tui_mode(mouse: bool) -> io::Result<()> {
    enable_raw_mode()?;
    TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
    let mut out = io::stdout();
    // Focus reports let the bell ring only while the terminal is
    // unfocused; a terminal without them never reports a loss.
    execute!(
        out,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableFocusChange
    )?;
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        // Disambiguation only: it tells Esc from Alt sequences and
        // Shift/Ctrl+Enter from Enter. Event types are not requested:
        // under them kitty 0.32 sends the release of a legacy-encoded
        // key (Backspace, Enter, Tab) as a second press byte, which
        // deleted two characters per Backspace; the spec says those keys
        // get no release event, and the UI needs none.
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
    }
    if mouse {
        execute!(out, EnableMouseCapture)?;
        MOUSE_CAPTURED.store(true, Ordering::SeqCst);
    }
    Ok(())
}

/// What the loop still has to do, collected from the [`LocalEffect`]s of
/// the updates since it last acted (copies and history lines are handled
/// as they arrive).
#[derive(Debug, Default)]
struct LocalEffects {
    /// The state changed: draw a frame.
    draw: bool,
    /// Ctrl-L: a full repaint.
    redraw: bool,
    /// Ctrl-Z: suspend to the shell.
    suspend: bool,
    /// `/memory`, Ctrl-G: run the user's editor.
    editor: Option<EditorTarget>,
    /// Ring the bell after the next frame.
    bell: bool,
    /// List the workspace's paths (the generation and the root).
    index: Option<(u64, crate::paths::ProjectRoot)>,
    /// The realm changed: its directory.
    reroot: Option<std::path::PathBuf>,
}

/// Leave TUI mode, run `job` with the terminal in cooked mode, then come
/// back: the event reader is stopped first (a new stream starts reading
/// only when polled), keys that arrived meanwhile are dropped, and the
/// screen is cleared for a full repaint.
async fn outside_tui<T>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    events: &mut crossterm::event::EventStream,
    mouse: bool,
    job: impl std::future::Future<Output = T>,
) -> Result<T, UiError> {
    // Dropping the old stream stops its reader thread.
    *events = crossterm::event::EventStream::new();
    restore_terminal();
    let result = job.await;
    enter_tui_mode(mouse)?;
    while ratatui::crossterm::event::poll(Duration::ZERO)? {
        let _ = ratatui::crossterm::event::read()?;
    }
    terminal.clear()?;
    Ok(result)
}

/// Run the user's editor on `target` outside TUI mode (on a blocking
/// thread, so the runtime's workers stay free) and report how it ended. An
/// editor variable naming no program is refused without leaving TUI mode.
async fn run_external_editor(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    events: &mut crossterm::event::EventStream,
    mouse: bool,
    target: EditorTarget,
    scratch: &std::path::Path,
) -> Result<EditorClosed, UiError> {
    let command = match editor_from_env() {
        Ok(command) => command,
        Err(refused) => return Ok(editor_failed(target, refused)),
    };
    let fallback = target.clone();
    let scratch = scratch.to_path_buf();
    let job = tokio::task::spawn_blocking(move || match target {
        EditorTarget::Memory { target, path } => EditorClosed::Memory {
            target,
            result: run_editor(&command, &path),
            path,
        },
        EditorTarget::Draft(text) => EditorClosed::Draft(edit_draft(&text, &scratch, |path| {
            run_editor(&command, path)
        })),
    });
    let joined = outside_tui(terminal, events, mouse, job).await?;
    Ok(joined
        .unwrap_or_else(|err| editor_failed(fallback, format!("the editor task failed: {err}"))))
}

/// `target`'s editor run failed before it could say more: `why`.
fn editor_failed(target: EditorTarget, why: String) -> EditorClosed {
    match target {
        EditorTarget::Memory { target, path } => EditorClosed::Memory {
            target,
            path,
            result: Err(why),
        },
        EditorTarget::Draft(_) => EditorClosed::Draft(Err(why)),
    }
}

/// Ctrl-Z: stop this process with SIGTSTP, as the terminal's own suspend
/// key would; returns once the shell continues it (`fg`).
#[cfg(unix)]
fn stop_self() {
    if let Err(err) = nix::sys::signal::raise(nix::sys::signal::Signal::SIGTSTP) {
        tracing::warn!(error = %err, "could not suspend");
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Install (once) a panic hook that restores the terminal before delegating to the
/// previous hook, so panic messages are readable.
pub fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    });
}

/// Receivers feeding the UI loop.
#[derive(Debug)]
pub struct UiChannels {
    /// Primary agent stream events.
    pub agent_rx: mpsc::Receiver<EventEnvelope<AgentEvent>>,
    /// Integrator messages: escalations, diffs, approvals, turn completion, status, notices.
    pub ui_rx: mpsc::UnboundedReceiver<Msg>,
}

/// `msg` (from the integrator) preceded by every agent event already
/// buffered (helpers' scoped events arrive as integrator messages, in
/// order). The runner sends `TurnFinished`, `SteerAck`,
/// `SteerDelivered` and `SteerRequeued` after the events that led to them,
/// but on a different channel;
/// applying the buffered events first keeps a turn's text from landing after
/// its summary (or in the next turn).
pub fn after_buffered_events(msg: Msg, channels: &mut UiChannels) -> Vec<Msg> {
    let mut batch = Vec::new();
    while let Ok(env) = channels.agent_rx.try_recv() {
        batch.push(Msg::Agent(env));
    }
    batch.push(msg);
    batch
}

/// The most messages applied before a frame is drawn.
pub const MAX_BATCH: usize = 256;

/// Append to `batch` what the runner has already queued (each message
/// after the agent events buffered before it), up to [`MAX_BATCH`]
/// messages, so a burst of stream events costs one frame rather than one
/// per event. Terminal input is not read here: it is drawn with the next
/// frame, at most one batch later.
pub fn drain_queued(batch: &mut Vec<Msg>, channels: &mut UiChannels) {
    while batch.len() < MAX_BATCH {
        match channels.ui_rx.try_recv() {
            Ok(msg) => batch.extend(after_buffered_events(msg, channels)),
            Err(_) => return,
        }
    }
}

fn translate(event: Event, mouse: bool) -> Option<Msg> {
    if let Event::Key(key) = &event {
        // `RUST_LOG=rkat_tui::ui::runtime=trace` shows every key event
        // as the terminal reported it (kind included), for input bugs.
        tracing::trace!(?key, "key event");
    }
    match event {
        // Windows reports Press and Release; a terminal asked for event types
        // would report Repeat and Release too (none is asked today).
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            Some(Msg::Key(key))
        }
        Event::Paste(text) => Some(Msg::Paste(text)),
        Event::Resize(..) => Some(Msg::Resize),
        Event::Mouse(m) if mouse => Some(Msg::Mouse(m)),
        Event::FocusGained => Some(Msg::Focus(true)),
        Event::FocusLost => Some(Msg::Focus(false)),
        _ => None,
    }
}

/// Run the UI until the user quits.
///
/// Effects produced by [`App::update`] are forwarded on `effects`; [`Effect::Quit`]
/// is forwarded too and then ends the loop. Every submitted line is appended to
/// each of `histories` (e.g. the workspace and the global history file). The
/// terminal is restored on return, on error and on panic.
pub async fn run_ui(
    mut app: App,
    mut channels: UiChannels,
    effects: mpsc::UnboundedSender<Effect>,
    mouse: bool,
    mut histories: Vec<History>,
    mut scratch: std::path::PathBuf,
) -> Result<(), UiError> {
    install_panic_hook();
    let _guard = TerminalGuard::enter(mouse)?;
    let mut terminal: Terminal<CrosstermBackend<Stdout>> =
        Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    let tmux = std::env::var_os("TMUX").is_some();
    // Copies queued by the last batch: written after the next frame, on the
    // handle ratatui renders to, so the OSC 52 sequence cannot interleave
    // with a frame.
    let mut copies: Vec<QueuedCopy> = Vec::new();
    let mut events = crossterm::event::EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // What the updates asked the loop itself to do ([`LocalEffect`]).
    let mut local = LocalEffects::default();
    // Path listings walked off the UI thread come back here.
    let (index_tx, mut index_rx) = mpsc::unbounded_channel::<Msg>();
    loop {
        absorb(&mut app, &mut histories, &mut copies, &mut local);
        if let Some((generation, root)) = local.index.take() {
            let tx = index_tx.clone();
            tokio::task::spawn_blocking(move || {
                let paths = crate::mentions::workspace_paths(&root);
                let _ = tx.send(Msg::PathIndex { generation, paths });
            });
        }
        if let Some(realm) = local.reroot.take() {
            reroot(&mut app, &mut histories, &realm);
            scratch = realm.join(DRAFTS);
        }
        if std::mem::take(&mut local.suspend) {
            #[cfg(unix)]
            outside_tui(&mut terminal, &mut events, mouse, async { stop_self() }).await?;
            local.redraw = true;
        }
        if let Some(target) = local.editor.take() {
            let closed =
                run_external_editor(&mut terminal, &mut events, mouse, target, &scratch).await?;
            if apply(
                &mut app,
                Msg::EditorClosed(closed),
                &effects,
                &mut histories,
                &mut copies,
                &mut local,
            )? {
                return Ok(());
            }
            local.redraw = true;
        }
        if std::mem::take(&mut local.redraw) {
            terminal.clear()?;
            terminal.draw(|frame| draw(frame, &mut app))?;
            local.draw = false;
        } else if std::mem::take(&mut local.draw) {
            terminal.draw(|frame| draw(frame, &mut app))?;
        }
        if std::mem::take(&mut local.bell) {
            ring_bell(terminal.backend_mut())?;
        }
        for copy in copies.drain(..) {
            let (level, text) = copy_notice(
                copy.text.chars().count(),
                copy.truncated,
                write_clipboard(&copy.text, tmux, terminal.backend_mut()).as_ref(),
            );
            // A notice produces no effects.
            let _ = app.update(Msg::Notice { level, text });
        }
        // Agent events first: integrator messages refer to events sent
        // before them (see `after_buffered_events`).
        let batch = tokio::select! {
            biased;
            Some(env) = channels.agent_rx.recv() => {
                let mut batch = vec![Msg::Agent(env)];
                drain_queued(&mut batch, &mut channels);
                batch
            }
            Some(msg) = channels.ui_rx.recv() => {
                let mut batch = after_buffered_events(msg, &mut channels);
                drain_queued(&mut batch, &mut channels);
                batch
            }
            Some(msg) = index_rx.recv() => vec![msg],
            maybe = events.next() => match maybe {
                Some(Ok(event)) => match translate(event, mouse) {
                    Some(msg) => vec![msg],
                    None => continue,
                },
                Some(Err(err)) => return Err(UiError::Io(err)),
                None => {
                    // Input stream ended (terminal gone): tell the integrator to shut down.
                    let _ = effects.send(Effect::Quit);
                    return Ok(());
                }
            },
            _ = tick.tick() => vec![Msg::Tick],
        };
        for msg in batch {
            if apply(
                &mut app,
                msg,
                &effects,
                &mut histories,
                &mut copies,
                &mut local,
            )? {
                return Ok(());
            }
        }
    }
}

/// Write BEL to `out` (the terminal backend, after a frame).
fn ring_bell(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\x07")?;
    out.flush()
}

/// Write `text` to `out` (the terminal backend, after a frame) as an OSC 52
/// sequence for the clipboard and the primary selection (wrapped for tmux),
/// and flush it.
fn write_clipboard(text: &str, tmux: bool, out: &mut impl Write) -> io::Result<()> {
    let seq = osc52(text, tmux);
    out.write_all(seq.as_bytes())?;
    out.flush()
}

/// Apply one message, take what it asks of the loop ([`absorb`]) and
/// forward its effects to the integrator. Returns whether the user quit.
fn apply(
    app: &mut App,
    msg: Msg,
    effects: &mpsc::UnboundedSender<Effect>,
    histories: &mut [History],
    copies: &mut Vec<QueuedCopy>,
    local: &mut LocalEffects,
) -> Result<bool, UiError> {
    let effects_now = app.update(msg);
    absorb(app, histories, copies, local);
    for effect in effects_now {
        let quit = effect == Effect::Quit;
        if effects.send(effect).is_err() && !quit {
            return Err(UiError::EffectChannelClosed);
        }
        if quit {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Drain the app's [`LocalEffect`]s: append submitted lines to every
/// history, queue copies (written after the next frame), and note the rest
/// in `local` for the loop.
fn absorb(
    app: &mut App,
    histories: &mut [History],
    copies: &mut Vec<QueuedCopy>,
    local: &mut LocalEffects,
) {
    for effect in app.take_local() {
        match effect {
            LocalEffect::Submitted(entry) => {
                for history in histories.iter_mut() {
                    if let Err(err) = history.append(&entry) {
                        tracing::warn!(
                            error = %err,
                            path = %history.path().display(),
                            "could not append to input history"
                        );
                    }
                }
            }
            LocalEffect::Copy(copy) => copies.push(copy),
            LocalEffect::ExternalEditor(target) => local.editor = Some(target),
            LocalEffect::Suspend => local.suspend = true,
            LocalEffect::Redraw => local.redraw = true,
            LocalEffect::Bell => local.bell = true,
            LocalEffect::IndexPaths { generation, root } => local.index = Some((generation, root)),
            LocalEffect::Draw => local.draw = true,
            LocalEffect::Reroot(realm) => local.reroot = Some(realm),
        }
    }
}

/// The realm's input history file and drafts directory names.
pub const HISTORY: &str = "tui_history";
/// See [`HISTORY`].
pub const DRAFTS: &str = "drafts";

/// The realm changed to `realm`: the first of `histories` (the realm's)
/// is that realm's from now on, and so is what the UI recalls.
fn reroot(app: &mut App, histories: &mut [History], realm: &std::path::Path) {
    let path = realm.join(HISTORY);
    let Some(first) = histories.first_mut() else {
        return;
    };
    if first.path() != path {
        *first = History::load(path);
        app.replace_history(first.entries().to_vec());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::ui::{Block, StatusInfo, TurnOutcome, TurnSummary};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventState, KeyModifiers};

    /// A burst of queued stream events is one batch, so one frame: the
    /// loop draws once per batch, not once per event. A burst larger than
    /// [`MAX_BATCH`] is split, so input still draws within a batch.
    #[tokio::test]
    async fn queued_deltas_are_applied_as_one_batch() {
        let (_agent_tx, agent_rx) = mpsc::channel(16);
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let mut channels = UiChannels { agent_rx, ui_rx };
        let delta = |seq| {
            Msg::Agent(EventEnvelope::new(
                "s",
                seq,
                None,
                AgentEvent::TextDelta {
                    delta: "x".into(),
                    assistant_message_id: None,
                },
            ))
        };
        for seq in 0..100 {
            ui_tx.send(delta(seq)).unwrap();
        }
        let first = channels.ui_rx.recv().await.unwrap();
        let mut batch = after_buffered_events(first, &mut channels);
        drain_queued(&mut batch, &mut channels);
        assert_eq!(batch.len(), 100);
        assert!(channels.ui_rx.try_recv().is_err());
        for seq in 0..(MAX_BATCH as u64 + 10) {
            ui_tx.send(delta(seq)).unwrap();
        }
        let first = channels.ui_rx.recv().await.unwrap();
        let mut batch = after_buffered_events(first, &mut channels);
        drain_queued(&mut batch, &mut channels);
        assert_eq!(batch.len(), MAX_BATCH);
        let mut rest = Vec::new();
        drain_queued(&mut rest, &mut channels);
        assert_eq!(rest.len(), 10);
    }

    #[tokio::test]
    async fn turn_finished_waits_for_buffered_agent_events() {
        let (agent_tx, agent_rx) = mpsc::channel(16);
        let (ui_tx, ui_rx) = mpsc::unbounded_channel();
        let mut channels = UiChannels { agent_rx, ui_rx };
        let mut app = App::new(StatusInfo::default());
        for c in "hi".chars() {
            app.update(Msg::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        // The turn's text is still buffered when its result arrives.
        for (seq, delta) in [(1, "Hel"), (2, "lo")] {
            agent_tx
                .send(EventEnvelope::new(
                    "s",
                    seq,
                    None,
                    AgentEvent::TextDelta {
                        delta: delta.into(),
                        assistant_message_id: None,
                    },
                ))
                .await
                .unwrap();
        }
        ui_tx
            .send(Msg::TurnFinished(TurnOutcome::plain(Ok(TurnSummary {
                text: "Hello".into(),
                input_tokens: 0,
                output_tokens: 0,
            }))))
            .unwrap();
        let msg = channels.ui_rx.recv().await.unwrap();
        let batch = after_buffered_events(msg, &mut channels);
        assert_eq!(batch.len(), 3);
        assert!(matches!(batch.last(), Some(Msg::TurnFinished(_))));
        for msg in batch {
            app.update(msg);
        }
        let assistant: Vec<&Block> = app
            .transcript
            .blocks()
            .iter()
            .filter(|b| matches!(b, Block::Assistant { .. }))
            .collect();
        assert_eq!(assistant, vec![&Block::Assistant { md: "Hello".into() }]);
        assert!(!app.is_running());
    }

    #[test]
    fn ctrl_l_asks_the_loop_for_a_full_repaint_and_nothing_else() {
        let (effects, mut effect_rx) = mpsc::unbounded_channel();
        let mut app = App::new(StatusInfo::default());
        let (mut copies, mut local) = (Vec::new(), LocalEffects::default());
        let ctrl_l = Msg::Key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        let quit = apply(&mut app, ctrl_l, &effects, &mut [], &mut copies, &mut local).unwrap();
        assert!(!quit && local.redraw && !local.suspend);
        assert!(effect_rx.try_recv().is_err());
    }

    #[test]
    fn ctrl_z_suspends_in_the_loop_and_nothing_reaches_the_integrator() {
        let (effects, mut effect_rx) = mpsc::unbounded_channel();
        let mut app = App::new(StatusInfo::default());
        let (mut copies, mut local) = (Vec::new(), LocalEffects::default());
        let ctrl_z = Msg::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        let quit = apply(&mut app, ctrl_z, &effects, &mut [], &mut copies, &mut local).unwrap();
        assert!(!quit && local.suspend);
        assert!(effect_rx.try_recv().is_err());
    }

    #[test]
    fn the_editor_runs_in_the_loop_and_its_answer_reaches_the_app() {
        use crate::ui::MemoryTarget;
        let (effects, mut effect_rx) = mpsc::unbounded_channel();
        let mut app = App::new(StatusInfo::default());
        let (mut copies, mut local) = (Vec::new(), LocalEffects::default());
        let path = std::path::PathBuf::from("/w/AGENTS.md");
        let open = Msg::EditFile {
            target: MemoryTarget::Workspace,
            path: path.clone(),
        };
        apply(&mut app, open, &effects, &mut [], &mut copies, &mut local).unwrap();
        assert_eq!(
            local.editor.take(),
            Some(EditorTarget::Memory {
                target: MemoryTarget::Workspace,
                path: path.clone(),
            })
        );
        assert!(effect_rx.try_recv().is_err());
        let closed = Msg::EditorClosed(EditorClosed::Memory {
            target: MemoryTarget::Workspace,
            path: path.clone(),
            result: Ok(()),
        });
        apply(&mut app, closed, &effects, &mut [], &mut copies, &mut local).unwrap();
        assert_eq!(
            effect_rx.try_recv().unwrap(),
            Effect::MemoryEdited {
                target: MemoryTarget::Workspace,
                path,
            }
        );

        // Ctrl-G: the draft goes to the loop, not the integrator.
        app.input.set_text("draft");
        let ctrl_g = Msg::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        apply(&mut app, ctrl_g, &effects, &mut [], &mut copies, &mut local).unwrap();
        assert_eq!(local.editor, Some(EditorTarget::Draft("draft".into())));
        assert!(effect_rx.try_recv().is_err());
    }

    #[test]
    fn a_failed_editor_run_keeps_its_target() {
        use crate::ui::MemoryTarget;
        let path = std::path::PathBuf::from("/c/SYSTEM.md");
        assert_eq!(
            editor_failed(
                EditorTarget::Memory {
                    target: MemoryTarget::System,
                    path: path.clone()
                },
                "no".into()
            ),
            EditorClosed::Memory {
                target: MemoryTarget::System,
                path,
                result: Err("no".into())
            }
        );
        assert_eq!(
            editor_failed(EditorTarget::Draft("x".into()), "no".into()),
            EditorClosed::Draft(Err("no".into()))
        );
    }

    #[test]
    fn translate_filters_release_events() {
        let mut key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(matches!(
            translate(Event::Key(key), false),
            Some(Msg::Key(_))
        ));
        key.kind = KeyEventKind::Release;
        key.state = KeyEventState::NONE;
        assert!(translate(Event::Key(key), false).is_none());
        assert!(matches!(
            translate(Event::Paste("x".into()), false),
            Some(Msg::Paste(_))
        ));
        assert!(matches!(
            translate(Event::Resize(1, 2), false),
            Some(Msg::Resize)
        ));
        assert!(matches!(
            translate(Event::FocusGained, false),
            Some(Msg::Focus(true))
        ));
        assert!(matches!(
            translate(Event::FocusLost, false),
            Some(Msg::Focus(false))
        ));
    }

    #[test]
    fn the_bell_is_one_bel_byte() {
        let mut out = Vec::new();
        ring_bell(&mut out).unwrap();
        assert_eq!(out, b"\x07");
    }
}
