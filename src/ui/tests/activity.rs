//! The prompt marker and the status bar's activity segment: idle, busy
//! and heard from (the spinner), busy and silent (the waiting form, with
//! what is waited for).

use super::*;
use crate::ui::render::{LONG_QUIET, QUIET_AFTER, SPINNER};
use ratatui::style::Color;

const W: u16 = 80;
const H: u16 = 12;

/// The screen at `now` as rows, and the input marker's cell (symbol and
/// colour).
fn draw(app: &mut App, width: u16, now: Instant) -> (Vec<String>, String, Option<Color>) {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, H)).unwrap();
    terminal.draw(|f| render::draw_at(f, app, now)).unwrap();
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..H)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    let cell = &buffer[(0, input_y(&rows))];
    (rows, cell.symbol().to_string(), cell.fg.into())
}

/// The input's first row: the last one above the status bar that starts
/// with a prompt marker (the transcript's user blocks are above it, the
/// helper panel and the checklist under it are indented).
fn input_y(rows: &[String]) -> u16 {
    let marker = |r: &String| {
        r.chars()
            .next()
            .is_some_and(|c| c == '›' || is_waiting(&c.to_string()) || SPINNER.contains(&c))
    };
    let y = rows[..rows.len() - 1].iter().rposition(marker).unwrap();
    u16::try_from(y).unwrap()
}

fn input_row(rows: &[String]) -> &str {
    &rows[usize::from(input_y(rows))]
}

fn status_row(rows: &[String]) -> &str {
    &rows[usize::from(H - 1)]
}

fn secs(t0: Instant, s: u64) -> Instant {
    t0 + Duration::from_secs(s)
}

/// Start a turn of this client at `t0` and type `draft` after it.
fn start(app: &mut App, t0: Instant, draft: &str) {
    for c in "go".chars() {
        app.update_at(key(KeyCode::Char(c)), t0);
    }
    app.update_at(key(KeyCode::Enter), t0);
    for c in draft.chars() {
        app.update_at(key(KeyCode::Char(c)), t0);
    }
}

fn is_spinner(glyph: &str) -> bool {
    glyph.chars().count() == 1 && glyph.chars().all(|c| SPINNER.contains(&c))
}

fn is_waiting(glyph: &str) -> bool {
    glyph == "•" || glyph == "·"
}

#[test]
fn the_idle_marker_is_the_plain_prompt() {
    let t0 = Instant::now();
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "abc");
    let (rows, marker, fg) = draw(&mut app, W, t0);
    assert_eq!(marker, "›");
    assert_eq!(fg, Some(Color::Cyan));
    assert!(input_row(&rows).starts_with("› abc"), "{rows:#?}");
    assert!(!status_row(&rows).contains("running"));
}

#[test]
fn the_busy_marker_spins_and_the_text_stays_put() {
    let t0 = Instant::now();
    let mut app = App::new(StatusInfo::default());
    start(&mut app, t0, "abc");
    let (a_rows, a, a_fg) = draw(&mut app, W, t0);
    let later = t0 + Duration::from_millis(u64::try_from(render::SPINNER_FRAME_MS).unwrap());
    let (b_rows, b, _) = draw(&mut app, W, later);
    assert!(is_spinner(&a) && is_spinner(&b), "{a:?} {b:?}");
    assert_ne!(a, b, "two instants, two frames");
    assert_eq!(a_fg, Some(Color::Cyan));
    for rows in [&a_rows, &b_rows] {
        assert_eq!(
            input_row(rows).chars().skip(2).take(3).collect::<String>(),
            "abc"
        );
    }
    assert!(
        status_row(&a_rows).starts_with(" ⠋ running 0s │"),
        "{a_rows:#?}"
    );
    // It stops when the turn ends, and the text is where it was.
    app.update_at(finished(Ok(summary())), secs(t0, 1));
    let (rows, marker, _) = draw(&mut app, W, secs(t0, 1));
    assert_eq!(marker, "›");
    assert!(input_row(&rows).starts_with("› abc"));
}

#[test]
fn a_quiet_model_request_says_it_waits_for_the_reply() {
    let t0 = Instant::now();
    let mut app = App::new(StatusInfo::default());
    start(&mut app, t0, "");
    app.update_at(
        agent(AgentEvent::TurnStarted {
            turn_number: 0,
            assistant_message_id: None,
        }),
        secs(t0, 1),
    );
    // Still within the quiet period: the spinner.
    let (_, marker, _) = draw(&mut app, W, secs(t0, 0) + QUIET_AFTER);
    assert!(is_spinner(&marker), "{marker:?}");
    let (rows, marker, fg) = draw(&mut app, W, secs(t0, 35));
    assert!(is_waiting(&marker), "{marker:?}");
    assert_eq!(fg, Some(Color::Yellow));
    let status = status_row(&rows);
    assert!(
        status.contains("waiting for the model's reply 34s"),
        "{status}"
    );
    assert!(!status.contains("Esc interrupts"), "{status}");
    assert!(!status.contains("running"), "{status}");
    // The pulse alternates.
    let (_, other, _) = draw(&mut app, W, secs(t0, 35) + Duration::from_millis(800));
    assert!(
        is_waiting(&other) && other != marker,
        "{marker:?} {other:?}"
    );
}

#[test]
fn a_quiet_tool_call_names_the_tool_and_a_long_silence_names_esc() {
    let t0 = Instant::now();
    let mut app = App::new(StatusInfo::default());
    start(&mut app, t0, "");
    app.update_at(
        agent(AgentEvent::ToolCallRequested {
            id: "t1".into(),
            name: "shell".into(),
            args: test_args(json!({"command": "cargo build"})),
        }),
        secs(t0, 1),
    );
    let (rows, marker, _) = draw(&mut app, W, secs(t0, 131));
    assert!(is_waiting(&marker));
    let status = status_row(&rows);
    assert!(
        status.contains("tool shell running 2m 10s · Esc interrupts"),
        "{status}"
    );
    assert!(LONG_QUIET <= Duration::from_secs(130));
    // Its completion closes it: the next silence is the model's.
    app.update_at(
        agent(AgentEvent::ToolExecutionCompleted {
            id: "t1".into(),
            name: "shell".into(),
            is_error: false,
            duration_ms: 1,
        }),
        secs(t0, 140),
    );
    let (rows, _, _) = draw(&mut app, W, secs(t0, 170));
    assert!(
        status_row(&rows).contains("no word from the agent for 30s"),
        "{rows:#?}"
    );
}

#[test]
fn a_quiet_classifier_review_and_compaction_are_named() {
    let t0 = Instant::now();
    let mut app = App::new(StatusInfo::default());
    start(&mut app, t0, "");
    app.update_at(Msg::Status(StatusUpdate::Classifying(true)), secs(t0, 2));
    let (rows, marker, _) = draw(&mut app, W, secs(t0, 30));
    assert!(is_waiting(&marker));
    assert!(
        status_row(&rows).contains("review by the classifier 28s"),
        "{rows:#?}"
    );
    app.update_at(Msg::Status(StatusUpdate::Classifying(false)), secs(t0, 31));
    app.update_at(
        agent(AgentEvent::CompactionStarted {
            input_tokens: 40_000,
            estimated_history_tokens: 40_000,
            message_count: 12,
        }),
        secs(t0, 32),
    );
    let (rows, _, _) = draw(&mut app, W, secs(t0, 40));
    assert!(
        status_row(&rows).starts_with(" ⠋ compacting… 8s │"),
        "{rows:#?}"
    );
    let (rows, marker, _) = draw(&mut app, W, secs(t0, 72));
    assert!(is_waiting(&marker));
    assert!(status_row(&rows).contains("compacting 40s"), "{rows:#?}");
}

#[test]
fn a_run_the_client_did_not_start_animates_and_says_when_it_last_spoke() {
    let t0 = Instant::now();
    let mut app = App::new(StatusInfo::default());
    app.update_at(Msg::RemoteTurn { prompt: None }, t0);
    let (_, a, _) = draw(&mut app, W, t0);
    let (_, b, _) = draw(&mut app, W, t0 + Duration::from_millis(80));
    assert!(is_spinner(&a) && is_spinner(&b) && a != b, "{a:?} {b:?}");
    let (rows, marker, _) = draw(&mut app, W, secs(t0, 45));
    assert!(is_waiting(&marker));
    let status = status_row(&rows);
    assert!(
        status.contains("no word from the agent for 45s"),
        "{status}"
    );
    assert!(!status.contains("Esc"), "{status}");
    let (rows, _, _) = draw(&mut app, W, secs(t0, 125));
    assert!(
        status_row(&rows).contains("no word from the agent for 2m 5s · Esc interrupts"),
        "{rows:#?}"
    );
    // A helper's event is word too: the spinner is back.
    app.update_at(
        helper_msg(
            "fork-1",
            AgentEvent::TextDelta {
                delta: "x".into(),
                assistant_message_id: None,
            },
        ),
        secs(t0, 126),
    );
    let (rows, marker, _) = draw(&mut app, W, secs(t0, 127));
    assert!(is_spinner(&marker), "{marker:?}");
    assert!(status_row(&rows).contains("running 127s"), "{rows:#?}");
}

#[test]
fn events_resuming_restore_the_spinner() {
    let t0 = Instant::now();
    let mut app = App::new(StatusInfo::default());
    start(&mut app, t0, "");
    let (_, marker, _) = draw(&mut app, W, secs(t0, 50));
    assert!(is_waiting(&marker));
    app.update_at(
        agent(AgentEvent::TextDelta {
            delta: "hi".into(),
            assistant_message_id: None,
        }),
        secs(t0, 50),
    );
    let (rows, marker, fg) = draw(&mut app, W, secs(t0, 51));
    assert!(is_spinner(&marker), "{marker:?}");
    assert_eq!(fg, Some(Color::Cyan));
    assert!(status_row(&rows).contains("running 51s"), "{rows:#?}");
    assert!(!status_row(&rows).contains("no word"), "{rows:#?}");
}

#[test]
fn the_activity_segment_survives_a_narrow_terminal() {
    let t0 = Instant::now();
    let mut status = StatusInfo::default();
    status.apply(StatusUpdate::Model {
        endpoint: "anthropic".into(),
        model: "claude-sonnet-4-6".into(),
    });
    status.apply(StatusUpdate::Session("abc12345-ffff".into()));
    let mut app = App::new(status);
    start(&mut app, t0, "");
    let (rows, _, _) = draw(&mut app, 20, secs(t0, 3));
    let status = status_row(&rows);
    assert!(status.starts_with(' '), "{status}");
    assert!(
        is_spinner(&status.chars().nth(1).unwrap().to_string()),
        "{status}"
    );
    assert!(
        status
            .chars()
            .skip(2)
            .collect::<String>()
            .starts_with(" running 3s"),
        "{status}"
    );
    let (rows, _, _) = draw(&mut app, 40, secs(t0, 60));
    assert!(
        status_row(&rows).contains("no word from the agent"),
        "{rows:#?}"
    );
}
