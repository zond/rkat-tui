//! `/btw`: the side question's block, its key rule and its refusals.

use super::*;

fn btw_block(app: &App) -> Option<(String, Option<Result<String, String>>)> {
    app.transcript.blocks().iter().rev().find_map(|b| match b {
        Block::Btw { question, answer } => Some((question.clone(), answer.clone())),
        _ => None,
    })
}

/// While a turn runs, `/btw` is asked at once (not held for the turn's
/// end, not sent to the agent), with the running tool; its block shows
/// the question, then the answer; it survives `/export` labelled as a side
/// note.
#[test]
fn btw_during_a_turn_is_asked_at_once_and_answered_in_its_block() {
    let mut app = running_app();
    app.update(agent(AgentEvent::ToolCallRequested {
        id: "t1".into(),
        name: "shell".into(),
        args: test_args(json!({"command": "sleep 100"})),
    }));
    let fx = submit(&mut app, "/btw what is it doing?");
    // With the running turn as the screen shows it (the stored history has
    // the run only once it ended), not the /btw block itself.
    assert!(
        matches!(&fx[..], [Effect::Btw { question, running_tool: Some((tool, _)), current_turn: Some(turn) }]
            if question == "what is it doing?" && tool == "shell"
                && turn.contains("## user\none") && !turn.contains("btw")),
        "{fx:?}"
    );
    assert!(app.btw_pending());
    assert_eq!(btw_block(&app), Some(("what is it doing?".into(), None)));
    assert!(render::status_line(&app.status, Instant::now()).contains("btw: asking"));
    assert!(
        app.update(Msg::BtwAnswered(Ok("it sleeps".into())))
            .is_empty()
    );
    assert!(!app.btw_pending());
    assert_eq!(
        btw_block(&app),
        Some(("what is it doing?".into(), Some(Ok("it sleeps".into()))))
    );
    assert!(app.is_running(), "the turn goes on");
    let text = app.transcript.export_text();
    assert!(
        text.contains(
            "## btw (a side question, not part of the conversation): what is it doing?\nit sleeps"
        ),
        "{text}"
    );
}

/// A pending `/btw` takes the first Esc, which cancels it and nothing
/// else; the next Esc is the usual interrupt.
#[test]
fn esc_cancels_a_pending_btw_first_and_then_interrupts() {
    let mut app = running_app();
    submit(&mut app, "/btw how far is it?");
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::CancelBtw]);
    assert!(!app.btw_pending());
    assert_eq!(
        btw_block(&app),
        Some(("how far is it?".into(), Some(Err("cancelled".into()))))
    );
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::Interrupt]);
}

/// One at a time; an empty question is a usage error; idle works too.
#[test]
fn a_second_btw_is_refused_and_an_empty_one_is_a_usage_error() {
    let mut app = App::new(StatusInfo::default());
    assert!(submit(&mut app, "/btw").is_empty());
    assert!(!app.btw_pending());
    let fx = submit(&mut app, "/btw first?");
    assert!(
        matches!(
            &fx[..],
            [Effect::Btw {
                running_tool: None,
                current_turn: None,
                ..
            }]
        ),
        "{fx:?}"
    );
    assert!(submit(&mut app, "/btw second?").is_empty());
    let last = app.transcript.blocks().last().cloned();
    assert!(
        matches!(&last, Some(Block::Notice { text, .. }) if text.contains("still waiting for its answer")),
        "{last:?}"
    );
    app.update(Msg::BtwAnswered(Err("no answer within 60s".into())));
    let text = app.transcript.export_text();
    assert!(text.contains("(no answer: no answer within 60s)"), "{text}");
    assert!(commands::commands_help().contains("/btw <question>"));
}
