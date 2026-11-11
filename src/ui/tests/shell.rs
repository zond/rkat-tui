//! `!` shell mode.

use super::*;

#[test]
fn bang_runs_a_command_and_escapes_send_text() {
    let mut app = App::new(StatusInfo::default());
    assert_eq!(submit(&mut app, "! ls -a"), vec![run_shell(1, "ls -a")]);
    assert!(!app.is_running());
    assert!(matches!(
        app.transcript.blocks().last(),
        Some(Block::Shell { command, state: ShellState::Running { .. }, .. }) if command == "ls -a"
    ));
    // Esc cancels it.
    assert_eq!(app.update(key(KeyCode::Esc)), vec![Effect::CancelShell]);
    assert!(app.update(shell_done(1, "a\nb\n")).is_empty());
    assert!(matches!(
        app.transcript.blocks().last(),
        Some(Block::Shell { state: ShellState::Done { .. }, output: Some(out), collapsed: false, .. })
            if out == "a\nb"
    ));
    // A late answer for a finished run changes nothing.
    assert!(app.update(shell_done(1, "again")).is_empty());
    // A bare ! runs nothing.
    assert!(submit(&mut app, "!").is_empty());
    assert!(matches!(
        app.transcript.blocks().last(),
        Some(Block::Notice { .. })
    ));
    let mut app = App::new(StatusInfo::default());
    assert_eq!(
        submit(&mut app, "\\!important"),
        vec![Effect::StartTurn("!important".into())]
    );
    let mut app = App::new(StatusInfo::default());
    assert_eq!(
        submit(&mut app, "!! wow"),
        vec![Effect::StartTurn("!! wow".into())]
    );
}

#[test]
fn shell_mode_shows_in_the_editor_title() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "!ls");
    assert!(app.shell_mode());
    assert!(app.shell_mode_hint().contains("as you, no sandbox"));
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "!!ls");
    assert!(!app.shell_mode());
}

#[test]
fn commands_typed_during_a_turn_run_after_it_before_the_queued_messages() {
    let mut app = running_app();
    assert!(submit(&mut app, "!echo a").is_empty());
    assert_eq!(submit(&mut app, "two"), vec![steer(2, "two")]);
    app.update(ack(2, Err(SteerFailure::NotRunning)));
    assert!(submit(&mut app, "!echo b").is_empty());
    assert_eq!(
        app.transcript.blocks(),
        &[
            user("one"),
            queued_block(1, "$ echo a", QueuedLabel::Shell),
            queued_block(2, "two", QueuedLabel::Deferred),
            queued_block(3, "$ echo b", QueuedLabel::Shell),
        ]
    );
    // The turn ends with "two" held: the commands run first, in
    // order, each in place of its placeholder.
    assert_eq!(
        app.update(finished(Ok(summary()))),
        vec![run_shell(1, "echo a")]
    );
    assert!(!app.is_running());
    assert!(
        matches!(app.transcript.block(BlockId(1)), Some(Block::Shell { command, .. }) if command == "echo a")
    );
    assert_eq!(
        app.transcript.block(BlockId(2)),
        Some(&queued_block(2, "two", QueuedLabel::Deferred))
    );
    // A message typed meanwhile waits too.
    assert!(submit(&mut app, "four").is_empty());
    assert_eq!(
        app.update(shell_done(1, "a\n")),
        vec![run_shell(2, "echo b")]
    );
    // Then the messages go, as one turn.
    assert_eq!(
        app.update(shell_done(2, "b\n")),
        vec![Effect::StartTurn("two\n\nfour".into())]
    );
    assert!(app.is_running());
    assert!(app.pending.is_empty());
}

#[test]
fn up_takes_a_held_command_back() {
    let mut app = running_app();
    submit(&mut app, "!make lint");
    assert!(app.can_take_back());
    assert!(app.update(key(KeyCode::Up)).is_empty());
    assert_eq!(app.input.text(), "!make lint");
    assert!(app.pending.is_empty());
    assert_eq!(app.transcript.blocks(), &[user("one")]);
    // Nothing is left to run after the turn.
    app.input.clear();
    assert!(app.update(finished(Ok(summary()))).is_empty());
}
