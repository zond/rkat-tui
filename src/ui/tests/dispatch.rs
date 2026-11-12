//! Slash commands, pickers and the idle guard.

use super::*;

#[test]
fn clear_is_refused_while_messages_are_pending() {
    let mut app = running_app();
    submit(&mut app, "two");
    app.update(finished(Ok(summary())));
    assert_eq!(app.status.queued, 1);
    assert!(submit(&mut app, "/clear").is_empty());
    assert!(matches!(
        app.transcript.blocks().last(),
        Some(Block::Notice { level: NoticeLevel::Warn, text }) if text.contains("queued")
    ));
    assert!(!app.transcript.is_empty());
}

#[test]
fn slash_commands_map_to_effects() {
    let mut app = App::new(StatusInfo::default());
    let run = |app: &mut App, s: &str| {
        type_str(app, s);
        app.update(key(KeyCode::Enter))
    };
    assert_eq!(
        run(&mut app, "/model gpt-5.4"),
        vec![Effect::SetModel("gpt-5.4".into())]
    );
    assert_eq!(
        run(&mut app, "/auto off"),
        vec![Effect::SetMode(PermissionMode::Manual)]
    );
    assert_eq!(
        run(&mut app, "/auto on"),
        vec![Effect::SetMode(PermissionMode::Auto)]
    );
    assert_eq!(
        run(&mut app, "/mode plan"),
        vec![Effect::SetMode(PermissionMode::Plan)]
    );
    assert_eq!(
        run(&mut app, "/mode accept-edits"),
        vec![Effect::SetMode(PermissionMode::AcceptEdits)]
    );
    app.status.apply(StatusUpdate::Mode {
        mode: PermissionMode::AcceptEdits,
        paused: false,
        denials: crate::approval::desk::Denials {
            consecutive: 0,
            consecutive_limit: 3,
            total: 0,
            total_limit: 20,
        },
    });
    assert!(run(&mut app, "/mode").is_empty());
    assert!(
        last_notice(&app).is_some_and(|(_, t)| t.starts_with("permission mode: accept edits")),
        "{:?}",
        last_notice(&app)
    );
    assert_eq!(run(&mut app, "/resume"), vec![Effect::RequestResumePicker]);
    assert_eq!(
        run(&mut app, "/resume last"),
        vec![Effect::ResumeSession("last".into())]
    );
    assert_eq!(
        run(&mut app, "/permissions rm 2"),
        vec![Effect::RemoveRule(2)]
    );
    assert_eq!(run(&mut app, "/status"), vec![Effect::StatusReport]);
    app.transcript.push(Block::User { text: "hi".into() });
    let text = app.transcript.export_text();
    assert!(text.ends_with("## user\nhi\n"), "{text}");
    assert_eq!(
        run(&mut app, "/export out.txt"),
        vec![Effect::Export {
            path: Some("out.txt".into()),
            text,
        }]
    );
    // The transcript is reset only once the client runner confirms.
    app.transcript.push_notice(NoticeLevel::Info, "old");
    assert_eq!(run(&mut app, "/clear"), vec![Effect::NewSession]);
    assert!(!app.transcript.is_empty());
    app.update(Msg::ResetTranscript(Vec::new()));
    assert!(app.transcript.is_empty());
    assert!(run(&mut app, "/bogus").is_empty());
    assert!(matches!(
        app.transcript.blocks().last(),
        Some(Block::Notice { level: NoticeLevel::Warn, text }) if text.contains("/bogus")
    ));
    assert_eq!(
        run(&mut app, "//etc"),
        vec![Effect::StartTurn("/etc".into())]
    );
    // While running, /clear and /model are refused, /quit asks again.
    assert!(run(&mut app, "/clear").is_empty());
    assert!(run(&mut app, "/model gpt-5.2").is_empty());
    assert!(matches!(
        app.transcript.blocks().last(),
        Some(Block::Notice { level: NoticeLevel::Warn, text }) if *text == Busy::Turn.to_string()
    ));
    assert!(run(&mut app, "/quit").is_empty());
    assert_eq!(run(&mut app, "/quit"), vec![Effect::Quit]);
}

#[test]
fn model_is_refused_while_messages_are_pending() {
    let mut app = running_app();
    submit(&mut app, "two");
    app.update(finished(Ok(summary())));
    assert!(!app.is_running());
    assert_eq!(app.status.queued, 1);
    assert!(submit(&mut app, "/model gpt-5.2").is_empty());
    app.pending = PendingQueue::new();
    assert_eq!(
        submit(&mut app, "/model gpt-5.2"),
        vec![Effect::SetModel("gpt-5.2".into())]
    );
}

#[test]
fn resume_picker_selects() {
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::ShowResumePicker(vec![
        ("aaa".into(), "first".into()),
        ("bbb".into(), "second".into()),
    ]));
    app.update(key(KeyCode::Down));
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::ResumeSession("bbb".into())]
    );
    assert!(app.modal.is_none());
}

/// `/agents` asks for the picker; its answers become the switch, the new
/// agent and the retire effects, and the picker closes.
#[test]
fn agent_picker_switches_seats_and_retires() {
    let entry = |name: &str, mine: bool| AgentEntry {
        name: name.into(),
        run: "idle",
        conversation: None,
        title: None,
        clients: usize::from(mine),
        detached: !mine,
        mine,
        unretirable: mine.then(|| "own".to_string()),
    };
    let open = || {
        let mut app = App::new(StatusInfo::default());
        assert_eq!(submit(&mut app, "/agents"), vec![Effect::Agents(None)]);
        app.update(Msg::ShowAgentPicker(vec![
            entry("main-aaaa", true),
            entry("main-bbbb", false),
        ]));
        assert!(matches!(app.modal, Some(Modal::Agents(_))));
        app
    };
    let mut app = open();
    app.update(key(KeyCode::Down));
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::Agents(Some("main-bbbb".into()))]
    );
    assert!(app.modal.is_none());
    let mut app = open();
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Down));
    assert_eq!(app.update(key(KeyCode::Enter)), vec![Effect::NewAgent]);
    let mut app = open();
    app.update(key(KeyCode::Down));
    assert!(app.update(key(KeyCode::Char('d'))).is_empty());
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::RetireAgent("main-bbbb".into())]
    );
    assert!(app.modal.is_none());
    // `/agents <name>` stays the shortcut.
    let mut app = App::new(StatusInfo::default());
    assert_eq!(
        submit(&mut app, "/agents main-bbbb"),
        vec![Effect::Agents(Some("main-bbbb".into()))]
    );
}

/// A schedule picker as the runner builds it: `n` rows, the first paused.
fn schedules_picker(n: usize) -> ListPicker {
    use crate::daemon::protocol::ScheduleOp;
    let rows = (0..n)
        .map(|i| {
            let op = if i == 0 {
                ScheduleOp::Resume
            } else {
                ScheduleOp::Pause
            };
            let toggle = PickAct::Schedule(op, format!("s-{i}"));
            PickRow::new(
                format!("sched-{i}"),
                vec![
                    PickKey::now(KeyCode::Enter, toggle.clone()),
                    PickKey::now(KeyCode::Char('p'), toggle),
                    PickKey::confirmed(
                        KeyCode::Char('d'),
                        PickAct::Schedule(ScheduleOp::Delete, format!("s-{i}")),
                        format!("delete schedule sched-{i}?"),
                    ),
                ],
            )
        })
        .collect();
    ListPicker::new(PickerKind::Schedules, rows, "no schedules", "keys")
}

/// `/schedules` asks for the picker; its acts run as the typed command
/// and leave it open, a refresh replaces it on the same row (and is
/// dropped once it was closed); `/schedules pause <name>` stays the
/// shortcut. An approval dialog takes over and the picker (refreshed
/// meanwhile) comes back after it.
#[test]
fn schedule_picker_acts_and_refreshes_in_place() {
    use crate::daemon::protocol::ScheduleOp;
    let mut app = App::new(StatusInfo::default());
    assert_eq!(
        submit(&mut app, "/schedules"),
        vec![Effect::Schedules(None)]
    );
    app.update(Msg::ShowPicker {
        picker: schedules_picker(2),
        refresh: false,
    });
    assert!(matches!(&app.modal, Some(Modal::List(p)) if p.kind == PickerKind::Schedules));
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::PickedSchedule(ScheduleOp::Resume, "s-0".into())]
    );
    app.update(key(KeyCode::Down));
    assert_eq!(
        app.update(key(KeyCode::Char('p'))),
        vec![Effect::PickedSchedule(ScheduleOp::Pause, "s-1".into())]
    );
    assert!(app.update(key(KeyCode::Char('d'))).is_empty());
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::PickedSchedule(ScheduleOp::Delete, "s-1".into())]
    );
    // Still open; the list again replaces it on the same row.
    app.update(Msg::ShowPicker {
        picker: schedules_picker(3),
        refresh: true,
    });
    assert!(
        matches!(&app.modal, Some(Modal::List(p)) if p.rows.len() == 3 && p.cursor.index() == 1)
    );
    // An approval request takes over; a refresh meanwhile updates the
    // waiting picker, which comes back once the request is answered.
    app.update(Msg::Escalation(shell_prompt(), EscalationId(1)));
    assert!(matches!(app.modal, Some(Modal::Escalation { .. })));
    app.update(Msg::ShowPicker {
        picker: schedules_picker(1),
        refresh: true,
    });
    app.update(Msg::EscalationResolved(EscalationId(1)));
    assert!(
        matches!(&app.modal, Some(Modal::List(p)) if p.rows.len() == 1 && p.cursor.index() == 0)
    );
    // Closed (q), a late refresh does not reopen it.
    app.update(key(KeyCode::Char('q')));
    assert!(app.modal.is_none());
    app.update(Msg::ShowPicker {
        picker: schedules_picker(1),
        refresh: true,
    });
    assert!(app.modal.is_none());
    assert_eq!(
        submit(&mut app, "/schedules pause nightly"),
        vec![Effect::Schedules(Some((
            ScheduleOp::Pause,
            "nightly".into()
        )))]
    );
}

/// `/permissions` asks for the rule picker, `/permissions list` still
/// lists; the picker's confirmed `d` is the removal, and it stays open
/// for the list again.
#[test]
fn rule_picker_removes_after_a_confirmation() {
    let mut app = App::new(StatusInfo::default());
    assert_eq!(
        submit(&mut app, "/permissions"),
        vec![Effect::RequestRulePicker]
    );
    assert_eq!(
        submit(&mut app, "/permissions list"),
        vec![Effect::ListRules]
    );
    assert_eq!(
        submit(&mut app, "/permissions rm 3"),
        vec![Effect::RemoveRule(3)]
    );
    let row = PickRow::new(
        "3. allow shell (every call) · realm",
        vec![PickKey::confirmed(
            KeyCode::Char('d'),
            PickAct::RemoveRule(3),
            "remove rule 3?",
        )],
    );
    app.update(Msg::ShowPicker {
        picker: ListPicker::new(PickerKind::Permissions, vec![row], "none", "keys"),
        refresh: false,
    });
    assert!(app.update(key(KeyCode::Char('d'))).is_empty());
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::PickedRuleRemoval(3)]
    );
    assert!(matches!(&app.modal, Some(Modal::List(p)) if p.kind == PickerKind::Permissions));
    app.update(Msg::ShowPicker {
        picker: ListPicker::new(
            PickerKind::Permissions,
            vec![],
            "no approval rules yet",
            "keys",
        ),
        refresh: true,
    });
    assert!(matches!(&app.modal, Some(Modal::List(p)) if p.rows.is_empty()));
}

#[test]
fn model_command_requests_picker_or_refresh() {
    let mut app = app_on_sonnet();
    type_str(&mut app, "/model");
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::RequestModelPicker { refresh: false }]
    );
    type_str(&mut app, "/model refresh");
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::RequestModelPicker { refresh: true }]
    );
    let mut running = running_app();
    type_str(&mut running, "/model");
    assert!(running.update(key(KeyCode::Enter)).is_empty());
}

#[test]
fn model_picker_filters_and_selects() {
    let mut app = app_on_sonnet();
    app.update(Msg::ShowModelPicker(picker_models()));
    assert!(matches!(app.modal, Some(Modal::Model(_))));
    // `j`/`k`/`q` are filter characters here, not navigation.
    type_str(&mut app, "haiku");
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::SetModel("anthropic:claude-haiku-4-5".into())]
    );
    assert!(app.modal.is_none());
}

#[test]
fn model_picker_navigation_backspace_and_esc() {
    let mut app = app_on_sonnet();
    app.update(Msg::ShowModelPicker(picker_models()));
    type_str(&mut app, "zz");
    // Nothing matches: Enter keeps the picker open.
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert!(app.modal.is_some());
    app.update(key(KeyCode::Backspace));
    app.update(key(KeyCode::Backspace));
    app.update(key(KeyCode::Down));
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::SetModel("anthropic:claude-opus-4-6".into())]
    );
    // Selecting the current model changes nothing.
    app.update(Msg::ShowModelPicker(picker_models()));
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert!(app.modal.is_none());
    app.update(Msg::ShowModelPicker(picker_models()));
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert!(app.modal.is_none());
}

#[test]
fn model_picker_warning_becomes_notice_when_modal_busy() {
    let mut app = app_on_sonnet();
    app.update(Msg::ShowResumePicker(vec![("a".into(), "x".into())]));
    app.update(Msg::ShowModelPicker(picker_models_warning(Some(
        "could not list models",
    ))));
    assert!(matches!(app.modal, Some(Modal::Resume(_))));
    assert!(app.transcript.blocks().iter().any(|b| matches!(
        b,
        Block::Notice { text, .. } if text.contains("could not list models")
    )));
}

#[test]
fn memory_opens_the_file_the_client_runner_names_and_reports_back() {
    let mut app = App::new(StatusInfo::default());
    assert_eq!(
        submit(&mut app, "/memory"),
        vec![Effect::OpenMemory(MemoryTarget::Workspace)]
    );
    assert_eq!(
        submit(&mut app, "/memory system"),
        vec![Effect::OpenMemory(MemoryTarget::System)]
    );
    let mut running = running_app();
    assert_eq!(
        submit(&mut running, "/memory user"),
        vec![Effect::OpenMemory(MemoryTarget::User)]
    );
    let path = std::path::PathBuf::from("/c/SYSTEM.md");
    let target = MemoryTarget::System;
    assert!(
        app.update(Msg::EditFile {
            target,
            path: path.clone()
        })
        .is_empty()
    );
    assert_eq!(
        loop_requests(&mut app),
        vec![LocalEffect::ExternalEditor(EditorTarget::Memory {
            target,
            path: path.clone()
        })]
    );
    assert_eq!(
        app.update(Msg::EditorClosed(EditorClosed::Memory {
            target,
            path: path.clone(),
            result: Ok(())
        })),
        vec![Effect::MemoryEdited {
            target,
            path: path.clone()
        }]
    );
    assert!(
        app.update(Msg::EditorClosed(EditorClosed::Memory {
            target,
            path,
            result: Err("could not start nano: not found".into())
        }))
        .is_empty()
    );
    assert_eq!(
        last_notice(&app),
        Some((
            NoticeLevel::Warn,
            "/c/SYSTEM.md may not be saved: could not start nano: not found".into()
        ))
    );
}

#[test]
fn remember_is_allowed_while_running() {
    let mut app = running_app();
    assert_eq!(
        submit(&mut app, "#note"),
        vec![Effect::Remember("note".into())]
    );
    assert!(app.pending.is_empty());
    assert!(app.is_running());
}

#[test]
fn compact_now_refused_while_running_or_pending() {
    let mut app = running_app();
    app.status.context_used = Some(10_000);
    assert!(submit(&mut app, "/compact now").is_empty());
    assert_eq!(
        last_notice(&app),
        Some((NoticeLevel::Warn, Busy::Turn.to_string()))
    );
    submit(&mut app, "two");
    app.update(finished(Ok(summary())));
    assert!(!app.is_running());
    assert_eq!(app.status.queued, 1);
    assert!(submit(&mut app, "/compact now").is_empty());
    assert!(!app.compacting);
    assert_eq!(last_notice(&app).map(|(l, _)| l), Some(NoticeLevel::Warn));
}

/// One guard for the session-level commands: each is refused, with the
/// same notice, by everything [`App::blocker`] knows about. These were
/// the drift cases when every command spelled its own guard (`/model`
/// and `/init` ignored a running `!` command; `/clear` and `/resume`
/// ignored a message being sent next or recalled).
#[test]
fn session_commands_share_one_idle_guard() {
    let commands = [
        "/clear",
        "/resume",
        "/resume last",
        "/model",
        "/model other-model",
        "/init",
        "/compact",
    ];
    let blocked = |busy: Busy, setup: &dyn Fn(&mut App)| {
        for cmd in commands {
            let mut app = App::new(StatusInfo::default());
            app.status.context_used = Some(10_000);
            setup(&mut app);
            assert_eq!(app.blocker(), Some(busy), "{cmd}");
            let fx = submit(&mut app, cmd);
            assert!(
                !fx.iter().any(|e| matches!(
                    e,
                    Effect::NewSession
                        | Effect::RequestResumePicker
                        | Effect::ResumeSession(_)
                        | Effect::RequestModelPicker { .. }
                        | Effect::SetModel(_)
                        | Effect::StartTurn(_)
                        | Effect::CompactNow
                )),
                "{cmd} ran while {busy:?}: {fx:?}"
            );
            assert_eq!(
                last_notice(&app),
                Some((NoticeLevel::Warn, busy.to_string())),
                "{cmd}"
            );
        }
    };
    blocked(Busy::Shell, &|app| {
        assert_eq!(submit(app, "! sleep 5"), vec![run_shell(1, "sleep 5")]);
    });
    blocked(Busy::Queued, &|app| app.send_next = Some("next".into()));
    blocked(Busy::Queued, &|app| app.recalling = true);
    blocked(Busy::Compacting, &|app| app.compacting = true);
    blocked(Busy::Turn, &|app| {
        submit(app, "one");
    });
    // Idle: nothing in the way.
    let app = App::new(StatusInfo::default());
    assert_eq!(app.blocker(), None);
    assert!(!Busy::Compacting.to_string().contains("rebuil"));
}

#[test]
fn compact_status_reports_the_policy_and_compact_needs_context() {
    let mut app = gauge_app(CompactMode::Auto);
    assert!(submit(&mut app, "/compact status").is_empty());
    assert!(
        last_notice(&app).is_some_and(|(level, text)| level == NoticeLevel::Info
            && text.contains("auto-compaction is on")
            && text.contains("85k tokens"))
    );
    let mut app = gauge_app(CompactMode::Warn);
    assert!(submit(&mut app, "/compact status").is_empty());
    assert!(last_notice(&app).is_some_and(|(_, text)| text.contains("auto-compaction is off")));
    // Nothing reported yet: nothing to compact.
    for cmd in ["/compact", "/compact now", "/compact keep the plan"] {
        assert!(submit(&mut app, cmd).is_empty(), "{cmd}");
        assert_eq!(
            last_notice(&app),
            Some((NoticeLevel::Info, "nothing to compact yet".into())),
            "{cmd}"
        );
    }
    assert!(!app.compacting);
}

#[test]
fn init_sends_the_canned_prompt_as_a_user_turn_when_idle() {
    let mut app = App::new(StatusInfo::default());
    assert_eq!(
        submit(&mut app, "/init"),
        vec![Effect::StartTurn(commands::INIT_PROMPT.into())]
    );
    assert_eq!(app.transcript.blocks(), &[user(commands::INIT_PROMPT)]);
    assert!(app.is_running());
    assert!(commands::INIT_PROMPT.contains("AGENTS.md"));
    assert!(commands::INIT_PROMPT.contains("under 200 lines"));
    // While a turn runs it is refused.
    assert!(submit(&mut app, "/init").is_empty());
    assert_eq!(
        last_notice(&app),
        Some((
            NoticeLevel::Warn,
            "a turn is running; interrupt it first (Ctrl-C)".into()
        ))
    );
}

#[test]
fn context_command_sends_the_gauge_and_the_policy() {
    let mut app = gauge_app(CompactMode::Warn);
    let fx = submit(&mut app, "/context");
    let [Effect::ContextReport(header)] = fx.as_slice() else {
        panic!("{fx:?}");
    };
    assert!(
        header.starts_with("context: unknown until the first model call"),
        "{header}"
    );
    assert!(
        header.contains("\ncompaction: auto-compaction is off"),
        "{header}"
    );
    app.status.context_used = Some(42_000);
    let header = app.context_header();
    assert!(
        header.starts_with("context: 42000 of 100000 tokens (42%); window from the model profile"),
        "{header}"
    );
    app.status.context_approx = true;
    app.status.window_approx = true;
    assert!(
        app.context_header()
            .starts_with("context: 42000 of 100000 tokens (42%) (estimated); window estimated"),
        "{}",
        app.context_header()
    );
    app.status.context_window = None;
    assert!(
        app.context_header()
            .starts_with("context: 42000 tokens (estimated); window unknown")
    );
    // Allowed while a turn runs.
    let mut app = running_app();
    assert!(matches!(
        submit(&mut app, "/context").as_slice(),
        [Effect::ContextReport(_)]
    ));
}

// `/compact` used to print the policy; it now compacts like Claude
// Code's, with `now` kept as an alias and the policy under `status`.
#[test]
fn bare_compact_compacts_and_instructions_add_a_warning() {
    let mut app = App::new(StatusInfo::default());
    app.status.context_used = Some(40_000);
    assert_eq!(submit(&mut app, "/compact"), vec![Effect::CompactNow]);
    assert!(app.compacting);
    assert!(
        !app.transcript
            .blocks()
            .iter()
            .any(|b| matches!(b, Block::Notice { text, .. } if text == COMPACT_FOCUS_UNSUPPORTED))
    );
    app.update(Msg::CompactRequested(Ok(())));
    assert_eq!(
        submit(&mut app, "/compact keep the test plan"),
        vec![Effect::CompactNow]
    );
    assert!(app.transcript.blocks().iter().any(|b| matches!(
        b,
        Block::Notice { level: NoticeLevel::Warn, text } if text == COMPACT_FOCUS_UNSUPPORTED
    )));
}

#[test]
fn a_refused_request_is_reported() {
    let mut app = App::new(StatusInfo::default());
    app.status.context_used = Some(1);
    submit(&mut app, "/compact now");
    let fx = app.update(Msg::CompactRequested(Err(CompactFailure::Request(
        "boom".into(),
    ))));
    assert!(fx.is_empty());
    assert!(!app.compacting);
    assert!(!app.is_running());
    assert_eq!(
        last_notice(&app),
        Some((
            NoticeLevel::Error,
            "/compact failed: the engine did not take the request: boom".into()
        ))
    );
}

#[test]
fn auto_compact_flag_overrides_settings() {
    let settings = crate::config::AutoSettings::default();
    let args =
        <crate::cli::Args as clap::Parser>::try_parse_from(["rkat-tui", "--auto-compact"]).unwrap();
    let overlay = crate::overlay::ConfigOverlay::from_args(&args)
        .unwrap()
        .rules_file()
        .unwrap()
        .unwrap();
    let mode = overlay.auto.and_then(|auto| auto.compact);
    let app =
        App::new(StatusInfo::default()).with_compaction_policy(settings.compaction_policy(mode));
    assert_eq!(app.compaction_policy().mode, CompactMode::Auto);
    assert_eq!(
        App::new(StatusInfo::default())
            .with_compaction_policy(settings.compaction_policy(None))
            .compaction_policy()
            .mode,
        CompactMode::Native
    );
}

#[test]
fn cost_command_prints_the_breakdown() {
    let mut app = App::new(StatusInfo::default());
    assert!(submit(&mut app, "/cost").is_empty());
    let (_, text) = last_notice(&app).unwrap();
    assert!(text.contains("no tokens used"), "{text}");
    app.status.apply(StatusUpdate::Model {
        endpoint: "anthropic".into(),
        model: "a".into(),
    });
    app.update(Msg::Status(StatusUpdate::Prices(priced(&[(
        "a", 3.0, 15.0,
    )]))));
    let mut totals = crate::cost::SessionCost::default();
    totals.record(
        "a",
        &meerkat_core::Usage {
            input_tokens: 2_000_000,
            output_tokens: 100_000,
            ..meerkat_core::Usage::default()
        },
    );
    app.update(Msg::Status(StatusUpdate::CostTotals(totals)));
    assert!(submit(&mut app, "/cost").is_empty());
    let (level, text) = last_notice(&app).unwrap();
    assert_eq!(level, NoticeLevel::Info);
    assert!(text.contains("a: in 2000000 · out 100000"), "{text}");
    assert!(text.contains("$7.50 (price: tui.toml)"), "{text}");
    assert!(text.contains("total: $7.50"), "{text}");
}

/// An idle app attached to a daemon.
fn attached_app() -> App {
    let mut status = StatusInfo::default();
    status.apply(StatusUpdate::Connection(crate::ui::Connection::Attached {
        address: "127.0.0.1:1".into(),
        daemon_build: None,
    }));
    App::new(status)
}

/// `/rehome` asks the runner, and the UI stays: a re-home re-roots it
/// (`@` completion in the new workspace, the history of the realm) and
/// the UI goes on.
#[test]
fn rehome_asks_the_runner_and_a_reroot_moves_the_ui() {
    let mut app = attached_app();
    assert_eq!(submit(&mut app, "/rehome"), vec![Effect::Rehome(None)]);
    assert_eq!(
        submit(&mut app, "/rehome /new/place"),
        vec![Effect::Rehome(Some("/new/place".into()))]
    );
    let _ = app.take_local();
    let tmp = tempfile::tempdir().unwrap();
    let workspace = crate::paths::ProjectRoot::new(tmp.path()).unwrap();
    assert_eq!(
        app.update(Msg::Rerooted {
            workspace,
            realm: tmp.path().join("realm"),
        }),
        Vec::new()
    );
    assert!(
        app.take_local()
            .contains(&LocalEffect::Reroot(tmp.path().join("realm"))),
    );
}

#[test]
fn an_attached_quit_asks_the_runner_and_follows_its_decision() {
    let mut app = attached_app();
    assert_eq!(submit(&mut app, "/quit"), vec![Effect::DecideQuit]);
    assert_eq!(
        app.update(Msg::QuitDecided(QuitDecision::Terminate)),
        // The daemon stopped itself (`rkat/quit`): nothing left to say.
        vec![Effect::Quit]
    );
    // Other clients attached: just leave.
    let mut app = attached_app();
    submit(&mut app, "/quit");
    assert_eq!(
        app.update(Msg::QuitDecided(QuitDecision::Leave)),
        vec![Effect::Quit]
    );
    // A decision nobody asked for is dropped.
    let mut app = attached_app();
    assert!(app.update(Msg::QuitDecided(QuitDecision::Leave)).is_empty());
    // A second /quit while the runner decides leaves at once (a detach).
    assert_eq!(submit(&mut app, "/quit"), vec![Effect::DecideQuit]);
    assert_eq!(submit(&mut app, "/quit"), vec![Effect::Quit]);
    // With no connection state reported, /quit quits.
    let mut app = App::new(StatusInfo::default());
    assert_eq!(submit(&mut app, "/quit"), vec![Effect::Quit]);
}

#[test]
fn the_quit_dialog_detaches_by_default_terminates_on_request_and_esc_stays() {
    let ask = || QuitDecision::Ask {
        reason: "a client detached from it earlier".into(),
        running: true,
    };
    let mut app = attached_app();
    submit(&mut app, "/quit");
    assert!(app.update(Msg::QuitDecided(ask())).is_empty());
    let Some(Modal::Quit(dialog)) = &app.modal else {
        panic!("no quit dialog: {:?}", app.modal);
    };
    assert!(dialog.running && dialog.cursor == 0);
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::Leave(Departure::Detach), Effect::Quit]
    );
    let mut app = attached_app();
    submit(&mut app, "/quit");
    app.update(Msg::QuitDecided(ask()));
    app.update(key(KeyCode::Down));
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::Leave(Departure::Terminate), Effect::Quit]
    );
    let mut app = attached_app();
    submit(&mut app, "/quit");
    app.update(Msg::QuitDecided(ask()));
    assert_eq!(
        app.update(key(KeyCode::Char('t'))),
        vec![Effect::Leave(Departure::Terminate), Effect::Quit]
    );
    let mut app = attached_app();
    submit(&mut app, "/quit");
    app.update(Msg::QuitDecided(ask()));
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert!(app.modal.is_none());
    // And /quit asks the runner again.
    assert_eq!(submit(&mut app, "/quit"), vec![Effect::DecideQuit]);
}

#[test]
fn unsent_messages_are_saved_whichever_way_an_attached_quit_leaves() {
    let mut status = StatusInfo::default();
    status.apply(StatusUpdate::Connection(crate::ui::Connection::Attached {
        address: "127.0.0.1:1".into(),
        daemon_build: None,
    }));
    let mut app = App::new(status);
    assert_eq!(
        submit(&mut app, "one"),
        vec![Effect::StartTurn("one".into())]
    );
    submit(&mut app, "two");
    // Busy: the first /quit warns, the second asks the runner.
    assert!(submit(&mut app, "/quit").is_empty());
    assert_eq!(submit(&mut app, "/quit"), vec![Effect::DecideQuit]);
    let effects = app.update(Msg::QuitDecided(QuitDecision::Terminate));
    assert!(
        matches!(effects.as_slice(), [Effect::SaveUnsent(held), Effect::Quit] if held.len() == 1),
        "{effects:?}"
    );
}

#[test]
fn detach_leaves_attached_and_says_nothing_to_detach_from_in_process() {
    let mut app = attached_app();
    assert_eq!(
        submit(&mut app, "/detach"),
        vec![Effect::Leave(Departure::Detach), Effect::Quit]
    );
    let mut app = App::new(StatusInfo::default());
    assert!(submit(&mut app, "/detach").is_empty());
    assert!(matches!(
        app.transcript.blocks().last(),
        Some(Block::Notice { text, .. }) if text.starts_with("nothing to detach from")
    ));
}

#[test]
fn a_hangup_leaves_without_stopping_the_daemon() {
    let mut app = attached_app();
    assert_eq!(app.update(Msg::Hangup), vec![Effect::Quit]);
    // Ctrl-C twice is /quit: attached, it asks the runner.
    let mut app = attached_app();
    let t0 = Instant::now();
    assert!(app.update_at(ctrl('c'), t0).is_empty());
    assert_eq!(app.update_at(ctrl('c'), t0), vec![Effect::DecideQuit]);
}
