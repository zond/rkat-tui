//! The help dialog (`/help`, `?`): its views, keys, scrolling, wrapping,
//! and how it shares the screen with the agent and the approval prompt.

use super::*;

/// The whole app drawn into a `width x height` test terminal, its rows
/// with trailing blanks trimmed.
fn drawn(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| render::draw_at(f, app, Instant::now()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// What is inside the help's box on `rows`: the tab line, the text's
/// lines and the footer, each trimmed at its end.
fn inside(rows: &[String]) -> Vec<String> {
    let top = rows
        .iter()
        .position(|r| r.contains("┌ Help "))
        .unwrap_or_else(|| panic!("no help box: {rows:#?}"));
    rows[top + 1..]
        .iter()
        .take_while(|r| !r.contains('└'))
        .map(|r| {
            let start = r.find('│').map_or(0, |i| i + '│'.len_utf8());
            let end = r.rfind('│').filter(|end| *end >= start).unwrap_or(r.len());
            r[start..end].trim_end().to_string()
        })
        .collect()
}

/// The help's text lines on screen (between its tab line and its
/// footer).
fn body(rows: &[String]) -> Vec<String> {
    let inner = inside(rows);
    inner
        .iter()
        .skip(1)
        .take_while(|line| !line.contains("j/k PgUp/PgDn"))
        .cloned()
        .collect()
}

fn help(app: &App) -> &HelpDialog {
    match &app.modal {
        Some(Modal::Help(help)) => help,
        other => panic!("no help open: {other:?}"),
    }
}

/// `text` without its whitespace: what wrapping must keep (a word too
/// long for its column is broken, not cut).
fn words(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Every line of the open view, paged through at `width x height` from
/// its top to its end.
fn whole_view(app: &mut App, width: u16, height: u16) -> Vec<String> {
    app.update(key(KeyCode::Home));
    let mut lines: Vec<String> = Vec::new();
    loop {
        let rows = drawn(app, width, height);
        for row in &rows {
            assert!(
                crate::ui::markdown::display_width(row) <= usize::from(width),
                "{row}"
            );
        }
        let at = help(app).scroll;
        let shown = body(&rows);
        assert!(!shown.is_empty(), "{rows:#?}");
        lines.truncate(at);
        lines.extend(shown);
        if at == help(app).scroll_max {
            return lines;
        }
        app.update(key(KeyCode::PageDown));
        assert!(help(app).scroll > at, "PgDn moved on");
    }
}

#[test]
fn help_opens_on_the_commands_and_question_mark_on_the_keys() {
    let mut app = App::new(StatusInfo::default());
    assert!(submit(&mut app, "/help").is_empty());
    assert_eq!(help(&app).view, HelpView::Commands);
    assert!(
        app.transcript.blocks().is_empty(),
        "nothing in the transcript"
    );
    let rows = drawn(&mut app, 100, 30);
    let inner = inside(&rows);
    assert!(
        inner[0].contains("Commands") && inner[0].contains("Keys"),
        "{inner:?}"
    );
    assert!(
        body(&rows)[0].starts_with("  /settings [filter]"),
        "{rows:#?}"
    );
    // Tab and Shift+Tab switch the view, at its top.
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Tab));
    assert_eq!(help(&app).view, HelpView::Keys);
    assert_eq!(help(&app).scroll, 0);
    assert!(body(&drawn(&mut app, 100, 30))[0].starts_with("  ? (empty input)"));
    app.update(key(KeyCode::BackTab));
    assert_eq!(help(&app).view, HelpView::Commands);
    app.update(key(KeyCode::Esc));
    assert!(app.modal.is_none());
    // `?` on an empty input opens it on the keys; with text it is typed.
    assert!(app.update(key(KeyCode::Char('?'))).is_empty());
    assert_eq!(help(&app).view, HelpView::Keys);
    app.update(key(KeyCode::Esc));
    type_str(&mut app, "why?");
    assert!(app.modal.is_none());
    assert_eq!(app.input.text(), "why?");
    assert!(app.transcript.blocks().is_empty());
}

#[test]
fn the_help_closes_with_esc_q_question_mark_and_ctrl_c_only() {
    let mut app = App::new(StatusInfo::default());
    for close in [
        key(KeyCode::Esc),
        key(KeyCode::Char('q')),
        key(KeyCode::Char('?')),
        ctrl('c'),
    ] {
        app.update(key(KeyCode::Char('?')));
        // Other keys neither close it nor reach the input (Enter sends
        // nothing); a held Esc does not close it.
        for stay in [KeyCode::Enter, KeyCode::Char('x'), KeyCode::Char('1')] {
            assert!(app.update(key(stay)).is_empty(), "{stay:?}");
        }
        let mut held = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        held.kind = KeyEventKind::Repeat;
        app.update(Msg::Key(held));
        assert!(matches!(app.modal, Some(Modal::Help(_))));
        assert!(app.update(close.clone()).is_empty(), "{close:?}");
        assert!(app.modal.is_none(), "{close:?}");
        assert!(app.input.is_empty());
    }
    // Ctrl-C closed the help only: it armed no quit.
    assert!(!app.quit_confirm_armed(Instant::now()));
    assert!(app.transcript.blocks().is_empty());
}

#[test]
fn the_help_scrolls_by_line_page_and_end_and_says_where_it_is() {
    let mut app = App::new(StatusInfo::default());
    submit(&mut app, "/help");
    let rows = drawn(&mut app, 100, 24);
    let first = body(&rows);
    let page = first.len();
    let total = help_text_len(&mut app, 100, 24);
    let footer = inside(&rows).last().cloned().unwrap();
    assert!(
        footer.starts_with(&format!("1-{page}/{total} · ")),
        "{footer}"
    );
    assert!(footer.contains("Esc, q or Ctrl-C closes"), "{footer}");
    for (code, at) in [
        (KeyCode::Down, 1),
        (KeyCode::Char('j'), 2),
        (KeyCode::Char('k'), 1),
        (KeyCode::Up, 0),
        (KeyCode::Up, 0),
    ] {
        app.update(key(code));
        assert_eq!(help(&app).scroll, at, "{code:?}");
    }
    app.update(key(KeyCode::Down));
    assert_eq!(body(&drawn(&mut app, 100, 24))[0], first[1]);
    app.update(key(KeyCode::PageDown));
    // From the second line a page down, which keeps the page's last line.
    assert_eq!(help(&app).scroll, page, "1 + a page");
    app.update(key(KeyCode::PageUp));
    assert_eq!(help(&app).scroll, 1);
    app.update(key(KeyCode::End));
    let rows = drawn(&mut app, 100, 24);
    let end = help(&app).scroll_max;
    assert_eq!(help(&app).scroll, end);
    assert_eq!(end + page, total);
    let footer = inside(&rows).last().cloned().unwrap();
    assert!(
        footer.starts_with(&format!("{}-{total}/{total}", end + 1)),
        "{footer}"
    );
    assert!(
        body(&rows).last().unwrap().contains("/new starts over."),
        "{rows:#?}"
    );
    // No further than the end.
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::PageDown));
    assert_eq!(help(&app).scroll, end);
    app.update(key(KeyCode::Home));
    assert_eq!(help(&app).scroll, 0);
    // The mouse wheel scrolls the help, not the transcript.
    app.update(mouse(MouseEventKind::ScrollDown, 5, 5));
    assert_eq!(help(&app).scroll, keymap::LINE_SCROLL);
    app.update(mouse(MouseEventKind::ScrollUp, 5, 5));
    assert_eq!(help(&app).scroll, 0);
}

/// How many lines the commands view wraps to at this size.
fn help_text_len(app: &mut App, width: u16, height: u16) -> usize {
    let at = help(app).scroll;
    let lines = whole_view(app, width, height).len();
    if let Some(Modal::Help(help)) = &mut app.modal {
        help.scroll = at;
    }
    lines
}

/// At 60x15 every word of both views is on screen in order, every row
/// fits; after a resize the scroll stays in bounds and the text rewraps.
#[test]
fn the_help_wraps_to_a_small_terminal_and_cuts_nothing_off() {
    let mut app = App::new(StatusInfo::default());
    submit(&mut app, "/help");
    for view in [HelpView::Commands, HelpView::Keys] {
        assert_eq!(help(&app).view, view);
        let shown = whole_view(&mut app, 60, 15);
        let source: String = view.text().lines().skip(1).collect::<Vec<_>>().join("\n");
        assert_eq!(words(&shown.join("\n")), words(&source), "{view:?}");
        // Descriptions continue under their column.
        assert!(
            shown
                .iter()
                .any(|l| l.starts_with(&" ".repeat(33)) && !l.trim().is_empty()),
            "{shown:#?}"
        );
        app.update(key(KeyCode::Tab));
    }
    // At its end in a wide terminal, then smaller and larger: the scroll
    // follows what the new size allows.
    app.update(key(KeyCode::End));
    drawn(&mut app, 120, 40);
    let wide_end = help(&app).scroll;
    assert!(wide_end > 0);
    app.update(key(KeyCode::End));
    let rows = drawn(&mut app, 60, 15);
    assert!(help(&app).scroll > wide_end, "more lines when narrow");
    assert!(!body(&rows).is_empty());
    let narrow_end = help(&app).scroll;
    let rows = drawn(&mut app, 120, 40);
    assert_eq!(help(&app).scroll, help(&app).scroll_max);
    assert!(help(&app).scroll < narrow_end);
    assert!(!body(&rows).is_empty());
    // A terminal too small for anything still shows a line of text in a
    // box that fits it (what fits of the footer under it).
    app.update(key(KeyCode::Home));
    let tiny = inside(&drawn(&mut app, 30, 6));
    assert_eq!(tiny.len(), 4, "{tiny:#?}");
    assert!(tiny[1].contains("/settings"), "{tiny:#?}");
}

/// The agent's output keeps arriving under the help; an approval request
/// takes the screen, and once answered the help comes back where it was.
#[test]
fn an_approval_takes_over_the_help_which_comes_back_after() {
    let mut app = App::new(StatusInfo::default());
    submit(&mut app, "hi");
    submit(&mut app, "/help");
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Down));
    let blocks = app.transcript.blocks().len();
    app.update(agent(AgentEvent::TextDelta {
        delta: "working on it".into(),
        assistant_message_id: None,
    }));
    assert!(
        app.transcript.blocks().len() > blocks,
        "the transcript goes on"
    );
    assert!(matches!(app.modal, Some(Modal::Help(_))));
    app.update(Msg::Escalation(shell_prompt(), EscalationId(1)));
    assert!(matches!(app.modal, Some(Modal::Escalation { .. })));
    let rows = drawn(&mut app, 100, 30);
    assert!(
        rows.iter().any(|r| r.contains("Approval needed")),
        "{rows:#?}"
    );
    assert!(!rows.iter().any(|r| r.contains("┌ Help ")));
    app.quiet_past_arming();
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::RespondEscalation(
            EscalationId(1),
            EscalationAnswer::AllowOnce
        )]
    );
    assert_eq!(help(&app).scroll, 2, "the help is back where it was");
    assert_eq!(help(&app).view, HelpView::Commands);
    // It came back by itself, so it takes keys once the user was quiet.
    app.quiet_past_arming();
    // A picker the user asked for earlier replaces it.
    app.update(Msg::ShowResumePicker(vec![("a".into(), "x".into())]));
    assert!(matches!(app.modal, Some(Modal::Resume(_))));
    app.update(key(KeyCode::Esc));
    app.update(key(KeyCode::Char('?')));
    app.update(Msg::ShowModelPicker(picker_models()));
    assert!(matches!(app.modal, Some(Modal::Model(_))));
}
