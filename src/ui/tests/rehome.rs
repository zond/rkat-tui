//! The `/rehome` picker: its rows, the question before a re-home, Esc,
//! and the directory field's check (a refusal kept in the field, a valid
//! directory asked about).

use super::*;
use crate::bootstrap::realm_home::{Home, Manifest};
use std::path::{Path, PathBuf};

fn home(id: &str, path: &Path) -> Home {
    Home {
        id: id.to_string(),
        dir: PathBuf::from("/cfg/workspaces").join(id),
        manifest: Manifest {
            path: path.to_path_buf(),
            name: "x".into(),
            git_remote: None,
            created: String::new(),
            last_used: "2026-11-01T10:00:00Z".into(),
        },
    }
}

/// This workspace's realm, one whose workspace is gone and one whose
/// workspace still exists (`kept`, a directory of the test's).
fn realms(kept: &Path) -> (Home, Vec<Home>, PathBuf) {
    let ws = kept.join("here");
    std::fs::create_dir_all(&ws).unwrap();
    let current = home("aaaa", &ws);
    let homes = vec![
        current.clone(),
        home("bbbb", &kept.join("gone-for-good")),
        home("cccc", kept),
    ];
    (current, homes, ws)
}

fn open(app: &mut App, kept: &Path) -> PathBuf {
    let (current, homes, ws) = realms(kept);
    app.update(Msg::ShowPicker {
        picker: crate::client::rehome::picker(&homes, &current, &ws),
        refresh: false,
    });
    ws
}

fn picker(app: &App) -> &ListPicker {
    match &app.modal {
        Some(Modal::List(picker)) if picker.kind == PickerKind::Rehome => picker,
        other => panic!("no /rehome picker: {other:?}"),
    }
}

/// `/rehome` alone asks the runner for the picker; the typed forms still
/// re-home directly.
#[test]
fn rehome_alone_asks_for_the_picker() {
    let mut app = App::new(StatusInfo::default());
    type_str(&mut app, "/rehome");
    assert_eq!(app.update(key(KeyCode::Enter)), vec![Effect::Rehome(None)]);
    type_str(&mut app, "/rehome from bbbb");
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::Rehome(Some("from bbbb".into()))]
    );
}

/// The rows: the gone workspace's state first, the one still there dim,
/// the move row last, under a line on this workspace's state; Enter asks
/// first, `n` and Esc cancel the question with nothing sent, `y` runs
/// `/rehome from <id>` and closes the picker; Esc closes it.
#[test]
fn the_picker_lists_the_adoptable_state_and_asks_before_it_adopts() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = App::new(StatusInfo::default());
    let ws = open(&mut app, tmp.path());
    let rows = &picker(&app).rows;
    assert_eq!(rows.len(), 3);
    assert!(rows[0].text.starts_with("bbbb") && rows[0].text.ends_with("(gone)"));
    assert!(!rows[0].dim);
    assert!(rows[1].text.starts_with("cccc") && rows[1].dim);
    assert!(rows[2].text.starts_with("Move this workspace's state"));
    let header = picker(&app).header.clone().unwrap();
    assert!(header.contains("realm aaaa"), "{header}");
    assert!(header.contains(&ws.display().to_string()), "{header}");

    let drawn = screen(&mut app, 30).join("\n");
    assert!(drawn.contains("Workspace state"), "{drawn}");
    assert!(drawn.contains("bbbb"), "{drawn}");
    assert!(drawn.contains("(gone)"), "{drawn}");
    assert!(drawn.contains("Move this workspace's state"), "{drawn}");

    // Enter asks; n cancels; nothing was sent.
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert!(matches!(
        &picker(&app).line,
        Some(PickerLine::Confirm { question, .. }) if question.contains("Adopt realm bbbb") && question.contains("realm aaaa")
    ));
    assert!(app.update(key(KeyCode::Char('n'))).is_empty());
    assert!(picker(&app).line.is_none());
    // The one whose workspace exists says it starts fresh there.
    app.update(key(KeyCode::Down));
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert!(matches!(
        &picker(&app).line,
        Some(PickerLine::Confirm { question, .. }) if question.contains("still exists") && question.contains("fresh state")
    ));
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert!(picker(&app).line.is_none());
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::Rehome(Some("from cccc".into()))]
    );
    assert!(app.modal.is_none());

    // Esc closes with nothing sent.
    let mut app = App::new(StatusInfo::default());
    open(&mut app, tmp.path());
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert!(app.modal.is_none());
}

/// With nothing to adopt the picker still has the move row and says so.
#[test]
fn with_nothing_to_adopt_the_move_row_stays() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("here");
    std::fs::create_dir_all(&ws).unwrap();
    let current = home("aaaa", &ws);
    let picker = crate::client::rehome::picker(std::slice::from_ref(&current), &current, &ws);
    assert_eq!(picker.rows.len(), 1);
    assert!(picker.rows[0].text.starts_with("Move"));
    assert!(
        picker
            .header
            .as_deref()
            .unwrap()
            .contains("no other workspace's state")
    );
}

/// The move row opens a field; an empty Enter is refused in the field;
/// a typed directory is sent for the check with the field kept; the
/// check's refusal shows in the field with the text kept (an edit clears
/// it); its question then asks, and `y` runs `/rehome to <dir>`.
#[test]
fn the_directory_field_checks_in_place_and_asks_before_it_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let mut app = App::new(StatusInfo::default());
    open(&mut app, tmp.path());
    app.update(key(KeyCode::Down));
    app.update(key(KeyCode::Down));
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert!(picker(&app).editing());
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    let refused = |app: &App| match &picker(app).line {
        Some(PickerLine::Field(Field {
            what: FieldFor::Directory { refused },
            text,
        })) => (text.clone(), refused.clone()),
        other => panic!("{other:?}"),
    };
    assert!(refused(&app).1.unwrap().contains("type the directory"));
    type_str(&mut app, "~/typo");
    assert_eq!(refused(&app), ("~/typo".to_string(), None));
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::RehomeCheck("~/typo".into())]
    );
    assert_eq!(refused(&app), ("~/typo".to_string(), None));
    app.update(Msg::RehomeChecked {
        typed: "~/typo".into(),
        answer: Err("/home/x/typo does not exist: check the spelling".into()),
    });
    assert_eq!(
        refused(&app),
        (
            "~/typo".to_string(),
            Some("/home/x/typo does not exist: check the spelling".to_string())
        )
    );
    let drawn = screen(&mut app, 30).join("\n");
    assert!(drawn.contains("does not exist"), "{drawn}");
    assert!(drawn.contains("> ~/typo"), "{drawn}");
    // A stale answer (other text typed since) changes nothing.
    app.update(key(KeyCode::Backspace));
    app.update(Msg::RehomeChecked {
        typed: "~/typo".into(),
        answer: Ok(("Move?".into(), PathBuf::from("/x"))),
    });
    assert_eq!(refused(&app), ("~/typ".to_string(), None));
    type_str(&mut app, "o");
    assert_eq!(
        app.update(key(KeyCode::Enter)),
        vec![Effect::RehomeCheck("~/typo".into())]
    );
    app.update(Msg::RehomeChecked {
        typed: "~/typo".into(),
        answer: Ok(("Move this workspace's state?".into(), PathBuf::from("/d"))),
    });
    assert!(matches!(
        &picker(&app).line,
        Some(PickerLine::Confirm { question, .. }) if question == "Move this workspace's state?"
    ));
    assert_eq!(
        app.update(key(KeyCode::Char('y'))),
        vec![Effect::Rehome(Some("to /d".into()))]
    );
    assert!(app.modal.is_none());
}
