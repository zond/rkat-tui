#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn settings(dir: &std::path::Path, layers: Layers) -> Settings {
    Settings::new(
        Some(SettingsFiles::new(
            dir.join("user").join("tui.toml"),
            dir.join("realm").join("tui.toml"),
        )),
        layers,
    )
}

/// A write says the key, the value, the level and file written, the
/// value in force and its layer, and when it takes effect.
#[test]
fn a_change_is_confirmed_with_its_file_value_in_force_and_timing() {
    let tmp = tempfile::tempdir().unwrap();
    let settings = settings(tmp.path(), Layers::default());
    let written = settings
        .write("auto.compact_prepare_at", Some("65%"), true)
        .unwrap();
    assert!(written.changed());
    let user_file = tmp.path().join("user").join("tui.toml");
    assert_eq!(
        written.confirmation(),
        format!(
            "auto.compact_prepare_at = 65%: written to your tui.toml ({}); in force: 65% (from your tui.toml); applied at once",
            user_file.display()
        )
    );
    assert_eq!(
        std::fs::read_to_string(&user_file).unwrap(),
        "[auto]\ncompact_prepare_at = \"65%\"\n"
    );
    // The workspace's wins over the user's.
    let written = settings
        .write("auto.compact_prepare_at", Some("300k"), false)
        .unwrap();
    assert!(
        written
            .confirmation()
            .contains("in force: 300000 (from this workspace); applied at once"),
        "{}",
        written.confirmation()
    );
    // Now the user's is under it: written, not in force, and said so.
    let written = settings
        .write("auto.compact_prepare_at", Some("50%"), true)
        .unwrap();
    assert!(!written.changed());
    let text = written.confirmation();
    assert!(
        text.contains("the value in force did not change; this workspace's tui.toml sets it (300000), which wins here: /unset auto.compact_prepare_at removes it"),
        "{text}"
    );
    // Removed from the workspace: the user's applies again.
    let written = settings
        .write("auto.compact_prepare_at", None, false)
        .unwrap();
    assert!(written.changed());
    assert!(
        written
            .confirmation()
            .starts_with("auto.compact_prepare_at: removed from this workspace's tui.toml"),
        "{}",
        written.confirmation()
    );
    assert_eq!(written.after.value, Some(toml::Value::String("50%".into())));
    // A setting read at start says so.
    let written = settings
        .write("helpers.max_depth", Some("2"), false)
        .unwrap();
    assert!(
        written
            .confirmation()
            .ends_with("it applies at the next start")
    );
}

/// The command line and a project's value win over a file's, and the
/// confirmation names them.
#[test]
fn a_higher_layer_that_wins_is_named() {
    let tmp = tempfile::tempdir().unwrap();
    let layers = Layers {
        cli: "[auto]\ncompact_prepare_at = \"80%\"\n".parse().unwrap(),
        project: "[shell]\nuser_timeout_secs = 30\n".parse().unwrap(),
        ..Layers::default()
    };
    let settings = settings(tmp.path(), layers);
    let text = settings
        .write("auto.compact_prepare_at", Some("60%"), false)
        .unwrap()
        .confirmation();
    assert!(
        text.contains("in force: 80% (from the command line); the value in force did not change; the daemon's command line sets it (80%), which wins until the daemon restarts"),
        "{text}"
    );
    let text = settings
        .write("shell.user_timeout_secs", Some("300"), true)
        .unwrap()
        .confirmation();
    assert!(
        text.contains("the project's .rkat/tui.toml lowers it to 30, which wins"),
        "{text}"
    );
}

/// A value the setting does not take, an unknown key and a section are
/// refused naming what to do; nothing is written.
#[test]
fn bad_changes_are_refused_and_nothing_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let settings = settings(tmp.path(), Layers::default());
    let err = settings.write("ui.bell", Some("loud"), false).unwrap_err();
    assert_eq!(err, "ui.bell takes `true` or `false`");
    let err = settings.write("ui.nope", Some("1"), false).unwrap_err();
    assert!(
        err.contains("did you mean ui.helpers, ui.bell, ui.checklist"),
        "{err}"
    );
    let err = settings
        .write("endpoints.x.base_url", Some("http://h"), false)
        .unwrap_err();
    assert!(err.contains("structured section endpoints.<name>"), "{err}");
    let err = settings
        .write("thinking", Some("adaptive"), false)
        .unwrap_err();
    assert!(err.contains("section"), "{err}");
    assert!(!tmp.path().join("realm").join("tui.toml").exists());
    // Without files, nothing can be written.
    let bare = Settings::new(None, Layers::default());
    assert!(bare.write("ui.bell", Some("false"), false).is_err());
}

/// The mode drops its older spelling from the file it writes.
#[test]
fn the_mode_replaces_enabled_in_the_file_it_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let realm = tmp.path().join("realm").join("tui.toml");
    std::fs::create_dir_all(realm.parent().unwrap()).unwrap();
    std::fs::write(&realm, "# mine\n[auto]\nenabled = false\n").unwrap();
    let settings = settings(
        tmp.path(),
        Layers {
            workspace: "[auto]\nenabled = false\n".parse().unwrap(),
            ..Layers::default()
        },
    );
    assert_eq!(
        settings.resolve(Setting::find("auto.mode").unwrap()).value,
        Some(toml::Value::String("default".into()))
    );
    let written = settings.write("auto.mode", Some("plan"), false).unwrap();
    assert!(written.changed());
    assert_eq!(
        std::fs::read_to_string(&realm).unwrap(),
        "# mine\n[auto]\nmode = \"plan\"\n"
    );
}

/// `/settings`' rows: the value in force, its layer and what each layer
/// holds, for every setting; then the sections.
#[test]
fn the_rows_hold_every_setting_and_layer() {
    let tmp = tempfile::tempdir().unwrap();
    let layers = Layers {
        user: "[ui]\nbell = false\n".parse().unwrap(),
        workspace: "[ui]\nbell = true\n".parse().unwrap(),
        ..Layers::default()
    };
    let list = settings(tmp.path(), layers).rows();
    assert_eq!(list.rows.len(), SETTINGS.len());
    assert_eq!(list.sections.len(), SECTIONS.len());
    let bell = list.rows.iter().find(|row| row.key == "ui.bell").unwrap();
    assert_eq!(bell.value, "true");
    assert_eq!(bell.source, Level::Workspace);
    assert_eq!(
        bell.levels,
        vec![
            (Level::User, "false".to_string()),
            (Level::Workspace, "true".to_string())
        ]
    );
    let model = list
        .rows
        .iter()
        .find(|row| row.key == "auto.classifier_model")
        .unwrap();
    assert_eq!(model.value, "the start-up endpoint's per-dialect default");
    assert_eq!(model.source, Level::Default);
    assert_eq!(list.user_file, tmp.path().join("user").join("tui.toml"));
}
