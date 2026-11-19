//! The `/permissions` dialog: its two sections (Blocked, In force), the
//! switch between them and which one it opens on, every key of each, the
//! new statement (`n`) and the new rule (`r`).

use super::dispatch::blocked_call;
use super::*;
use crate::approval::desk::RuleRow;
use crate::approval::grants::{BlockedCall, GrantOrigin, GrantRow, GrantScope, GrantTarget};
use crate::approval::rules::RuleOrigin;
use crate::approval::statements::{StatementOrigin, StatementRow};

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::from(code)
}

/// The dialog over `calls` alone, opened as `/permissions` opens it.
fn blocked(calls: &[BlockedCall], now: u64) -> ListPicker {
    permissions_dialog(calls, now, &[], &[], &[], None)
}

/// The dialog over the rules, grants and statements alone.
fn in_force(rules: &[RuleRow], grants: &[GrantRow], statements: &[StatementRow]) -> ListPicker {
    permissions_dialog(&[], 0, rules, grants, statements, None)
}

fn question(picker: &ListPicker) -> String {
    match &picker.line {
        Some(PickerLine::Confirm { question, .. }) => question.clone(),
        other => format!("not a question: {other:?}"),
    }
}

fn session_grant(id: u64) -> GrantRow {
    GrantRow {
        id,
        target: GrantTarget::Dir {
            path: "/data/store".into(),
        },
        origin: GrantOrigin::Session,
        removable: true,
    }
}

/// The dialog open in `app`.
fn open(app: &App) -> &ListPicker {
    match &app.modal {
        Some(Modal::List(picker)) if picker.kind == PickerKind::Permissions => picker,
        other => panic!("the dialog is not open: {other:?}"),
    }
}

/// It opens on Blocked when a call is blocked or an approval waits, else
/// on In force, or on the section asked for; the title names both with
/// their counts and marks the one shown; Tab and Shift+Tab switch, each
/// section keeping its highlight; the footers name the switch.
#[test]
fn the_dialog_opens_on_the_right_section_and_tab_switches() {
    let calls = [blocked_call(1, false), blocked_call(2, false)];
    let grants = [session_grant(4)];
    let mut p = permissions_dialog(&calls, 160, &[], &grants, &[], None);
    assert_eq!(p.section(), Some(Section::Blocked.index()));
    assert_eq!(p.title(), " Permissions: [Blocked 2] · In force 1 ");
    assert!(
        p.footer.ends_with("· Tab In force · Esc close"),
        "{}",
        p.footer
    );
    // Only a waiting approval: still Blocked.
    let waiting = permissions_dialog(&[blocked_call(3, true)], 160, &[], &grants, &[], None);
    assert_eq!(waiting.section(), Some(Section::Blocked.index()));
    // Nothing blocked: In force; asked for: that one.
    let none = permissions_dialog(&[], 160, &[], &grants, &[], None);
    assert_eq!(none.section(), Some(Section::InForce.index()));
    assert_eq!(none.title(), " Permissions: Blocked 0 · [In force 1] ");
    let asked = permissions_dialog(&[], 160, &[], &grants, &[], Some(Section::Blocked));
    assert_eq!(asked.section(), Some(Section::Blocked.index()));
    assert!(asked.rows.is_empty() && asked.empty.contains("nothing was blocked"));
    let asked = permissions_dialog(&calls, 160, &[], &grants, &[], Some(Section::InForce));
    assert_eq!(asked.section(), Some(Section::InForce.index()));
    // Tab switches, the highlight of each section kept.
    p.key(&press(KeyCode::Down));
    assert_eq!(p.key(&press(KeyCode::Tab)), ModalKey::Stay);
    assert_eq!(p.section(), Some(Section::InForce.index()));
    assert_eq!(p.title(), " Permissions: Blocked 2 · [In force 1] ");
    assert!(
        p.rows[0].text.starts_with("grant writes under"),
        "{:?}",
        p.rows
    );
    assert!(
        p.footer.ends_with("· Tab Blocked · Esc close"),
        "{}",
        p.footer
    );
    p.key(&press(KeyCode::BackTab));
    assert_eq!(p.section(), Some(Section::Blocked.index()));
    assert_eq!(p.cursor.index(), 1);
    assert!(p.selected().unwrap().text.starts_with("#1 "));
    // `q`, Esc and Ctrl-C close from either section.
    assert_eq!(p.key(&press(KeyCode::Char('q'))), ModalKey::Close);
    p.key(&press(KeyCode::Tab));
    assert_eq!(p.key(&press(KeyCode::Esc)), ModalKey::Close);
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(p.key(&ctrl_c), ModalKey::Close);
}

/// Tab switches sections only while no question or field is open: an
/// open question is answered first (Tab only points at y and n), and in
/// a statement's field Tab switches its scope.
#[test]
fn tab_is_the_field_s_or_the_question_s_while_one_is_open() {
    let calls = [BlockedCall {
        grant: Some(GrantTarget::Dir {
            path: "/data/store".into(),
        }),
        ..blocked_call(1, false)
    }];
    let mut p = permissions_dialog(&calls, 160, &[], &[], &[], None);
    p.key(&press(KeyCode::Char('s')));
    p.key(&press(KeyCode::Tab));
    assert_eq!(p.section(), Some(Section::Blocked.index()));
    assert!(matches!(
        &p.line,
        Some(PickerLine::Confirm { nudged: true, .. })
    ));
    p.key(&press(KeyCode::Char('n')));
    p.key(&press(KeyCode::Char('e')));
    assert!(p.editing());
    p.key(&press(KeyCode::Tab));
    assert_eq!(p.section(), Some(Section::Blocked.index()));
    assert!(matches!(
        &p.line,
        Some(PickerLine::Field(Field {
            what: FieldFor::Effect {
                scope: GrantScope::Always,
                ..
            },
            ..
        }))
    ));
    p.key(&press(KeyCode::Esc));
    p.key(&press(KeyCode::Tab));
    assert_eq!(p.section(), Some(Section::InForce.index()));
    p.key(&press(KeyCode::Char('n')));
    p.key(&press(KeyCode::BackTab));
    assert_eq!(p.section(), Some(Section::InForce.index()));
    assert!(matches!(
        &p.line,
        Some(PickerLine::Field(Field {
            what: FieldFor::Statement {
                scope: GrantScope::Always
            },
            ..
        }))
    ));
}

/// A key of the other section says where it acts, and acts on nothing.
#[test]
fn a_key_of_the_other_section_says_where_it_acts() {
    let mut p = permissions_dialog(&[blocked_call(1, false)], 160, &[], &[], &[], None);
    for code in ['d', 'n', 'r'] {
        assert_eq!(p.key(&press(KeyCode::Char(code))), ModalKey::Stay);
        assert!(
            matches!(&p.line, Some(PickerLine::Refused(why)) if why.contains("In force")),
            "{code}: {:?}",
            p.line
        );
    }
    p.key(&press(KeyCode::Tab));
    for code in ['o', 's', 'a', 'A', 'e', 'w'] {
        assert_eq!(p.key(&press(KeyCode::Char(code))), ModalKey::Stay);
        assert!(
            matches!(&p.line, Some(PickerLine::Refused(why)) if why.contains("Tab switches to Blocked")),
            "{code}: {:?}",
            p.line
        );
    }
}

/// In force: a row per rule with its number, kind, tool, pattern or
/// literal and layer; `d` asks before removing it by its number, saying
/// for good for the workspace's own rules and until the daemon restarts
/// for one written in another layer; Enter does nothing; with none, the
/// empty line says how to add.
#[test]
fn in_force_rules_remove_by_number_after_saying_for_how_long() {
    let rule = |n, deny, arg: Option<&str>, literal, origin| RuleRow {
        n,
        deny,
        tool: "shell".into(),
        arg: arg.map(str::to_string),
        literal,
        origin,
    };
    let mut picker = in_force(
        &[
            rule(1, false, Some("git status*"), false, RuleOrigin::Realm),
            rule(4, true, Some("git push"), true, RuleOrigin::Global),
            rule(7, false, None, false, RuleOrigin::Cli),
        ],
        &[],
        &[],
    );
    let texts: Vec<&str> = picker.rows.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "1. allow shell pattern `git status*` · realm",
            "4. deny shell literal `git push` · global",
            "7. allow shell (every call) · cli",
        ]
    );
    assert_eq!(picker.key(&press(KeyCode::Enter)), ModalKey::Stay);
    picker.key(&press(KeyCode::Char('d')));
    assert!(
        question(&picker).contains("for good"),
        "{}",
        question(&picker)
    );
    assert_eq!(
        picker.key(&press(KeyCode::Char('y'))),
        ModalKey::Choose(PickAct::RemoveRule(1))
    );
    picker.key(&press(KeyCode::Down));
    picker.key(&press(KeyCode::Char('d')));
    let asked = question(&picker);
    assert!(
        asked.contains("your own tui.toml") && asked.contains("removed from that file"),
        "{asked}"
    );
    assert_eq!(
        picker.key(&press(KeyCode::Char('y'))),
        ModalKey::Choose(PickAct::RemoveRule(4))
    );
    picker.key(&press(KeyCode::Down));
    picker.key(&press(KeyCode::Char('d')));
    assert!(
        question(&picker).contains("command line"),
        "{}",
        question(&picker)
    );
    let none = in_force(&[], &[], &[]);
    assert!(none.rows.is_empty());
    assert!(
        none.empty.contains("n adds a statement, r a rule"),
        "{}",
        none.empty
    );
    assert!(
        none.footer.contains("n new statement · r new rule"),
        "{}",
        none.footer
    );
}

/// In force lists the grants with where each comes from; only a session
/// grant can be removed there, the others say where to drop them.
#[test]
fn in_force_lists_grants_and_removes_session_ones() {
    let grant = |id, origin: GrantOrigin, removable| GrantRow {
        id,
        target: GrantTarget::Dir {
            path: format!("/data/{}", origin.word()).into(),
        },
        origin,
        removable,
    };
    let mut picker = in_force(
        &[],
        &[
            grant(0, GrantOrigin::Default, false),
            grant(0, GrantOrigin::Flag, false),
            grant(5, GrantOrigin::Always, false),
            grant(9, GrantOrigin::Session, true),
        ],
        &[],
    );
    let texts: Vec<&str> = picker.rows.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "grant writes under `/data/default` · default",
            "grant writes under `/data/flag` · flag",
            "grant writes under `/data/always` · always",
            "grant writes under `/data/session` · session",
        ]
    );
    picker.key(&press(KeyCode::Char('d')));
    assert!(matches!(&picker.line, Some(PickerLine::Refused(why)) if why.contains("by default")));
    for _ in 0..3 {
        picker.key(&press(KeyCode::Down));
    }
    picker.key(&press(KeyCode::Char('d')));
    assert_eq!(
        picker.key(&press(KeyCode::Char('y'))),
        ModalKey::Choose(PickAct::RemoveGrant(9))
    );
}

/// In force lists the statements with their origin; `d` asks before
/// removing a session or always one, and says to edit the file for one of
/// the user's own `tui.toml`.
#[test]
fn in_force_lists_and_removes_statements() {
    let row = |id, text: &str, origin| StatementRow {
        id,
        text: text.into(),
        origin,
    };
    let mut picker = in_force(
        &[],
        &[],
        &[
            row(
                1,
                "network requests to api.example.com",
                StatementOrigin::Config,
            ),
            row(4, "pushing to main", StatementOrigin::Always),
            row(
                7,
                "building after a dependency change",
                StatementOrigin::Session,
            ),
        ],
    );
    let texts: Vec<&str> = picker.rows.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "allow kind \"network requests to api.example.com\" · config",
            "allow kind \"pushing to main\" · always",
            "allow kind \"building after a dependency change\" · session",
        ]
    );
    assert!(
        picker.rows[0]
            .note
            .as_deref()
            .unwrap()
            .contains("judged by the classifier")
    );
    picker.key(&press(KeyCode::Char('d')));
    assert!(
        matches!(&picker.line, Some(PickerLine::Refused(why)) if why.contains("your own tui.toml"))
    );
    picker.key(&press(KeyCode::Down));
    picker.key(&press(KeyCode::Char('d')));
    assert!(
        matches!(&picker.line, Some(PickerLine::Confirm { question, .. }) if question.contains("leaves the realm's tui.toml"))
    );
    assert_eq!(
        picker.key(&press(KeyCode::Char('y'))),
        ModalKey::Choose(PickAct::RemoveStatement(4))
    );
}

/// Blocked lists the calls newest first; Enter and `o` approve once, `s`,
/// `a` and `A` ask before a session or lasting grant, and a call no grant
/// fits says so.
#[test]
fn blocked_offers_once_session_and_always() {
    let call = |id, grant| BlockedCall {
        id,
        tool: "shell".into(),
        helper: None,
        summary: format!("cmd {id}"),
        rule: Some("T3".into()),
        path: None,
        reason: "the safety classifier blocked it: x".into(),
        at: 100,
        effect: (id == 2).then(|| "pushing to the default branch".to_string()),
        grant,
        approved_once: false,
        call: format!("cmd {id}"),
        call_bytes: None,
    };
    let mut picker = blocked(
        &[
            call(1, None),
            call(
                2,
                Some(GrantTarget::Dir {
                    path: "/data/store".into(),
                }),
            ),
        ],
        160,
    );
    assert!(
        picker.rows[0]
            .text
            .starts_with("#2 shell: cmd 2 [T3] · 1m ago"),
        "{}",
        picker.rows[0].text
    );
    assert_eq!(
        picker.key(&press(KeyCode::Enter)),
        ModalKey::Choose(PickAct::Approve(2, GrantScope::Once))
    );
    assert_eq!(
        picker.key(&press(KeyCode::Char('o'))),
        ModalKey::Choose(PickAct::Approve(2, GrantScope::Once))
    );
    picker.key(&press(KeyCode::Char('s')));
    assert_eq!(
        question(&picker),
        "Grant writes under `/data/store` for this session?"
    );
    // The owner's case: Enter while the question is up approves nothing
    // (not even once) and leaves the question for y or n.
    assert_eq!(picker.key(&press(KeyCode::Enter)), ModalKey::Stay);
    assert!(matches!(
        &picker.line,
        Some(PickerLine::Confirm { nudged: true, .. })
    ));
    let lines = crate::ui::picker_lines_for_test(&picker, 200);
    assert!(
        lines
            .iter()
            .any(|l| l == "Grant writes under `/data/store` for this session? y yes · n or Esc no"),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l == "press y to confirm, n or Esc to cancel"),
        "{lines:?}"
    );
    assert_eq!(
        picker.key(&press(KeyCode::Char('y'))),
        ModalKey::Choose(PickAct::Approve(2, GrantScope::Session))
    );
    picker.key(&press(KeyCode::Char('a')));
    assert!(question(&picker).contains("written to the realm's tui.toml"));
    assert_eq!(
        picker.key(&press(KeyCode::Char('y'))),
        ModalKey::Choose(PickAct::Approve(2, GrantScope::Always))
    );
    picker.key(&press(KeyCode::Char('A')));
    assert!(question(&picker).contains("in every workspace (written to your own tui.toml)"));
    assert_eq!(
        picker.key(&press(KeyCode::Char('y'))),
        ModalKey::Choose(PickAct::Approve(2, GrantScope::AlwaysUser))
    );
    picker.key(&press(KeyCode::Down));
    picker.key(&press(KeyCode::Char('a')));
    assert!(
        matches!(&picker.line, Some(PickerLine::Refused(why)) if why.contains("no grant fits"))
    );
    let empty = blocked(&[], 0);
    assert!(empty.rows.is_empty());
}

/// `e` opens the one-line field with the call's kind of effect (empty
/// when the classifier named none): typing edits it, a paste stays on one
/// line, Tab switches to always and back, Esc keeps everything, Enter
/// allows the statement as edited. The rows say which choices are exact
/// and which the classifier judges.
#[test]
fn blocked_allows_an_edited_kind_of_effect() {
    let call = |id, effect: Option<&str>| BlockedCall {
        id,
        tool: "shell".into(),
        helper: None,
        summary: "git push origin main".into(),
        rule: Some("R1".into()),
        path: None,
        reason: "blocked".into(),
        at: 100,
        effect: effect.map(str::to_string),
        grant: None,
        approved_once: false,
        call: "git push origin main".into(),
        call_bytes: None,
    };
    let mut picker = blocked(
        &[
            call(1, None),
            call(2, Some("pushing to the default branch")),
        ],
        100,
    );
    assert_eq!(
        picker.footer,
        "Enter approve once · s grant this session · a grant always here · A always in every workspace · e allow this kind of effect · Tab In force · Esc close"
    );
    assert!(
        picker.rows[0]
            .note
            .as_deref()
            .unwrap()
            .ends_with("e judged: \"pushing to the default branch\""),
        "{:?}",
        picker.rows[0].note
    );
    assert_eq!(picker.key(&press(KeyCode::Char('e'))), ModalKey::Stay);
    assert!(picker.editing());
    for c in " of my own repo".chars() {
        picker.key(&press(KeyCode::Char(c)));
    }
    picker.type_text("\nnow");
    picker.key(&press(KeyCode::Backspace));
    picker.key(&press(KeyCode::Tab));
    let lines = crate::ui::picker_lines_for_test(&picker, 200);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("allow this kind of action always")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("> pushing to the default branch of my own repo no")),
        "{lines:?}"
    );
    assert_eq!(
        picker.key(&press(KeyCode::Enter)),
        ModalKey::Choose(PickAct::AllowEffect {
            id: 2,
            scope: GrantScope::Always,
            text: "pushing to the default branch of my own repo no".into(),
        })
    );
    assert!(!picker.editing());
    // A block without a sentence: the field starts empty; Esc keeps
    // things, the dialog stays open.
    picker.key(&press(KeyCode::Down));
    picker.key(&press(KeyCode::Char('e')));
    assert!(matches!(
        &picker.line,
        Some(PickerLine::Field(Field {
            what: FieldFor::Effect {
                id: 1,
                scope: GrantScope::Session
            },
            text
        })) if text.is_empty()
    ));
    assert!(
        picker.rows[1]
            .note
            .as_deref()
            .unwrap()
            .contains("a kind of effect you type")
    );
    assert_eq!(picker.key(&press(KeyCode::Esc)), ModalKey::Stay);
    assert_eq!(picker.line, None);
}

/// `n`: an empty field for a new statement; Tab goes through this
/// session, always here and every workspace (the heading says which);
/// it takes at most 200 characters on one line; Enter allows it (the
/// dialog stays open, and what was done, or the daemon's refusal, shows
/// under the rows).
#[test]
fn n_types_a_new_statement_for_the_scope_chosen() {
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::ShowPicker {
        picker: in_force(&[], &[], &[]),
        refresh: false,
    });
    assert!(app.update(key(KeyCode::Char('n'))).is_empty());
    let heading = |app: &App| {
        crate::ui::picker_lines_for_test(open(app), 300)
            .into_iter()
            .find(|l| l.starts_with("new statement:"))
            .unwrap_or_default()
    };
    assert!(
        heading(&app).contains("for this session")
            && heading(&app).contains("Tab: always for this workspace"),
        "{}",
        heading(&app)
    );
    app.update(key(KeyCode::Tab));
    assert!(
        heading(&app).contains("always for this workspace (the realm's tui.toml)"),
        "{}",
        heading(&app)
    );
    app.update(key(KeyCode::Tab));
    assert!(
        heading(&app).contains("always in every workspace (your tui.toml)"),
        "{}",
        heading(&app)
    );
    app.update(key(KeyCode::Tab));
    assert!(
        heading(&app).contains("for this session"),
        "{}",
        heading(&app)
    );
    app.update(key(KeyCode::Tab));
    // The cap: a paste of more than 200 characters keeps 200.
    app.update(Msg::Paste("x".repeat(250)));
    let Some(PickerLine::Field(field)) = &open(&app).line else {
        panic!("the field is open")
    };
    assert_eq!(
        field.text.chars().count(),
        crate::approval::statements::MAX_STATEMENT_CHARS
    );
    for _ in 0..crate::approval::statements::MAX_STATEMENT_CHARS {
        app.update(key(KeyCode::Backspace));
    }
    type_str(&mut app, "pushing to my own fork");
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::PickedStatement {
            scope: GrantScope::Always,
            text: "pushing to my own fork".into(),
        }]
    );
    assert!(!open(&app).editing());
    // The daemon's refusal lands on the dialog, which stays open.
    app.update(Msg::PickerRefused {
        kind: PickerKind::Permissions,
        why: "a statement must name a kind of action, not \"anything\"".into(),
    });
    assert!(matches!(&open(&app).line, Some(PickerLine::Refused(why)) if why.contains("anything")));
    app.update(Msg::PickerDone {
        kind: PickerKind::Permissions,
        text: "allowed always for this workspace: \"pushing to my own fork\"".into(),
    });
    assert!(matches!(&open(&app).line, Some(PickerLine::Done(what)) if what.contains("pushing")));
}

/// `r`: a form for a new rule on the dialog's line, `<tool> [pattern]`
/// typed; Tab switches allow and deny, Shift+Tab this workspace and your
/// own tui.toml, ↑/↓ pattern and literal, the heading says each; Enter
/// adds it; no tool, or a literal with nothing to match, is refused on
/// the line and adds nothing.
#[test]
fn r_adds_a_rule_from_a_small_form() {
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::ShowPicker {
        picker: in_force(&[], &[], &[]),
        refresh: false,
    });
    let heading = |app: &App| {
        crate::ui::picker_lines_for_test(open(app), 300)
            .into_iter()
            .find(|l| l.starts_with("new rule:"))
            .unwrap_or_default()
    };
    app.update(key(KeyCode::Char('r')));
    assert!(
        heading(&app).starts_with(
            "new rule: allow · this workspace (the realm's tui.toml) · type <tool> [pattern]"
        ),
        "{}",
        heading(&app)
    );
    // Enter with nothing typed: refused, nothing added.
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert!(
        matches!(&open(&app).line, Some(PickerLine::Refused(why)) if why.contains("type the tool's name"))
    );
    app.update(key(KeyCode::Char('r')));
    type_str(&mut app, "shell cargo test *");
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::PickedRule(RuleForm {
            deny: false,
            tool: "shell".into(),
            pattern: Some("cargo test *".into()),
            literal: false,
            user: false,
        })]
    );
    // Deny, in your own file, a literal.
    app.update(key(KeyCode::Char('r')));
    app.update(key(KeyCode::Tab));
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::BackTab,
        KeyModifiers::SHIFT,
    )));
    app.update(key(KeyCode::Down));
    assert!(
        heading(&app).starts_with(
            "new rule: deny · your tui.toml (every workspace) · type <tool> <exact text>"
        ),
        "{}",
        heading(&app)
    );
    type_str(&mut app, "shell");
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert!(
        matches!(&open(&app).line, Some(PickerLine::Refused(why)) if why.contains("literal rule needs the exact text"))
    );
    app.update(key(KeyCode::Char('r')));
    app.update(key(KeyCode::Tab));
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::BackTab,
        KeyModifiers::SHIFT,
    )));
    app.update(key(KeyCode::Up));
    type_str(&mut app, "shell git push --force");
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::PickedRule(RuleForm {
            deny: true,
            tool: "shell".into(),
            pattern: Some("git push --force".into()),
            literal: true,
            user: true,
        })]
    );
    // Esc leaves the form, a second Esc the dialog.
    app.update(key(KeyCode::Char('r')));
    app.update(key(KeyCode::Esc));
    assert!(!open(&app).editing());
    app.update(key(KeyCode::Esc));
    assert!(app.modal.is_none());
}

/// Approving a blocked call that makes a grant: the dialog stays open on
/// Blocked, on the next call; the grant shows in In force at once (its
/// count in the title, its row after Tab), and the line says what was
/// done.
#[test]
fn an_approval_shows_in_in_force_and_the_dialog_stays_open() {
    let with_grant = |id| BlockedCall {
        grant: Some(GrantTarget::Dir {
            path: "/data/store".into(),
        }),
        ..blocked_call(id, false)
    };
    let mut app = App::new(StatusInfo::default());
    app.update(Msg::ShowPicker {
        picker: permissions_dialog(&[with_grant(1), with_grant(2)], 160, &[], &[], &[], None),
        refresh: false,
    });
    app.update(key(KeyCode::Char('s')));
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::PickedApproval(2, GrantScope::Session, None)]
    );
    assert_eq!(open(&app).section(), Some(Section::Blocked.index()));
    // The daemon's lists again: the call is gone, its grant in force.
    app.update(Msg::ShowPicker {
        picker: permissions_dialog(&[with_grant(1)], 170, &[], &[session_grant(7)], &[], None),
        refresh: true,
    });
    app.update(Msg::PickerDone {
        kind: PickerKind::Permissions,
        text: "granted for this session: writes under `/data/store`".into(),
    });
    let p = open(&app);
    assert_eq!(p.section(), Some(Section::Blocked.index()));
    assert_eq!(p.title(), " Permissions: [Blocked 1] · In force 1 ");
    assert!(p.selected().unwrap().text.starts_with("#1 "));
    assert!(matches!(&p.line, Some(PickerLine::Done(what)) if what.starts_with("granted")));
    let rows = screen(&mut app, 30).join("\n");
    assert!(
        rows.contains("Permissions: [Blocked 1] · In force 1"),
        "{rows}"
    );
    assert!(rows.contains("granted for this session"), "{rows}");
    app.update(key(KeyCode::Tab));
    assert_eq!(
        open(&app).rows[0].text,
        "grant writes under `/data/store` · session"
    );
    // A refresh while In force shows keeps In force, even with calls
    // blocked (the opening rule is for opening only).
    app.update(Msg::ShowPicker {
        picker: permissions_dialog(&[with_grant(1)], 180, &[], &[session_grant(7)], &[], None),
        refresh: true,
    });
    assert_eq!(open(&app).section(), Some(Section::InForce.index()));
}

/// Review 7, M32: a 3 000-character shell command, its row cut at 200
/// bytes and marked so, is viewable in full (Ctrl+O, scrolled to its end)
/// before `o` approves it once; before that `o` and Enter say to read it.
/// A call cut for its size is refused even after reading.
#[test]
fn a_long_blocked_call_is_read_in_full_before_it_is_approved_once() {
    let command = format!("curl https://example.com/{}TAIL", "a".repeat(3000));
    let long = BlockedCall {
        summary: command.chars().take(200).collect(),
        call: command.clone(),
        ..blocked_call(5, false)
    };
    let mut p = blocked(std::slice::from_ref(&long), 160);
    assert!(
        p.rows[0].text.contains("… (Ctrl+O: the whole call)"),
        "{}",
        p.rows[0].text
    );
    for code in [KeyCode::Char('o'), KeyCode::Enter] {
        assert_eq!(p.key(&press(code)), ModalKey::Stay);
        assert!(
            matches!(&p.line, Some(PickerLine::Refused(why)) if why == crate::ui::picker::VIEW_FIRST),
            "{:?}",
            p.line
        );
    }
    // Ctrl+O shows it whole; End scrolls to its end.
    let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
    p.key(&ctrl_o);
    let shown = |p: &ListPicker| -> String {
        crate::ui::picker::list_picker_lines(p, 60, 20)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let first = shown(&p);
    assert!(first.contains("in full: lines 1-"), "{first}");
    assert!(first.contains("https://example.com/aaa"), "{first}");
    p.key(&press(KeyCode::End));
    let last = shown(&p);
    assert!(last.contains("TAIL"), "{last}");
    // Every line of it was reachable: the whole text, line by line.
    let lines = crate::ui::markdown::wrap_text(&command, 60).len();
    assert!(last.contains(&format!("of {lines}")), "{last}");
    // Back to the list: `o` approves it once now.
    p.key(&press(KeyCode::Esc));
    assert!(p.detail.is_none());
    assert_eq!(
        p.key(&press(KeyCode::Char('o'))),
        ModalKey::Choose(PickAct::Approve(5, GrantScope::Once))
    );
    // Cut for its size: shown with what was kept, never approved once.
    let cut = BlockedCall {
        call_bytes: Some(60_000),
        ..long
    };
    let mut p = blocked(&[cut], 160);
    p.key(&ctrl_o);
    // Drawn once (the draw records how far it scrolls), then End.
    shown(&p);
    p.key(&press(KeyCode::End));
    assert!(shown(&p).contains("cut: the call is 60000 bytes"));
    p.key(&press(KeyCode::Esc));
    assert_eq!(p.key(&press(KeyCode::Char('o'))), ModalKey::Stay);
    assert!(
        matches!(&p.line, Some(PickerLine::Refused(why)) if why.contains("cannot be approved once")),
        "{:?}",
        p.line
    );
}
