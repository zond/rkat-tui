//! The helper panel and a helper's full view.

use super::*;

/// The owner's rule of 2026-11-18: the panel lists what is live, running
/// helpers first (newest first), then the seated idle ones; one that ended
/// stays dim with its ending word for `PANEL_LINGER`, then leaves, with
/// no turn boundary; its header stays in the transcript.
#[test]
fn the_panel_lists_live_helpers_and_an_ended_one_leaves_after_its_linger() {
    let t0 = Instant::now();
    let (mut app, scout, fork) = helper_app();
    // The agent's turn runs all along: no turn starts or ends below.
    type_str(&mut app, "go on");
    app.update_at(key(KeyCode::Enter), t0);
    assert!(app.is_running());
    app.update_at(
        Msg::SeatedHelpers(vec!["scout".into(), "fork-1".into()]),
        t0,
    );
    assert_eq!(app.helper_panel(), vec![fork, scout]);
    assert!(app.status.helper_panel);
    // scout goes idle, still seated: listed after the running one.
    app.update_at(live(&["fork-1"]), t0);
    assert_eq!(app.helper_panel(), vec![fork, scout]);
    // fork-1 finishes and leaves its mob: dim with its ending, after the
    // live one, for its linger.
    app.update_at(
        Msg::HelperEnded {
            path: vec!["fork-1".into()],
            how: "finished".into(),
            told: Some("main-ab12 was told".into()),
        },
        t0,
    );
    app.update_at(live(&[]), t0);
    app.update_at(Msg::SeatedHelpers(vec!["scout".into()]), t0);
    assert_eq!(app.helper_panel(), vec![scout, fork]);
    assert!(
        app.transcript.helpers_lingering(),
        "the UI ticks until it leaves"
    );
    let rows = screen(&mut app, 12);
    let ended = rows
        .iter()
        .find(|r| r.contains("[fork-1 · m]"))
        .unwrap_or_else(|| panic!("{rows:#?}"));
    assert!(ended.contains("finished"), "{ended}");
    app.update_at(Msg::Tick, t0 + transcript::PANEL_LINGER / 2);
    assert_eq!(app.helper_panel(), vec![scout, fork]);
    app.update_at(Msg::Tick, t0 + transcript::PANEL_LINGER);
    assert_eq!(app.helper_panel(), vec![scout]);
    assert!(!app.transcript.helpers_lingering());
    assert_eq!(app.transcript.helper_header("fork-1"), Some(fork));
    assert!(app.is_running(), "no turn boundary was needed");
    // scout retired: it leaves too, and the panel goes.
    app.update_at(
        Msg::SeatedHelpers(Vec::new()),
        t0 + transcript::PANEL_LINGER,
    );
    app.update_at(Msg::Tick, t0 + transcript::PANEL_LINGER * 3);
    assert!(app.helper_panel().is_empty());
    assert!(!app.status.helper_panel);
}

/// The owner's 0.0.16 report: in one long turn twenty helpers, of which
/// three are live (seated) now: the panel lists the three, with no
/// "more" line, once the ended ones' linger is over.
#[test]
fn of_twenty_helpers_in_one_long_turn_the_panel_lists_the_three_live() {
    let t0 = Instant::now();
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "do it all");
    app.update_at(key(KeyCode::Enter), t0);
    let labels: Vec<String> = (0..20).map(|i| format!("h{i}")).collect();
    let names: Vec<&str> = labels.iter().map(String::as_str).collect();
    app.update_at(Msg::SeatedHelpers(labels.clone()), t0);
    app.update_at(live(&names), t0);
    assert_eq!(app.helper_panel().len(), 20);
    // Seventeen end; three stay seated (idle).
    for label in &labels[3..] {
        app.update_at(
            Msg::HelperEnded {
                path: vec![label.clone()],
                how: "finished".into(),
                told: Some("main-ab12 was told".into()),
            },
            t0,
        );
    }
    app.update_at(live(&[]), t0);
    app.update_at(Msg::SeatedHelpers(labels[..3].to_vec()), t0);
    app.update_at(Msg::Tick, t0 + transcript::PANEL_LINGER);
    let panel: Vec<String> = app
        .helper_panel()
        .into_iter()
        .filter_map(|id| match app.transcript.block(id) {
            Some(Block::Helper(fold)) => Some(fold.label.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(panel, ["h2", "h1", "h0"]);
    let rows = screen(&mut app, 16);
    assert!(!rows.iter().any(|r| r.contains("more")), "{rows:#?}");
    assert_eq!(app.transcript.helper_headers().count(), 20);
    assert!(app.is_running(), "all in one turn");
}

/// A selected row does not leave while selected, ended or not; when a
/// row's header goes, the selection moves to its neighbour, not the top.
#[test]
fn a_selected_ended_helper_stays_and_a_gone_row_hands_the_selection_to_its_neighbour() {
    let t0 = Instant::now();
    let later = t0 + transcript::PANEL_LINGER * 2;
    let mut app = App::new(StatusInfo::default());
    let labels = ["a", "b", "c", "d"];
    app.update_at(
        Msg::SeatedHelpers(labels.iter().map(ToString::to_string).collect()),
        t0,
    );
    app.update_at(live(&labels), t0);
    let [a, b, c, d] = labels.map(|l| app.transcript.helper_header(l).unwrap());
    assert_eq!(app.helper_panel(), vec![d, c, b, a]);
    // Select b, then b ends: it stays while selected, past its linger.
    for _ in 0..3 {
        app.update_at(ctrl_key(KeyCode::Down), t0);
    }
    assert_eq!(app.panel_selection(), Some(b));
    app.update_at(live(&["a", "c", "d"]), t0);
    app.update_at(
        Msg::SeatedHelpers(vec!["a".into(), "c".into(), "d".into()]),
        t0,
    );
    app.update_at(Msg::Tick, later);
    assert_eq!(app.panel_selection(), Some(b));
    assert_eq!(app.helper_panel(), vec![d, c, a, b]);
    // Moving off it lets it go.
    app.update_at(ctrl_key(KeyCode::Up), later);
    assert_eq!(app.panel_selection(), Some(a));
    assert_eq!(app.helper_panel(), vec![d, c, a]);
    // The selected row's header goes: the selection moves to the row
    // after it, not to the top.
    app.update_at(ctrl_key(KeyCode::Up), later);
    assert_eq!(app.panel_selection(), Some(c));
    assert!(app.transcript.remove_block(c).is_some());
    app.update_at(Msg::Tick, later);
    assert_eq!(app.helper_panel(), vec![d, a]);
    assert_eq!(app.panel_selection(), Some(a));
}

/// Past six helpers the last line says what the window hides: how many,
/// how many of them run, and how many are above it.
#[test]
fn the_panel_s_overflow_line_counts_what_it_hides() {
    assert_eq!(render::panel_more(0, 3, 0), "  … and 3 more");
    assert_eq!(render::panel_more(0, 3, 2), "  … and 3 more (2 running)");
    assert_eq!(
        render::panel_more(2, 3, 1),
        "  … and 5 more (1 running) · 2 above · 3 below"
    );
    assert_eq!(render::panel_more(3, 0, 0), "  … and 3 more · 3 above");
}

#[test]
fn a_nested_helper_is_listed_under_its_parent() {
    let (mut app, scout, fork) = helper_app();
    app.update(Msg::Scoped(
        crate::mobs::HelperTag {
            owner: meerkat_core::SessionId::new(),
            path: vec!["scout".into(), "inner".into()],
            model: "m".into(),
            kind: crate::mobs::HelperKind::Other,
        }
        .scope(AgentEvent::ToolCallRequested {
            id: "b".into(),
            name: "grep".into(),
            args: test_args(json!({})),
        }),
    ));
    let inner = app.transcript.helper_header("inner").unwrap();
    assert_eq!(app.helper_panel(), vec![fork, scout, inner]);
    let rows = screen(&mut app, 12);
    // Under the input (its border, one line), above the status bar.
    assert_eq!(
        rows[8],
        "  [fork-1 · m] ⠋ running · 1 tool call · glob \"*\""
    );
    assert_eq!(
        rows[9],
        "  [scout · m] ⠋ running · 1 tool call · glob \"*\""
    );
    assert_eq!(rows[10], "    [inner] ⠋ running · 1 tool call · grep");
}

#[test]
fn the_panel_shows_at_most_six_lines() {
    let mut app = App::new(StatusInfo::default());
    let labels: Vec<String> = (0..8).map(|i| format!("h{i}")).collect();
    let names: Vec<&str> = labels.iter().map(String::as_str).collect();
    app.update(live(&names));
    assert_eq!(app.helper_panel().len(), 8);
    let rows = screen(&mut app, 16);
    // Five helpers (newest first), then the rest counted.
    assert!(rows[9].starts_with("  [h7 · m]"), "{rows:?}");
    assert!(rows[13].starts_with("  [h3 · m]"), "{rows:?}");
    assert_eq!(rows[14], "  … and 3 more (3 running)");
    // Selecting past the window moves it (the first press selects the
    // newest, seven more the oldest).
    for _ in 0..8 {
        app.update(ctrl_key(KeyCode::Down));
    }
    let rows = screen(&mut app, 16);
    assert!(rows[13].starts_with("  [h0 · m]"), "{rows:?}");
    assert_eq!(rows[14], "  … and 3 more (3 running) · 3 above");
    assert_eq!(render::panel_window(8, Some(7)), (3, 5, 3));
    assert_eq!(render::panel_window(6, Some(5)), (0, 6, 0));
}

#[test]
fn ctrl_down_selects_the_newest_helper_and_ctrl_up_down_wrap() {
    let (mut app, scout, fork) = helper_app();
    assert_eq!(app.panel_selection(), None);
    app.update(ctrl_key(KeyCode::Down));
    assert_eq!(app.panel_selection(), Some(fork));
    // Down past the last (oldest) wraps to the first, up past the first
    // to the last.
    app.update(ctrl_key(KeyCode::Down));
    assert_eq!(app.panel_selection(), Some(scout));
    app.update(ctrl_key(KeyCode::Down));
    assert_eq!(app.panel_selection(), Some(fork));
    app.update(ctrl_key(KeyCode::Up));
    assert_eq!(app.panel_selection(), Some(scout));
    // The first Ctrl+Up also starts at the newest.
    let (mut app, _, fork) = helper_app();
    app.update(ctrl_key(KeyCode::Up));
    assert_eq!(app.panel_selection(), Some(fork));
    // Typing keeps the selection.
    type_str(&mut app, "x");
    assert_eq!(app.panel_selection(), Some(fork));
    // No helpers: nothing to select.
    let mut app = App::new(StatusInfo::default());
    app.update(ctrl_key(KeyCode::Down));
    assert_eq!(app.panel_selection(), None);
    assert!(!app.status.helper_panel);
}

#[test]
fn enter_with_an_empty_input_opens_the_full_view_without_submitting() {
    let (mut app, scout, fork) = helper_app();
    app.update(ctrl_key(KeyCode::Down));
    let fx = app.update(key(KeyCode::Enter));
    assert!(fx.is_empty(), "{fx:?}");
    assert!(!app.is_running());
    assert_eq!(app.helper_view(), Some(fork));
    // The inline header is not expanded by the view.
    assert!(!expanded(&app, fork));
    // Another selection switches the view; Enter on the open one
    // closes it.
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    assert_eq!(app.helper_view(), Some(scout));
    app.update(key(KeyCode::Enter));
    assert_eq!(app.helper_view(), None);
}

#[test]
fn enter_with_text_submits_despite_a_selection() {
    let (mut app, _, fork) = helper_app();
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    assert_eq!(app.helper_view(), Some(fork));
    // The editor keeps working while the view is open.
    type_str(&mut app, "draft");
    let fx = app.update(key(KeyCode::Enter));
    assert_eq!(fx, vec![Effect::StartTurn("draft".into())]);
    assert!(app.input.is_empty());
    // The view and the selection stay.
    assert_eq!(app.helper_view(), Some(fork));
    assert_eq!(app.panel_selection(), Some(fork));
}

/// The owner's rule of 2026-11-17, replacing the 0.0.8 one
/// (`esc_in_a_running_helper_s_view_asks_to_stop_it`: Esc asked whether to
/// stop a running helper): Esc in a helper's view, running or not, goes
/// back to the transcript at once, with no question, without touching
/// the agent's run (no interrupt) and without arming Esc-Esc; the next Esc
/// drops the selection. A pending `/btw` takes the first Esc. The view's
/// title says the keys.
#[test]
fn esc_in_a_helper_s_view_goes_back_at_once() {
    let (mut app, _, fork) = helper_app();
    // The agent's turn runs: an Esc that reached it would interrupt it.
    app.running = true;
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    assert_eq!(app.helper_view(), Some(fork));
    let rows = screen(&mut app, 14);
    assert!(rows[0].ends_with("Esc back · Ctrl-C stop"), "{rows:#?}");
    assert!(app.update(key(KeyCode::Esc)).is_empty(), "no interrupt");
    assert_eq!(app.helper_view(), None);
    assert_eq!(app.stop_confirmation(), None);
    assert!(app.last_esc.is_none(), "Esc-Esc is not armed");
    assert_eq!(app.panel_selection(), Some(fork));
    // The next Esc drops the selection (and the one after that is the
    // agent's own).
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert_eq!(app.panel_selection(), None);
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::Interrupt]);
    // A finished helper's view: the title says only Esc.
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    app.update(live(&[]));
    let rows = screen(&mut app, 14);
    assert!(rows[0].ends_with("Esc back"), "{rows:#?}");
    assert!(!rows[0].contains("Ctrl-C"), "{rows:#?}");
    app.update(key(KeyCode::Esc));
    assert_eq!(app.helper_view(), None);
    // A pending `/btw` takes the first Esc; the next one goes back.
    app.update(key(KeyCode::Enter));
    assert_eq!(app.helper_view(), Some(fork));
    submit(&mut app, "/btw how far is it?");
    assert!(app.btw_pending());
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::CancelBtw]);
    assert_eq!(app.helper_view(), Some(fork));
    app.update(key(KeyCode::Esc));
    assert_eq!(app.helper_view(), None);
}

/// Ctrl-C in a running helper's view asks whether to stop it: the
/// question is drawn under the view, other keys leave it up, `n`, Esc or a
/// second Ctrl-C keep the helper running and stay in the view, `y` stops
/// it (and the view closes). Ctrl-C never reaches the agent's interrupt
/// or the quit ladder; in a finished helper's view it only says the
/// helper is not running.
#[test]
fn ctrl_c_in_a_running_helper_s_view_asks_to_stop_it() {
    let (mut app, _, fork) = helper_app();
    app.running = true;
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    assert_eq!(app.helper_view(), Some(fork));
    assert!(app.update(ctrl('c')).is_empty(), "no interrupt");
    assert_eq!(app.helper_view(), Some(fork));
    let question = app.stop_confirmation().unwrap();
    assert!(
        question.starts_with("Stop helper fork-1? It cannot be undone")
            && question.contains("the helpers it started keep running"),
        "{question}"
    );
    let rows = screen(&mut app, 14).join("\n");
    assert!(rows.contains("Stop helper fork-1?"), "{rows}");
    assert!(
        rows.contains("y stop it · n, Esc or Ctrl-C keep it running"),
        "{rows}"
    );
    // Another key is not typed and leaves the question up.
    assert!(app.update(key(KeyCode::Char('x'))).is_empty());
    assert!(app.input.is_empty() && app.stop_confirmation().is_some());
    // n, Esc, a second Ctrl-C: the helper left running, the view stays.
    for answer in [key(KeyCode::Char('n')), key(KeyCode::Esc), ctrl('c')] {
        assert!(app.update(answer).is_empty());
        assert_eq!(app.helper_view(), Some(fork));
        assert_eq!(app.stop_confirmation(), None);
        app.update(ctrl('c'));
        assert!(app.stop_confirmation().is_some());
    }
    // y: the helper is stopped (by its labels); the view stays, the
    // question gone, until Esc (the owner's rule of 2026-11-18: what you
    // act in closes only when you close it).
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::StopHelper(vec!["fork-1".into()])]
    );
    assert_eq!(app.helper_view(), Some(fork));
    assert_eq!(app.stop_confirmation(), None);
    app.update(key(KeyCode::Esc));
    assert_eq!(app.helper_view(), None);
    // The Ctrl-C presses armed no quit: idle on the transcript, one
    // Ctrl-C only arms it, as always.
    app.running = false;
    assert!(app.update(ctrl('c')).is_empty());
    assert!(!app.update(ctrl('c')).is_empty(), "the second quits");
    // A finished helper's view: Ctrl-C asks nothing and says why.
    let (mut app, _, fork) = helper_app();
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    app.update(live(&[]));
    assert_eq!(app.helper_view(), Some(fork));
    assert!(app.update(ctrl('c')).is_empty());
    assert_eq!(app.helper_view(), Some(fork));
    assert_eq!(app.stop_confirmation(), None);
    assert!(app.transcript.blocks().iter().any(|b| matches!(b,
        Block::Notice { text, .. } if text == "helper fork-1 is not running: nothing to stop")));
}

/// The `/helpers` picker's `s` asks to stop a running helper (nested ones
/// by their whole path) and refuses for one that is not running.
#[test]
fn the_helper_picker_stops_a_running_helper_after_a_confirmation() {
    let (mut app, _, _) = helper_app();
    app.update(Msg::Scoped(
        crate::mobs::HelperTag {
            owner: meerkat_core::SessionId::new(),
            path: vec!["scout".into(), "inner".into()],
            model: "m".into(),
            kind: crate::mobs::HelperKind::Other,
        }
        .scope(AgentEvent::ToolCallRequested {
            id: "b".into(),
            name: "grep".into(),
            args: test_args(json!({})),
        }),
    ));
    type_str(&mut app, "/helpers");
    app.update(key(KeyCode::Enter));
    let line = |app: &App| match &app.modal {
        Some(Modal::List(p)) => p.line.clone(),
        other => panic!("{other:?}"),
    };
    // To scout's helper `inner`.
    let at = match &app.modal {
        Some(Modal::List(p)) => p
            .rows
            .iter()
            .position(|r| r.text.contains("[inner"))
            .unwrap(),
        other => panic!("{other:?}"),
    };
    for _ in 0..at {
        app.update(key(KeyCode::Down));
    }
    assert!(app.update(key(KeyCode::Char('s'))).is_empty());
    assert!(
        matches!(line(&app), Some(crate::ui::picker::PickerLine::Confirm { question, .. })
            if question.starts_with("Stop helper inner?")),
        "{:?}",
        line(&app)
    );
    let rows = screen(&mut app, 24).join("\n");
    assert!(rows.contains("Stop helper inner?"), "{rows}");
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::StopHelper(vec!["scout".into(), "inner".into()])]
    );
    // The picker stays open on the same helper.
    assert_eq!(line(&app), None);
    let selected = |app: &App| match &app.modal {
        Some(Modal::List(p)) => p.selected().map(|r| r.text.clone()),
        other => panic!("{other:?}"),
    };
    assert!(selected(&app).unwrap().contains("[inner"));
    // A helper that is not running has nothing to stop.
    app.update(Msg::HelperEnded {
        path: vec!["scout".into()],
        how: "stopped by you".into(),
        told: None,
    });
    // (Ended, it is listed with `a`, after the live ones.)
    app.update(key(KeyCode::Char('a')));
    let at = match &app.modal {
        Some(Modal::List(p)) => p.rows.iter().position(|r| r.text.contains("[scout")),
        _ => None,
    }
    .unwrap();
    for _ in 0..8 {
        app.update(key(KeyCode::Up));
    }
    for _ in 0..at {
        app.update(key(KeyCode::Down));
    }
    assert!(app.update(key(KeyCode::Char('s'))).is_empty());
    assert!(
        matches!(line(&app), Some(crate::ui::picker::PickerLine::Refused(why))
            if why == "helper scout is not running: nothing to stop"),
        "{:?}",
        line(&app)
    );
}

/// The transcript's lines at width 80.
fn transcript_rows(app: &mut App) -> Vec<String> {
    let n = app.transcript.ensure_layout(80);
    app.transcript
        .visible_lines(0, n)
        .iter()
        .map(|line| line.to_string().trim_end().to_string())
        .collect()
}

/// An answer cut in the middle of a Markdown link by a helper's tool line
/// (the event order of the owner's report of 2026-11-18, invented names)
/// draws exactly as the whole answer does, the helper's lines below it,
/// not inside it.
#[test]
fn an_answer_cut_by_a_helper_s_line_draws_as_the_whole_answer() {
    let first = "Tagged it.\n\nThe [release pipeline](https://forge.example/gro";
    let rest = "up/project/-/pipelines/1) is running; I am watching it.";
    let mut cut = App::new(StatusInfo::default());
    let mut whole = App::new(StatusInfo::default());
    for app in [&mut cut, &mut whole] {
        app.transcript.push(user("go"));
        app.update(agent(AgentEvent::TurnStarted {
            turn_number: 1,
            assistant_message_id: None,
        }));
    }
    cut.update(agent(AgentEvent::TextDelta {
        delta: first.into(),
        assistant_message_id: None,
    }));
    cut.update(helper_msg(
        "watcher",
        AgentEvent::ToolCallRequested {
            id: "h1".into(),
            name: "skill".into(),
            args: test_args(json!({})),
        },
    ));
    cut.update(agent(AgentEvent::TextDelta {
        delta: rest.into(),
        assistant_message_id: None,
    }));
    whole.update(agent(AgentEvent::TextDelta {
        delta: format!("{first}{rest}"),
        assistant_message_id: None,
    }));
    let cut_rows = transcript_rows(&mut cut);
    let whole_rows = transcript_rows(&mut whole);
    assert_eq!(
        cut_rows[..whole_rows.len()],
        whole_rows[..],
        "{cut_rows:#?}"
    );
    let below = cut_rows[whole_rows.len()..].join("\n");
    assert!(below.contains("[watcher]"), "{cut_rows:#?}");
    assert!(
        whole_rows.iter().any(|r| r.contains("release pipeline")),
        "{whole_rows:#?}"
    );
}

/// The owner's rule of 2026-11-18: the `/helpers` picker stays open
/// after its acts (`f`, `a`, `s` then `y` or `n`) and closes only on Esc,
/// `q` or Ctrl-C. When the daemon's word of the stopped helper's ending
/// arrives, the list shows it in place: with the ended ones shown, its
/// row says how it ended and stays highlighted; without them, the
/// highlight goes to its neighbour.
#[test]
fn the_helper_picker_stays_open_after_its_acts_and_follows_the_helpers() {
    let (mut app, _, _) = helper_app();
    submit(&mut app, "/helpers");
    let picker = |app: &App| match &app.modal {
        Some(Modal::List(p)) if p.kind == crate::ui::PickerKind::Helpers => p.clone(),
        other => panic!("the helpers picker is open: {other:?}"),
    };
    let texts =
        |app: &App| -> Vec<String> { picker(app).rows.iter().map(|r| r.text.clone()).collect() };
    assert_eq!(texts(&app).len(), 2, "{:?}", texts(&app));
    let first = texts(&app)[0].clone();
    let second = texts(&app)[1].clone();
    let label = |text: &str| {
        if text.contains("[scout") {
            "scout"
        } else {
            "fork-1"
        }
    };
    // f folds or expands: open, the row's marker flips, same row.
    app.update(key(KeyCode::Char('f')));
    assert!(picker(&app).rows[0].text.contains('▾'), "{:?}", texts(&app));
    assert_eq!(picker(&app).cursor.index(), 0);
    // s then n: still open, nothing stopped.
    app.update(key(KeyCode::Char('s')));
    assert!(app.update(key(KeyCode::Char('n'))).is_empty());
    assert_eq!(picker(&app).line, None);
    // s then y on the first: stopped, open, on it.
    app.update(key(KeyCode::Char('s')));
    let fx = app.update(key(KeyCode::Char('y')));
    assert_eq!(fx, vec![Effect::StopHelper(vec![label(&first).into()])]);
    assert_eq!(picker(&app).cursor.index(), 0);
    assert!(picker(&app).selected().unwrap().text.contains("running"));
    // The daemon's word arrives: the row is gone from the live list and
    // the highlight is on its neighbour.
    app.update(Msg::HelperEnded {
        path: vec![label(&first).into()],
        how: "stopped by you".into(),
        told: None,
    });
    assert_eq!(texts(&app), vec![second.clone()]);
    assert_eq!(picker(&app).selected().unwrap().text, second);
    // a shows the ended one too: open, still on the same helper, the
    // ended row saying how it ended.
    app.update(key(KeyCode::Char('a')));
    let rows = texts(&app);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(picker(&app).selected().unwrap().text, second);
    assert!(
        rows.iter()
            .any(|r| r.contains(&format!("[{}", label(&first))) && r.contains("stopped by you")),
        "{rows:?}"
    );
    // Its keys close it.
    app.update(key(KeyCode::Char('q')));
    assert!(app.modal.is_none());
    submit(&mut app, "/helpers");
    app.update(key(KeyCode::Esc));
    assert!(app.modal.is_none());
    submit(&mut app, "/helpers");
    app.update(ctrl('c'));
    assert!(app.modal.is_none());
}

/// How a helper ended is said in plain words: on its line in the panel and
/// in its view's title instead of its state, and in a notice in the
/// transcript that says who was told.
#[test]
fn a_helper_s_line_says_how_it_ended() {
    let (mut app, scout, _) = helper_app();
    app.update(Msg::HelperEnded {
        path: vec!["scout".into()],
        how: "stopped by you".into(),
        told: Some("main-ab12 was told".into()),
    });
    let rows = screen(&mut app, 14);
    assert!(
        rows.iter()
            .any(|r| r.contains("[scout · m] stopped by you · 1 tool call")),
        "{rows:#?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("helper [scout · m] stopped by you; main-ab12 was told")),
        "{rows:#?}"
    );
    assert!(matches!(
        app.transcript.block(scout),
        Some(Block::Helper(fold)) if fold.state == transcript::HelperState::Cancelled
    ));
    app.update(Msg::HelperEnded {
        path: vec!["fork-1".into()],
        how: "ended when the daemon stopped".into(),
        told: None,
    });
    let text = screen(&mut app, 24).join(" ");
    assert!(
        text.contains("ended when the daemon stopped; nobody is told"),
        "{text}"
    );
    assert!(!text.contains("InterruptedByRecovery"));
}

#[test]
fn the_helper_view_follows_new_output_while_at_the_bottom() {
    let (mut app, _, _) = helper_app();
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    for i in 0..10 {
        app.update(fork_call(&format!("f{i}")));
    }
    // 12 rows: the view gets 7 (title + 6), then the input, the panel
    // (two helpers) and the status bar.
    let rows = screen(&mut app, 12);
    assert!(rows[0].starts_with("◂ [fork-1 · m]"), "{rows:?}");
    assert!(rows[5].contains("read f9"), "{rows:?}");
    app.update(fork_call("f10"));
    let rows = screen(&mut app, 12);
    assert!(rows[5].contains("read f10"), "{rows:?}");
    // Scrolled back, new output does not move the view.
    app.update(key(KeyCode::PageUp));
    let before = screen(&mut app, 12);
    app.update(fork_call("f11"));
    let after = screen(&mut app, 12);
    assert_eq!(before[1..7], after[1..7]);
    // The transcript's own scroll is untouched.
    assert!(app.scroll.follow);
    // Back at the bottom it follows again.
    app.update(Msg::Key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL)));
    let rows = screen(&mut app, 12);
    assert!(rows[5].contains("read f11"), "{rows:?}");
}

#[test]
fn a_helpers_prose_and_reasoning_are_in_its_view_in_order() {
    let (mut app, _, fork) = helper_app();
    for event in [
        AgentEvent::TurnStarted {
            turn_number: 1,
            assistant_message_id: None,
        },
        AgentEvent::ReasoningDelta {
            delta: "Where is it?".into(),
            assistant_message_id: None,
        },
        AgentEvent::TextDelta {
            delta: "It is in **a.rs**.".into(),
            assistant_message_id: None,
        },
        AgentEvent::TextComplete {
            content: "It is in **a.rs**.".into(),
            assistant_message_id: None,
        },
    ] {
        app.update(helper_msg("fork-1", event));
    }
    app.update(fork_call("f1"));
    let kinds: Vec<&str> = app
        .transcript
        .helper_view_blocks(fork)
        .into_iter()
        .map(|id| match app.transcript.block(id) {
            Some(Block::Tool { .. }) => "tool",
            Some(Block::Reasoning { .. }) => "reasoning",
            Some(Block::Assistant { .. }) => "prose",
            other => panic!("unexpected block {other:?}"),
        })
        .collect();
    assert_eq!(kinds, ["tool", "reasoning", "prose", "tool"]);
    // Ctrl-O in the view toggles the helper's latest reasoning (and
    // its latest tool output).
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    app.update(ctrl('o'));
    let reasoning = app.transcript.helper_view_blocks(fork)[1];
    assert!(matches!(
        app.transcript.block(reasoning),
        Some(Block::Reasoning {
            collapsed: false,
            ..
        })
    ));
    // Folded inline: nothing of it is on the transcript.
    assert!(app.transcript.is_hidden(reasoning));
}

#[test]
fn a_click_on_a_helper_header_toggles_it() {
    let (mut app, scout, _) = helper_app();
    draw_rows(&mut app, 0, 80, 20);
    // Rows: user (2 lines), then scout's header.
    assert_eq!(app.block_at_row(2), Some(scout));
    click_at(&mut app, 2);
    assert!(expanded(&app, scout));
    // Later than a double click.
    draw_rows(&mut app, 0, 80, 20);
    let later = Instant::now() + Duration::from_secs(2);
    app.update_at(down(2, 3), later);
    app.update_at(up(2, 3), later);
    assert!(!expanded(&app, scout));
}

#[test]
fn helpers_command_lists_expands_and_folds() {
    let (mut app, scout, fork) = helper_app();
    type_str(&mut app, "/helpers list");
    app.update(key(KeyCode::Enter));
    let listing = match app.transcript.blocks().last() {
        Some(Block::Notice { text, .. }) => text.clone(),
        other => panic!("no listing: {other:?}"),
    };
    assert!(
        listing.contains("▸ [scout · m] running · 1 tool call"),
        "{listing}"
    );
    assert!(listing.contains("[fork-1 · m]"), "{listing}");
    type_str(&mut app, "/helpers expand scout");
    app.update(key(KeyCode::Enter));
    assert!(expanded(&app, scout));
    assert!(!expanded(&app, fork));
    type_str(&mut app, "/helpers expand all");
    app.update(key(KeyCode::Enter));
    assert!(expanded(&app, scout) && expanded(&app, fork));
    type_str(&mut app, "/helpers fold all");
    app.update(key(KeyCode::Enter));
    assert!(!expanded(&app, scout) && !expanded(&app, fork));
    type_str(&mut app, "/helpers fold nobody");
    app.update(key(KeyCode::Enter));
    assert!(matches!(
        app.transcript.blocks().last(),
        Some(Block::Notice { level: NoticeLevel::Warn, text }) if text.contains("nobody")
    ));
}

#[test]
fn a_reset_drops_the_selection_and_the_view() {
    let (mut app, _, _) = helper_app();
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    app.update(Msg::ResetTranscript(Vec::new()));
    assert_eq!(app.panel_selection(), None);
    assert_eq!(app.helper_view(), None);
    assert!(!app.status.helper_panel);
}

/// `/helpers` opens the picker over this conversation's helpers, nested
/// ones indented, each as its header shows it; `f` folds and expands the
/// highlighted helper's block and the picker stays, refreshed; Enter opens
/// its full view (as Ctrl+Down then Enter does) and closes the picker.
/// With none, the picker says so. An approval request takes over and the
/// picker comes back after it.
#[test]
fn the_helper_picker_folds_and_opens_a_helper() {
    let mut empty = App::new(StatusInfo::default());
    type_str(&mut empty, "/helpers");
    empty.update(key(KeyCode::Enter));
    assert!(matches!(&empty.modal, Some(Modal::List(p))
        if p.kind == PickerKind::Helpers && p.rows.is_empty() && p.empty.contains("no helpers")));

    let (mut app, scout, fork) = helper_app();
    app.update(Msg::Scoped(
        crate::mobs::HelperTag {
            owner: meerkat_core::SessionId::new(),
            path: vec!["scout".into(), "inner".into()],
            model: "m".into(),
            kind: crate::mobs::HelperKind::Other,
        }
        .scope(AgentEvent::ToolCallRequested {
            id: "b".into(),
            name: "grep".into(),
            args: test_args(json!({})),
        }),
    ));
    type_str(&mut app, "/helpers");
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    let texts = |app: &App| match &app.modal {
        Some(Modal::List(p)) => p.rows.iter().map(|r| r.text.clone()).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    let rows = texts(&app);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(
        rows[0],
        "  ▸ [scout · m] running · 1 tool call · glob \"*\""
    );
    assert!(
        rows.iter().any(|r| r.starts_with("    ▸ [inner]")),
        "{rows:?}"
    );
    assert!(rows.iter().any(|r| r.contains("[fork-1 · m]")), "{rows:?}");
    // `f`: scout's block expands, the picker stays on it, marked ▾.
    assert!(!expanded(&app, scout));
    app.update(key(KeyCode::Char('f')));
    assert!(expanded(&app, scout));
    assert!(
        texts(&app)[0].starts_with("  ▾ [scout"),
        "{:?}",
        texts(&app)
    );
    app.update(key(KeyCode::Char('f')));
    assert!(!expanded(&app, scout));
    // An approval request takes over; the picker returns after it.
    app.update(Msg::Escalation(shell_prompt(), EscalationId(1)));
    app.update(Msg::EscalationResolved(EscalationId(1)));
    assert!(matches!(&app.modal, Some(Modal::List(p)) if p.kind == PickerKind::Helpers));
    // Back by itself, it takes keys once the user has been quiet.
    app.quiet_past_arming();
    // Enter on fork-1: its full view, the picker closed.
    let at = texts(&app)
        .iter()
        .position(|r| r.contains("[fork-1"))
        .unwrap();
    for _ in 0..at {
        app.update(key(KeyCode::Down));
    }
    app.update(key(KeyCode::Enter));
    assert!(app.modal.is_none());
    assert_eq!(app.helper_view(), Some(fork));
}

/// A helper run's start with `input` as its prompt.
fn run_started(input: &str) -> AgentEvent {
    use meerkat_core::types::{ContentInput, RunInput, TranscriptMessageIdentity};
    AgentEvent::RunStarted {
        session_id: meerkat_core::SessionId::new(),
        identity: TranscriptMessageIdentity::default(),
        input: RunInput::Content {
            content: ContentInput::Text(input.into()),
        },
        request_reasoning: None,
        reasoning_effort: None,
    }
}

/// The text of the helper's full view (title and blocks) at 80 columns.
fn view_rows(app: &mut App, label: &str) -> Vec<String> {
    app.update(Msg::Status(StatusUpdate::Helpers(Vec::new())));
    let header = app.transcript.helper_header(label).unwrap();
    app.update(ctrl_key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    assert_eq!(app.helper_view(), Some(header));
    screen(app, 30)
}

/// An `assign` helper's view names its kind, by the call's `name`, and
/// its state.
#[test]
fn an_assigned_helper_s_view_names_its_kind() {
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::Status(StatusUpdate::Agent(Some("main-ab12".into()))));
    app.transcript.push(user("go"));
    app.update(agent(AgentEvent::ToolCallRequested {
        id: "a1".into(),
        name: "assign".into(),
        args: test_args(json!({"name": "scout", "task": "find the bug"})),
    }));
    app.update(live(&["scout"]));
    for event in [
        run_started("find the bug"),
        AgentEvent::TextComplete {
            content: "It is in a.rs.".into(),
            assistant_message_id: None,
        },
    ] {
        app.update(helper_msg("scout", event));
    }
    let rows = view_rows(&mut app, "scout");
    assert!(
        rows[0].starts_with("◂ [scout · m] fresh · single-use · finished"),
        "{rows:#?}"
    );
}

/// The view of a `delegate` helper (a conversation an earlier build
/// stored) reads as the main transcript: its task
/// from the agent first, as a prompt; a later peer message in order with
/// its sender; its answer; the title names its kind and state.
#[test]
fn a_delegate_s_view_starts_with_its_task_and_shows_later_input_in_order() {
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::Status(StatusUpdate::Agent(Some("main-ab12".into()))));
    app.transcript.push(user("go"));
    app.update(agent(AgentEvent::ToolCallRequested {
        id: "d1".into(),
        name: "delegate".into(),
        args: test_args(json!({"member_id": "scout", "task": "find the bug"})),
    }));
    app.update(live(&["scout"]));
    for event in [
        run_started("find the bug"),
        AgentEvent::ToolCallRequested {
            id: "g".into(),
            name: "grep".into(),
            args: test_args(json!({"pattern": "bug"})),
        },
        AgentEvent::TextComplete {
            content: "It is in a.rs.".into(),
            assistant_message_id: None,
        },
        run_started("Peer message\nPeer message from tui/main/main-ab12:\nlook at b.rs too"),
        AgentEvent::TurnStarted {
            turn_number: 2,
            assistant_message_id: None,
        },
        AgentEvent::TextComplete {
            content: "b.rs is fine.".into(),
            assistant_message_id: None,
        },
    ] {
        app.update(helper_msg("scout", event));
    }
    let scout = app.transcript.helper_header("scout").unwrap();
    let kinds: Vec<String> = app
        .transcript
        .helper_view_blocks(scout)
        .into_iter()
        .map(|id| match app.transcript.block(id) {
            Some(Block::Task { label, text }) => format!("task {label}: {text}"),
            Some(Block::Peer { from, text, .. }) => format!("peer {from}: {text}"),
            Some(Block::Tool { .. }) => "tool".into(),
            Some(Block::Assistant { md }) => format!("prose {md}"),
            other => panic!("unexpected block {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "task task from main-ab12: find the bug",
            "tool",
            "prose It is in a.rs.",
            "peer main-ab12: look at b.rs too",
            "prose b.rs is fine.",
        ]
    );
    let rows = view_rows(&mut app, "scout");
    assert!(
        rows[0].starts_with("◂ [scout · m] delegate · finished · 1 tool call"),
        "{rows:#?}"
    );
    assert!(rows[0].ends_with("Esc back"), "{rows:#?}");
    assert_eq!(rows[1], "task from main-ab12");
    assert_eq!(rows[2], "› find the bug");
}

/// A fork's view opens with one line that it carries its parent's
/// conversation, then its own task (not the engine's framing), and
/// nothing of the parent's history.
#[test]
fn a_fork_s_view_says_it_was_forked_and_shows_only_its_own_task() {
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::Status(StatusUpdate::Agent(Some("main-ab12".into()))));
    app.update(live(&["forky"]));
    app.update(helper_msg(
        "forky",
        run_started(
            "DELEGATED DURABLE FORK\n\nTask:\nFORK-TASK go\n\nExpected output guidance:\nReport it.\n\nInstructions:\n- This is a delegated durable transcript fork.",
        ),
    ));
    app.update(helper_msg(
        "forky",
        AgentEvent::TextComplete {
            content: "FORK-DONE".into(),
            assistant_message_id: None,
        },
    ));
    let rows = view_rows(&mut app, "forky");
    // With no call seen (a stored `fork_off` child), a kept fork: seated,
    // idle, until the daemon says it was retired.
    assert!(
        rows[0].starts_with("◂ [forky · m] fork · kept · idle"),
        "{rows:#?}"
    );
    assert_eq!(
        rows[1],
        "forked from main-ab12 with its conversation so far"
    );
    assert_eq!(rows[3], "task from main-ab12");
    assert_eq!(rows[4], "› FORK-TASK go");
    let all = rows.join("\n");
    assert!(!all.contains("DELEGATED"), "{all}");
    assert!(!all.contains("Expected output"), "{all}");
    assert!(all.contains("FORK-DONE"), "{all}");
}

/// A spawned member (a call a conversation of an earlier build stored: a
/// kept fresh helper) whose task is done is idle (it stays); a failed
/// helper says so; a nested helper's task is from the helper above it.
#[test]
fn the_view_title_names_a_member_idle_and_a_nested_task_s_giver() {
    let mut app = App::new(StatusInfo::default());
    app.update(agent(AgentEvent::ToolCallRequested {
        id: "s1".into(),
        name: "spawn_many_members".into(),
        args: test_args(json!({"specs": [{"profile": "p", "member_id": "w1"}]})),
    }));
    app.update(live(&["w1"]));
    app.update(helper_msg("w1", run_started("index the repo")));
    let w1 = app.transcript.helper_header("w1").unwrap();
    app.update(live(&[]));
    match app.transcript.block(w1) {
        Some(Block::Helper(fold)) => {
            assert_eq!(
                fold.view_status_text(),
                "fresh · kept · idle · 0 tool calls"
            );
        }
        other => panic!("{other:?}"),
    }
    // Without the agent's name known, the giver is "its parent".
    match app
        .transcript
        .block(app.transcript.helper_view_blocks(w1)[0])
    {
        Some(Block::Task { label, .. }) => assert_eq!(label, "task from its parent"),
        other => panic!("{other:?}"),
    }
    let nested = Msg::Scoped(
        crate::mobs::HelperTag {
            owner: meerkat_core::SessionId::new(),
            path: vec!["w1".into(), "inner".into()],
            model: "m".into(),
            kind: crate::mobs::HelperKind::Other,
        }
        .scope(run_started("nested work")),
    );
    app.update(nested);
    let inner = app.transcript.helper_header("inner").unwrap();
    match app
        .transcript
        .block(app.transcript.helper_view_blocks(inner)[0])
    {
        Some(Block::Task { label, text }) => {
            assert_eq!(label, "task from w1");
            assert_eq!(text, "nested work");
        }
        other => panic!("{other:?}"),
    }
}

/// The note of one ending as the daemon words it.
fn ending_note(label: &str, how: crate::mobs::endings::How) -> String {
    crate::daemon::agents::endings::note_text(&[crate::daemon::agents::endings::NoteItem {
        label: label.into(),
        how,
        rest: None,
        with: Vec::new(),
    }])
}

/// The folded line the daemon sends with a run a helper's note started is
/// the one drawn: the note's words are not read for it.
#[test]
fn the_daemon_s_summary_of_a_note_is_its_folded_line() {
    use crate::daemon::agents::HELPER_ENDED_LABEL;
    use crate::daemon::agents::endings::{NoteItem, summary_of};
    use crate::mobs::endings::How;
    let items = [NoteItem {
        label: "scout".into(),
        how: How::Failed {
            reason: "it timed out".into(),
        },
        rest: None,
        with: Vec::new(),
    }];
    let (mut app, _, _) = helper_app();
    app.update(Msg::Triggered {
        summary: Some(summary_of(&items)),
        label: HELPER_ENDED_LABEL.into(),
        // Words this build does not know: the summary says what happened.
        prompt: "Something about scout, worded anew.".into(),
    });
    assert!(
        matches!(
            app.transcript.last(),
            Some(Block::HelperNote { title, helpers, failed: true, .. })
                if title == "helper scout failed: it timed out" && helpers == &["scout"]
        ),
        "{:?}",
        app.transcript.last()
    );
}

/// A run the daemon started with a helper's ending note is one folded line
/// (how it ended and the answer's first line), not the whole note under a
/// clock; a click opens the note. The daemon's notice of the same ending
/// is not drawn as well, whichever of the two comes first; a note steered
/// into a running turn (no run started for it) leaves the notice alone.
#[test]
fn a_helper_s_ending_note_is_one_folded_line() {
    use crate::daemon::agents::{HELPER_ENDED_LABEL, HELPER_IDLE_LABEL};
    use crate::mobs::endings::How;
    let long = format!(
        "The parser drops the last token.\n\n{}",
        "More of the answer. ".repeat(200)
    );
    let ended = |app: &mut App, label: &str, how: &str| {
        app.update(Msg::HelperEnded {
            path: vec![label.into()],
            how: how.into(),
            told: Some("main-ab12 was told".into()),
        });
    };
    // The notice first, then the run the note started.
    let (mut app, _, _) = helper_app();
    ended(&mut app, "scout", "finished");
    app.update(Msg::Triggered {
        summary: None,
        label: HELPER_ENDED_LABEL.into(),
        prompt: ending_note("scout", How::Done { answer: long }),
    });
    let kinds: Vec<String> = app
        .transcript
        .blocks()
        .iter()
        .map(|b| format!("{b:?}").chars().take(60).collect())
        .collect();
    assert!(
        matches!(
            app.transcript.last(),
            Some(Block::HelperNote {
                collapsed: true,
                ..
            })
        ),
        "{kinds:#?}"
    );
    let rows = screen(&mut app, 30);
    let text = rows.join("\n");
    assert!(
        rows.iter().any(|r| r
            .starts_with("✓ helper scout finished: The parser drops the last token.")
            && r.ends_with('▸')),
        "{text}"
    );
    assert!(!text.contains("More of the answer"), "{text}");
    assert!(!text.contains("was told"), "{text}");
    assert!(!text.contains('⏰'), "{text}");
    // A click opens the whole note.
    let row = rows
        .iter()
        .position(|r| r.starts_with("✓ helper scout"))
        .unwrap();
    click_at(&mut app, u16::try_from(row).unwrap());
    assert!(matches!(
        app.transcript.last(),
        Some(Block::HelperNote {
            collapsed: false,
            ..
        })
    ));
    let open = screen(&mut app, 120).join("\n");
    assert!(open.contains("✓ helper scout finished ▾"), "{open}");
    assert!(open.contains("More of the answer"), "{open}");

    // The run first, then the notice: still one line, no notice.
    let (mut app, _, _) = helper_app();
    app.update(Msg::Triggered {
        summary: None,
        label: HELPER_ENDED_LABEL.into(),
        prompt: ending_note(
            "scout",
            How::Failed {
                reason: "the model refused".into(),
            },
        ),
    });
    ended(&mut app, "scout", "failed: the model refused");
    let text = screen(&mut app, 30).join("\n");
    assert!(
        text.contains("✗ helper scout failed: the model refused ▸"),
        "{text}"
    );
    assert!(!text.contains("was told"), "{text}");

    // Several endings in one note, and a helper going idle.
    let (mut app, _, _) = helper_app();
    app.update(Msg::Triggered {
        summary: None,
        label: HELPER_ENDED_LABEL.into(),
        prompt: crate::daemon::agents::endings::note_text(&[
            crate::daemon::agents::endings::NoteItem {
                label: "scout".into(),
                how: How::Done {
                    answer: "found it".into(),
                },
                rest: None,
                with: Vec::new(),
            },
            crate::daemon::agents::endings::NoteItem {
                label: "fork-1".into(),
                how: How::StoppedByUser,
                rest: None,
                with: Vec::new(),
            },
        ]),
    });
    app.update(Msg::Triggered {
        summary: None,
        label: HELPER_IDLE_LABEL.into(),
        prompt: ending_note(
            "scout",
            How::WentIdle {
                answer: "waiting for more".into(),
                failed: None,
            },
        ),
    });
    let text = screen(&mut app, 30).join("\n");
    assert!(
        text.contains("✓ 2 helpers ended: scout finished; fork-1 stopped by you ▸"),
        "{text}"
    );
    assert!(
        text.contains("✓ helper scout went idle: waiting for more ▸"),
        "{text}"
    );

    // Steered into a running turn: no run started, so only the notice.
    let (mut app, _, _) = helper_app();
    ended(&mut app, "scout", "finished");
    let text = screen(&mut app, 30).join("\n");
    assert!(
        text.contains("helper [scout · m] finished; main-ab12 was told"),
        "{text}"
    );
    // `/export` has the whole note under a heading.
    let (mut app, _, _) = helper_app();
    app.update(Msg::Triggered {
        summary: None,
        label: HELPER_ENDED_LABEL.into(),
        prompt: ending_note(
            "scout",
            How::Done {
                answer: "found it".into(),
            },
        ),
    });
    let export = app.transcript.export_text();
    assert!(
        export.contains("## helper scout finished (the note the agent was given)\n"),
        "{export}"
    );
    assert!(export.contains("found it"), "{export}");
}

/// A helper run's end.
fn run_completed() -> AgentEvent {
    AgentEvent::RunCompleted {
        session_id: meerkat_core::SessionId::new(),
        identity: Default::default(),
        result: "ok".into(),
        structured_output: None,
        extraction_required: false,
        usage: Default::default(),
        terminal_cause_kind: None,
        assistant_message_id: None,
    }
}

/// The line `/helpers` shows for `label` (with the ended ones shown too).
fn picker_line(app: &mut App, label: &str) -> String {
    type_str(app, "/helpers");
    app.update(key(KeyCode::Enter));
    if matches!(&app.modal, Some(Modal::List(p)) if p.footer.contains("a: show")) {
        app.update(key(KeyCode::Char('a')));
    }
    let line = match &app.modal {
        Some(Modal::List(p)) => p
            .rows
            .iter()
            .map(|r| r.text.clone())
            .find(|r| r.contains(&format!("[{label}")))
            .unwrap(),
        other => panic!("{other:?}"),
    };
    app.update(key(KeyCode::Esc));
    line
}

/// A kept helper that a message wakes again keeps its one header (it is
/// the same helper, not a new one), stays in the panel while it is seated
/// (running, then idle; turns do not matter), says `idle` while seated and
/// `retired` once the daemon says it left its mob.
#[test]
fn a_woken_kept_helper_keeps_its_header_and_says_idle_until_retired() {
    let mut app = App::new(StatusInfo::default());
    app.update(agent(AgentEvent::ToolCallRequested {
        id: "s1".into(),
        name: "assign".into(),
        args: test_args(json!({"name": "w1", "task": "index the repo", "keep": true})),
    }));
    app.update(Msg::SeatedHelpers(vec!["w1".into()]));
    app.update(live(&["w1"]));
    app.update(helper_msg("w1", run_started("index the repo")));
    let w1 = app.transcript.helper_header("w1").unwrap();
    app.update(helper_msg("w1", run_completed()));
    app.update(live(&[]));
    assert!(picker_line(&mut app, "w1").contains("[w1 · m] fresh · kept · idle · 0 tool calls"));
    // The next turn: seated and idle, it stays on the panel.
    type_str(&mut app, "next");
    app.update(key(KeyCode::Enter));
    app.update(Msg::TurnFinished(Ok(summary())));
    assert_eq!(app.helper_panel(), vec![w1]);
    // A message wakes it: the same header, running, in the panel.
    app.update(helper_msg("w1", run_started("UPDATE")));
    app.update(live(&["w1"]));
    assert_eq!(app.transcript.helper_header("w1"), Some(w1));
    assert_eq!(app.transcript.helper_headers().count(), 1);
    assert_eq!(app.helper_panel(), vec![w1]);
    // Its run ends: listed, idle.
    app.update(helper_msg("w1", run_completed()));
    app.update(live(&[]));
    assert_eq!(app.helper_panel(), vec![w1]);
    let rows = screen(&mut app, 14);
    assert!(
        rows.iter()
            .any(|r| r.contains("[w1 · m] fresh · kept · idle · 0 tool calls")),
        "{rows:#?}"
    );
    assert_eq!(app.transcript.helper_headers().count(), 1);
    // Retired: said so on its line, and nothing else is shown.
    let before = app.transcript.len();
    app.update(Msg::HelperEnded {
        path: vec!["w1".into()],
        how: crate::daemon::protocol::HELPER_RETIRED.into(),
        told: None,
    });
    app.update(Msg::SeatedHelpers(Vec::new()));
    assert_eq!(app.transcript.len(), before);
    assert!(picker_line(&mut app, "w1").contains("[w1 · m] fresh · kept · retired · 0 tool calls"));
    // A helper whose line says how it ended keeps those words.
    let (mut app, _, _) = helper_app();
    app.update(Msg::HelperEnded {
        path: vec!["scout".into()],
        how: "finished".into(),
        told: Some("main-ab12 was told".into()),
    });
    app.update(Msg::HelperEnded {
        path: vec!["scout".into()],
        how: crate::daemon::protocol::HELPER_RETIRED.into(),
        told: None,
    });
    assert!(picker_line(&mut app, "scout").contains("[scout · m] finished · 1 tool call"));
}

/// A kept fork whose run ended is idle until it is retired; a stored
/// conversation's `fork_off` child is a kept fork. A single-use fork's line
/// says finished, then retired once the daemon retires it (it does once
/// its outcome reached its parent).
#[test]
fn a_fork_s_line_says_idle_then_retired() {
    for (name, args, life, done) in [
        (
            "fork_off",
            json!({"member_id": "kid", "task": "look"}),
            "kept",
            "idle",
        ),
        (
            "assign",
            json!({"name": "kid", "task": "look", "context": "fork", "keep": true}),
            "kept",
            "idle",
        ),
        (
            "assign",
            json!({"name": "kid", "task": "look", "context": "fork"}),
            "single-use",
            "finished",
        ),
    ] {
        let mut app = App::new(StatusInfo::default());
        app.update(agent(AgentEvent::ToolCallRequested {
            id: "f1".into(),
            name: name.into(),
            args: test_args(args),
        }));
        app.update(live(&["kid"]));
        app.update(helper_msg(
            "kid",
            run_started("DELEGATED DURABLE FORK\n\nTask:\nlook"),
        ));
        app.update(helper_msg("kid", run_completed()));
        app.update(live(&[]));
        let line = picker_line(&mut app, "kid");
        assert!(line.contains(&format!("[kid · m] fork · {life}")), "{line}");
        let header = app.transcript.helper_header("kid").unwrap();
        match app.transcript.block(header) {
            Some(Block::Helper(fold)) => assert!(
                fold.view_status_text()
                    .starts_with(&format!("fork · {life} · {done}")),
                "{}",
                fold.view_status_text()
            ),
            other => panic!("{other:?}"),
        }
        app.update(Msg::HelperEnded {
            path: vec!["kid".into()],
            how: crate::daemon::protocol::HELPER_RETIRED.into(),
            told: None,
        });
        let line = picker_line(&mut app, "kid");
        assert!(
            line.contains(&format!("[kid · m] fork · {life} · retired")),
            "{line}"
        );
    }
}

/// `/helpers` lists the live helpers (running, or seated and idle) by
/// default; the ended ones (the daemon said how, or the mob's listing no
/// longer names them) come with `a`, dim, after the live ones, and the
/// hint counts them. `/helpers list` counts them, `/helpers list all`
/// lists them.
#[test]
fn helpers_lists_the_live_helpers_and_shows_the_ended_ones_on_a() {
    let mut app = App::new(StatusInfo::default());
    for (tool, args) in [
        ("a1", json!({"name": "done1", "task": "t"})),
        ("a2", json!({"name": "w1", "task": "t", "keep": true})),
        ("a3", json!({"name": "kid", "task": "t", "context": "fork"})),
    ] {
        app.update(agent(AgentEvent::ToolCallRequested {
            id: tool.into(),
            name: "assign".into(),
            args: test_args(args),
        }));
    }
    app.update(live(&["done1", "w1", "kid"]));
    for label in ["done1", "w1", "kid"] {
        app.update(helper_msg(label, run_started("work")));
        app.update(helper_msg(label, run_completed()));
    }
    app.update(live(&[]));
    // done1 finished (the daemon says so); kid is no longer in the mob.
    app.update(Msg::HelperEnded {
        path: vec!["done1".into()],
        how: "finished".into(),
        told: Some("main-ab12 was told".into()),
    });
    app.update(Msg::SeatedHelpers(vec!["main-ab12".into(), "w1".into()]));
    type_str(&mut app, "/helpers");
    app.update(key(KeyCode::Enter));
    let rows = |app: &App| match &app.modal {
        Some(Modal::List(p)) => (
            p.rows
                .iter()
                .map(|r| (r.text.clone(), r.dim))
                .collect::<Vec<_>>(),
            p.footer.clone(),
        ),
        other => panic!("{other:?}"),
    };
    let (shown, footer) = rows(&app);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(
        shown[0].0.contains("[w1 · m] fresh · kept · idle"),
        "{shown:?}"
    );
    assert!(footer.contains("a: show 2 ended"), "{footer}");
    app.update(key(KeyCode::Char('a')));
    let (shown, footer) = rows(&app);
    assert_eq!(shown.len(), 3, "{shown:?}");
    assert!(!shown[0].1 && shown[1].1 && shown[2].1, "{shown:?}");
    assert!(
        shown[1]
            .0
            .contains("[done1 · m] fresh · single-use · finished"),
        "{shown:?}"
    );
    assert!(
        shown[2].0.contains("[kid · m] fork · single-use · done"),
        "{shown:?}"
    );
    assert!(footer.contains("a: hide the ended"), "{footer}");
    // An ended one's full view is still reachable.
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Enter));
    assert_eq!(app.helper_view(), app.transcript.helper_header("done1"));
    app.update(key(KeyCode::Esc));
    // Only ended ones: the empty line says how to see them.
    app.update(Msg::HelperEnded {
        path: vec!["w1".into()],
        how: crate::daemon::protocol::HELPER_RETIRED.into(),
        told: None,
    });
    type_str(&mut app, "/helpers");
    app.update(key(KeyCode::Enter));
    assert!(matches!(&app.modal, Some(Modal::List(p))
        if p.rows.is_empty() && p.empty == "no live helpers; a shows 3 ended"));
    app.update(key(KeyCode::Char('a')));
    assert_eq!(rows(&app).0.len(), 3);
    app.update(key(KeyCode::Esc));
    // The listings.
    type_str(&mut app, "/helpers list");
    app.update(key(KeyCode::Enter));
    let text = screen(&mut app, 30).join("\n");
    assert!(
        text.contains("no live helpers") && text.contains("3 ended helpers (/helpers list all"),
        "{text}"
    );
    type_str(&mut app, "/helpers list all");
    app.update(key(KeyCode::Enter));
    let text = screen(&mut app, 40).join("\n");
    assert!(text.contains("[kid · m] fork"), "{text}");
}

/// `/helpers` with more helpers than fit on the screen scrolls to keep
/// the selection in sight.
#[test]
fn the_helper_picker_scrolls_through_more_helpers_than_a_page() {
    let mut app = App::new(StatusInfo::default());
    let labels: Vec<String> = (0..40).map(|i| format!("h{i:02}")).collect();
    let names: Vec<&str> = labels.iter().map(String::as_str).collect();
    app.update(live(&names));
    type_str(&mut app, "/helpers");
    app.update(key(KeyCode::Enter));
    let rows = screen(&mut app, 20).join("\n");
    assert!(
        rows.contains("[h00 · m]") && !rows.contains("[h39 · m]"),
        "{rows}"
    );
    for _ in 0..39 {
        app.update(key(KeyCode::Down));
    }
    let rows = screen(&mut app, 20).join("\n");
    assert!(
        rows.contains("[h39 · m]") && !rows.contains("[h00 · m]"),
        "{rows}"
    );
}

/// What the transcript holds of helpers' ending notes: the folded lines'
/// titles, and every user block (none may hold a note's words).
fn notes_and_users(app: &App) -> (Vec<String>, Vec<String>) {
    let mut notes = Vec::new();
    let mut users = Vec::new();
    for block in app.transcript.blocks() {
        match block {
            Block::HelperNote { title, .. } => notes.push(title.clone()),
            Block::User { text } => users.push(text.clone()),
            _ => {}
        }
    }
    (notes, users)
}

/// A helper's ending note is only ever its one folded line, never the
/// whole note and never as the user's message, in every order its two
/// reports (the daemon's `rkat/triggered`, the run's own input) arrive:
/// the first run of an idle agent; a follow-up run of a turn that is
/// running (the field case: a single-use helper's second ending, its
/// announcement first, then the run's input as a remote turn); the
/// input first; no announcement at all; and two endings with the same
/// words in one turn, each its own line.
#[test]
fn a_helper_s_ending_note_is_one_folded_line_in_every_order() {
    use crate::daemon::agents::HELPER_ENDED_LABEL;
    use crate::mobs::endings::How;
    let first = ending_note(
        "scout",
        How::Done {
            answer: "Found it.\nMore.".into(),
        },
    );
    let second = ending_note(
        "scout",
        How::Done {
            answer: "Also this.\nMore.".into(),
        },
    );
    let triggered = |app: &mut App, prompt: &str| {
        app.update(Msg::Triggered {
            summary: None,
            label: HELPER_ENDED_LABEL.into(),
            prompt: prompt.into(),
        });
    };
    let remote = |app: &mut App, prompt: &str| {
        app.update(Msg::RemoteTurn {
            prompt: Some(prompt.into()),
        });
    };
    let users_hold_no_note = |users: &[String]| {
        assert!(
            users.iter().all(|u| !u.contains("Your helper")),
            "{users:#?}"
        );
    };

    // Idle agent, announcement first; then a follow-up run of the same
    // turn, announced first too (the field case).
    let (mut app, _, _) = helper_app();
    triggered(&mut app, &first);
    remote(&mut app, &first);
    triggered(&mut app, &second);
    remote(&mut app, &second);
    let (notes, users) = notes_and_users(&app);
    assert_eq!(notes.len(), 2, "{notes:#?}");
    assert_eq!(users, vec!["go".to_string()]);
    let text = screen(&mut app, 40).join("\n");
    assert!(!text.contains("Your helper"), "{text}");
    assert!(!text.contains("More."), "{text}");

    // The run's input first, the announcement after: idle and running.
    let (mut app, _, _) = helper_app();
    remote(&mut app, &first);
    triggered(&mut app, &first);
    remote(&mut app, &second);
    triggered(&mut app, &second);
    let (notes, users) = notes_and_users(&app);
    assert_eq!(notes.len(), 2, "{notes:#?}");
    users_hold_no_note(&users);

    // A note whose run nobody announced (a note steered into a running
    // run that missed its last boundary) is still the folded line.
    let (mut app, _, _) = helper_app();
    remote(&mut app, &first);
    remote(&mut app, &second);
    let (notes, users) = notes_and_users(&app);
    assert_eq!(notes.len(), 2, "{notes:#?}");
    users_hold_no_note(&users);

    // Something drawn between the announcement and the run's input does
    // not make the input a user message.
    let (mut app, _, _) = helper_app();
    triggered(&mut app, &first);
    app.update(Msg::Notice {
        level: NoticeLevel::Info,
        text: "something else".into(),
    });
    remote(&mut app, &first);
    let (notes, users) = notes_and_users(&app);
    assert_eq!(notes.len(), 1, "{notes:#?}");
    users_hold_no_note(&users);

    // The same words twice in one turn are two endings: two lines.
    let (mut app, _, _) = helper_app();
    triggered(&mut app, &first);
    remote(&mut app, &first);
    triggered(&mut app, &first);
    remote(&mut app, &first);
    let (notes, users) = notes_and_users(&app);
    assert_eq!(notes.len(), 2, "{notes:#?}");
    users_hold_no_note(&users);
}

/// The second field sighting's order: the agent's turn runs (a tool call
/// done, a paragraph written) when its single-use helper finishes; the
/// daemon's notice of the ending, the note's announcement, then the note
/// as the input of the turn's next run, which the agent answers. The note
/// is its one folded line, never the whole note as the user's message.
#[test]
fn a_note_that_reaches_a_running_turn_is_one_folded_line() {
    use crate::daemon::agents::HELPER_ENDED_LABEL;
    use crate::mobs::endings::How;
    let note = ending_note(
        "scout",
        How::Done {
            answer: "## Findings\n\nThe parser drops the last token.\n\n```rust\nlet x = 1;\n```"
                .into(),
        },
    );
    let (mut app, _, _) = helper_app();
    app.update(Msg::RemoteTurn { prompt: None });
    app.update(agent(AgentEvent::ToolCallRequested {
        id: "t9".into(),
        name: "read_file".into(),
        args: test_args(json!({"path": "src/x.rs"})),
    }));
    app.update(agent(AgentEvent::ToolExecutionCompleted {
        id: "t9".into(),
        name: "read_file".into(),
        is_error: false,
        duration_ms: 1,
    }));
    app.update(agent(AgentEvent::TextComplete {
        content: "Still checking the lexer.".into(),
        assistant_message_id: None,
    }));
    app.update(Msg::HelperEnded {
        path: vec!["scout".into()],
        how: "finished".into(),
        told: Some("main-ab12 was told".into()),
    });
    app.update(Msg::Triggered {
        summary: None,
        label: HELPER_ENDED_LABEL.into(),
        prompt: note.clone(),
    });
    app.update(Msg::RemoteTurn { prompt: Some(note) });
    app.update(agent(AgentEvent::TextComplete {
        content: "The helper confirms it.".into(),
        assistant_message_id: None,
    }));
    let (notes, users) = notes_and_users(&app);
    assert_eq!(notes, vec!["helper scout finished".to_string()]);
    assert_eq!(users, vec!["go".to_string()]);
    let text = screen(&mut app, 40).join("\n");
    assert!(text.contains("✓ helper scout finished"), "{text}");
    assert!(!text.contains("Your helper"), "{text}");
    assert!(!text.contains("let x = 1"), "{text}");
}

/// The run input of a message `from` sent, as the engine gives it.
fn peer_input(from: &str, body: &str) -> String {
    format!("Peer message\nPeer message from tui/main/{from}:\n{body}\nPeer message")
}

/// A helper's run started on `input`.
fn run_on(input: &str) -> AgentEvent {
    use meerkat_core::types::{ContentInput, RunInput, TranscriptMessageIdentity};
    AgentEvent::RunStarted {
        session_id: meerkat_core::SessionId::new(),
        identity: TranscriptMessageIdentity::default(),
        input: RunInput::Content {
            content: ContentInput::Text(input.into()),
        },
        request_reasoning: None,
        reasoning_effort: None,
    }
}

/// The `✉` lines of `lines`.
fn mail(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| line.contains('✉'))
        .map(|line| line.trim_end().to_string())
        .collect()
}

/// Item 7 (owner, 0.0.17): a message is drawn once, in the view of the
/// member that received it. The agent's message to its helper is the
/// helper's (its full view; in the main view only while the helper is
/// unfolded, tagged and indented as the helper's lines are, never as a
/// message the agent received); the helper's message to the agent is the
/// agent's (the main view, once; not in the helper's view).
#[test]
fn a_message_is_drawn_once_in_the_view_of_the_member_that_received_it() {
    let (mut app, scout, _) = helper_app();
    app.update(helper_msg(
        "scout",
        run_on(&peer_input("main-kgem", "fix the six comments")),
    ));
    app.update(Msg::Peer {
        from: "scout".into(),
        text: "all six fixed".into(),
    });
    // Folded: the agent's received message only.
    let folded = mail(&screen(&mut app, 40));
    assert_eq!(
        folded,
        ["✉ from agent scout: all six fixed ▸"],
        "{folded:?}"
    );
    // Unfolded: the helper's received message too, as the helper's line.
    app.transcript.toggle_helper(scout);
    let open = mail(&screen(&mut app, 40));
    assert_eq!(
        open.iter()
            .filter(|l| l.contains("✉ [scout · m] from main-kgem: fix the six comments"))
            .count(),
        1,
        "{open:?}"
    );
    assert_eq!(
        open.iter()
            .filter(|l| l.contains("✉ from agent scout"))
            .count(),
        1
    );
    assert!(
        !open.iter().any(|l| l.contains("✉ from agent main-kgem")),
        "never as the agent's own: {open:?}"
    );
    // The helper's full view: its received message once, not the agent's.
    let view: Vec<String> = app
        .transcript
        .helper_view_lines(scout, 80)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();
    let view = mail(&view);
    assert_eq!(view.len(), 1, "{view:?}");
    assert!(
        view[0].contains("from main-kgem: fix the six comments"),
        "{view:?}"
    );
}

/// Invariant: no block of a folded helper is among the main view's lines
/// (only its header is), whatever it is: tool lines, its prose, a message
/// it received, a nested helper's blocks.
#[test]
fn no_block_of_a_folded_helper_is_in_the_main_view() {
    let (mut app, scout, fork) = helper_app();
    app.update(helper_msg(
        "scout",
        run_on(&peer_input("main-kgem", "and the docs")),
    ));
    app.update(helper_msg(
        "scout",
        AgentEvent::TextComplete {
            content: "looking".into(),
            assistant_message_id: None,
        },
    ));
    for header in [scout, fork] {
        assert!(!expanded(&app, header));
    }
    for (id, _) in app.transcript.iter() {
        let mut owner = app.transcript.owner_of(id);
        let mut folded = false;
        while let Some(up) = owner {
            folded |= !expanded(&app, up);
            owner = app.transcript.owner_of(up);
        }
        if folded {
            assert!(
                app.transcript.is_hidden(id),
                "{:?}",
                app.transcript.block(id)
            );
        }
    }
    // The main view's lines (the transcript's, without the helpers' panel).
    let total = app.transcript.ensure_layout(80);
    let rows: Vec<String> = app
        .transcript
        .visible_lines(0, total)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();
    for label in ["[scout", "[fork-1"] {
        assert_eq!(
            rows.iter().filter(|row| row.contains(label)).count(),
            1,
            "only its header: {rows:#?}"
        );
    }
    // Its message is not drawn; its prose shows at most as the header's
    // last activity.
    assert!(
        !rows.iter().any(|row| row.contains("and the docs")),
        "{rows:#?}"
    );
    assert!(
        rows.iter()
            .filter(|row| row.contains("looking"))
            .all(|row| row.contains("[scout")),
        "{rows:#?}"
    );
}

/// Another client's message that carries notes held after an interrupt
/// shows them as their folded line and the message as its own words.
#[test]
fn held_notes_before_another_client_s_message_fold_and_leave_its_words() {
    use crate::mobs::endings::How;
    let note = ending_note(
        "scout",
        How::Done {
            answer: "Found it.".into(),
        },
    );
    let (mut app, _, _) = helper_app();
    app.update(Msg::RemoteTurn {
        prompt: Some(format!(
            "{note}\n\n{}\n\nnow fix it",
            crate::daemon::local::HELD_NOTE_SEPARATOR
        )),
    });
    let (notes, users) = notes_and_users(&app);
    assert_eq!(notes, vec!["helper scout finished".to_string()]);
    assert_eq!(users, vec!["go".to_string(), "now fix it".to_string()]);
}
