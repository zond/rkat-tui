//! The UI's unit tests, by concern (one file per `App` concern), over
//! the shared helpers below.

use super::transcript::test_support::{test_args, test_tool_result, test_usage};
use super::*;
use meerkat_core::{StopReason, Usage};
use ratatui::crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Rect;
use serde_json::json;

mod dispatch;
mod escalations;
mod helpers;
mod keys;
mod mouse;
mod shell;
mod turns;

fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(c: char) -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn agent(ev: AgentEvent) -> Msg {
    Msg::Agent(EventEnvelope::new("s", 0, None, ev))
}

fn type_str(app: &mut App, s: &str) {
    for c in s.chars() {
        app.update(key(KeyCode::Char(c)));
    }
}

fn summary() -> TurnSummary {
    TurnSummary {
        text: String::new(),
        input_tokens: 0,
        output_tokens: 0,
    }
}

/// What the renderer does: lay out at `width` and record the rows of a
/// transcript drawn from screen row `top` with `height` rows.
fn draw_rows(app: &mut App, top: u16, width: u16, height: usize) {
    let total = app.transcript.ensure_layout(width);
    let start = app.scroll.window(total, height);
    let rows = app.transcript.visible_blocks(start, height);
    let area = Rect::new(0, top, width, u16::try_from(height).unwrap());
    app.set_transcript_rows(area, start, rows);
}

fn mouse(kind: MouseEventKind, row: u16, column: u16) -> Msg {
    Msg::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn down(row: u16, column: u16) -> Msg {
    mouse(MouseEventKind::Down(MouseButton::Left), row, column)
}

fn drag(row: u16, column: u16) -> Msg {
    mouse(MouseEventKind::Drag(MouseButton::Left), row, column)
}

fn up(row: u16, column: u16) -> Msg {
    mouse(MouseEventKind::Up(MouseButton::Left), row, column)
}

/// A left click (press and release without moving) at column 3.
fn click_at(app: &mut App, row: u16) {
    app.update(down(row, 3));
    app.update(up(row, 3));
}

fn tool_collapsed(app: &App, id: BlockId) -> bool {
    match app.transcript.block(id) {
        Some(Block::Tool { collapsed, .. }) => *collapsed,
        other => panic!("not a tool block: {other:?}"),
    }
}

/// `› hello world` / spacer / `› second line here` / spacer, drawn from
/// screen row 1 at width 60.
fn selection_app() -> App {
    let mut app = App::new(StatusInfo::default());
    app.transcript.push(user("hello world"));
    app.transcript.push(user("second line here"));
    draw_rows(&mut app, 1, 60, 20);
    app
}

fn agent_at(ev: AgentEvent, seq: u64) -> Msg {
    Msg::Agent(EventEnvelope::new("s", seq, None, ev))
}

fn submit(app: &mut App, s: &str) -> Vec<Effect> {
    type_str(app, s);
    app.update(key(KeyCode::Enter))
}

fn finished(result: Result<TurnSummary, TurnFailure>) -> Msg {
    Msg::TurnFinished(TurnOutcome { result })
}

fn ack(id: u64, result: Result<(), SteerFailure>) -> Msg {
    Msg::SteerAck {
        id: PendingId(id),
        result,
    }
}

fn queued_block(id: u64, text: &str, label: QueuedLabel) -> Block {
    Block::Queued {
        id: PendingId(id),
        text: text.into(),
        label,
    }
}

fn user(text: &str) -> Block {
    Block::User { text: text.into() }
}

/// A running app with "one" as the turn prompt.
fn running_app() -> App {
    let mut app = App::new(StatusInfo::default());
    assert_eq!(
        submit(&mut app, "one"),
        vec![Effect::StartTurn("one".into())]
    );
    app
}

fn steer(id: u64, text: &str) -> Effect {
    Effect::Steer {
        id: PendingId(id),
        text: text.into(),
    }
}

fn shell_done(id: u64, output: &str) -> Msg {
    Msg::ShellFinished {
        id: ShellRunId(id),
        outcome: ShellOutcome::Ran {
            run: crate::user_shell::ShellRun {
                output: output.into(),
                exit: crate::user_shell::ShellExit::Code(0),
                duration: Duration::from_millis(5),
            },
            context: Ok(()),
        },
    }
}

fn run_shell(id: u64, command: &str) -> Effect {
    Effect::RunShell {
        id: ShellRunId(id),
        command: command.into(),
    }
}

fn menu_rows(app: &App) -> Vec<String> {
    app.menu()
        .map(|menu| menu.items.iter().map(|item| item.insert.clone()).collect())
        .unwrap_or_default()
}

fn mention_app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    std::fs::create_dir_all(p.join("src/ui")).unwrap();
    std::fs::create_dir_all(p.join(".rkat")).unwrap();
    std::fs::write(p.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(p.join("src/ui/render.rs"), "").unwrap();
    std::fs::write(p.join("README.md"), "# r\n").unwrap();
    std::fs::write(p.join(".rkat/config.toml"), "").unwrap();
    let app = App::new(StatusInfo::default()).with_workspace(ProjectRoot::new(p).unwrap());
    (dir, app)
}

/// Answer the app's path-listing request as the UI loop does (the
/// walk runs off the UI thread there).
fn answer_index(app: &mut App) {
    for effect in app.take_local() {
        if let LocalEffect::IndexPaths { generation, root } = effect {
            app.update(Msg::PathIndex {
                generation,
                paths: crate::mentions::workspace_paths(&root),
            });
        }
    }
}

fn reasoning_collapsed(app: &App) -> bool {
    app.transcript.blocks().iter().rev().any(|b| {
        matches!(
            b,
            Block::Reasoning {
                collapsed: true,
                ..
            }
        )
    })
}

fn picker_models() -> Vec<ModelGroup> {
    picker_models_warning(None)
}

fn picker_models_warning(warning: Option<&str>) -> Vec<ModelGroup> {
    use crate::models::{ModelEntry, ModelSource};
    let entry = |id: &str, source| ModelEntry {
        id: id.into(),
        display_name: None,
        source,
        context_window: None,
        max_output_tokens: None,
        capabilities: Default::default(),
    };
    vec![ModelGroup {
        endpoint: "anthropic".into(),
        dialect: crate::endpoints::WireDialect::Anthropic,
        base_url: None,
        models: vec![
            entry("claude-sonnet-4-6", ModelSource::Current),
            entry("claude-opus-4-6", ModelSource::Provider),
            entry("claude-haiku-4-5", ModelSource::Provider),
        ],
        warning: warning.map(Into::into),
    }]
}

fn app_on_sonnet() -> App {
    let mut status = StatusInfo::default();
    status.apply(StatusUpdate::Model {
        endpoint: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
    });
    App::new(status)
}

fn shell_prompt() -> RecordedPrompt {
    RecordedPrompt {
        agent: None,
        tool: "shell".into(),
        args_preview: "{}".into(),
        ..RecordedPrompt::default()
    }
}

/// A model picker with a filter typed and the selection moved.
fn app_with_moved_model_picker() -> (App, Modal) {
    let mut app = app_on_sonnet();
    app.update(Msg::ShowModelPicker(picker_models()));
    type_str(&mut app, "claude");
    app.update(key(KeyCode::Down));
    let picker = app.modal.clone().expect("picker open");
    (app, picker)
}

fn answered(text: &str) -> Msg {
    Msg::TurnFinished(TurnOutcome {
        result: Ok(TurnSummary {
            text: text.into(),
            input_tokens: 1,
            output_tokens: 1,
        }),
    })
}

/// The loop's own requests among [`App::take_local`] (frames, bells,
/// history lines and copies left out).
fn loop_requests(app: &mut App) -> Vec<LocalEffect> {
    app.take_local()
        .into_iter()
        .filter(|l| {
            matches!(
                l,
                LocalEffect::ExternalEditor(_) | LocalEffect::Suspend | LocalEffect::Redraw
            )
        })
        .collect()
}

fn last_notice(app: &App) -> Option<(NoticeLevel, String)> {
    app.transcript.blocks().iter().rev().find_map(|b| match b {
        Block::Notice { level, text } => Some((*level, text.clone())),
        _ => None,
    })
}

fn gauge_app(mode: CompactMode) -> App {
    let mut app = App::new(StatusInfo::default()).with_compaction_policy(CompactionPolicy {
        mode,
        ..CompactionPolicy::default()
    });
    app.update(Msg::Status(StatusUpdate::ContextWindow(Some(100_000))));
    app
}

/// Run one turn that ends with `used` context tokens.
fn turn_at(app: &mut App, used: u64) -> Vec<Effect> {
    assert_eq!(submit(app, "go"), vec![Effect::StartTurn("go".into())]);
    app.update(agent(AgentEvent::TurnCompleted {
        stop_reason: StopReason::EndTurn,
        usage: test_usage(Usage {
            input_tokens: used,
            ..Usage::default()
        }),
        assistant_message_id: None,
    }));
    app.update(finished(Ok(summary())))
}

fn warn_count(app: &App) -> usize {
    app.transcript
        .blocks()
        .iter()
        .filter(|b| {
            matches!(
                b,
                Block::Notice {
                    level: NoticeLevel::Warn,
                    ..
                }
            )
        })
        .count()
}

fn priced(entries: &[(&str, f64, f64)]) -> Arc<crate::pricing::PriceTable> {
    use crate::config::PricingSection;
    Arc::new(crate::pricing::PriceTable::new(
        entries
            .iter()
            .map(|(id, input, output)| {
                (
                    (*id).to_string(),
                    PricingSection {
                        input: Some(*input),
                        output: Some(*output),
                        ..PricingSection::default()
                    },
                )
            })
            .collect(),
    ))
}

/// A turn whose usage the provider accounted to `model`.
fn completed_by(model: &str, input: u64, output: u64) -> Msg {
    agent(AgentEvent::TurnCompleted {
        stop_reason: StopReason::EndTurn,
        usage: Some(meerkat_core::TurnUsage::host_declared(
            meerkat_core::Provider::Anthropic,
            model,
            Usage {
                input_tokens: input,
                output_tokens: output,
                ..Usage::default()
            },
        )),
        assistant_message_id: None,
    })
}

fn footer(app: &App) -> String {
    render::status_line(&app.status, Instant::now())
}

fn helper_msg(label: &str, event: AgentEvent) -> Msg {
    Msg::Scoped(
        crate::mobs::HelperTag {
            owner: meerkat_core::SessionId::new(),
            path: vec![label.to_string()],
            model: "m".into(),
        }
        .scope(event),
    )
}

/// An app with the helpers `scout` then `fork-1`, one call each.
fn helper_app() -> (App, BlockId, BlockId) {
    let mut app = App::new(StatusInfo::default());
    app.transcript.push(user("go"));
    let mut live = Vec::new();
    for label in ["scout", "fork-1"] {
        live.push(crate::mobs::HelperStatus {
            label: label.into(),
            model: "m".into(),
            depth: 1,
        });
        app.update(Msg::Status(StatusUpdate::Helpers(live.clone())));
        app.update(helper_msg(
            label,
            AgentEvent::ToolCallRequested {
                id: "a".into(),
                name: "glob".into(),
                args: test_args(json!({"pattern": "*"})),
            },
        ));
    }
    let scout = app.transcript.helper_header("scout").unwrap();
    let fork = app.transcript.helper_header("fork-1").unwrap();
    (app, scout, fork)
}

fn ctrl_key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
}

fn expanded(app: &App, id: BlockId) -> bool {
    match app.transcript.block(id) {
        Some(Block::Helper(fold)) => fold.expanded,
        other => panic!("not a helper header: {other:?}"),
    }
}

/// The live update listing exactly `labels` (top-level helpers).
fn live(labels: &[&str]) -> Msg {
    Msg::Status(StatusUpdate::Helpers(
        labels
            .iter()
            .map(|label| crate::mobs::HelperStatus {
                label: (*label).into(),
                model: "m".into(),
                depth: 1,
            })
            .collect(),
    ))
}

/// Draw the whole app into an `80 x height` test terminal and return
/// its rows (trailing blanks trimmed).
fn screen(app: &mut App, height: u16) -> Vec<String> {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, height)).unwrap();
    terminal
        .draw(|f| render::draw_at(f, app, Instant::now()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..80)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// A tool call of the helper `fork-1` with id `id`.
fn fork_call(id: &str) -> Msg {
    helper_msg(
        "fork-1",
        AgentEvent::ToolCallRequested {
            id: id.into(),
            name: "read_file".into(),
            args: test_args(json!({"path": id})),
        },
    )
}
