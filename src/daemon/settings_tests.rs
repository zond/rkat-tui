#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use toml::Value;

fn settings(dir: &std::path::Path, layers: Layers) -> Settings {
    Settings::new(
        Some(SettingsFiles::new(
            dir.join("user").join("tui.toml"),
            dir.join("realm").join("tui.toml"),
        )),
        layers,
        Some(dir.join("project").join(".rkat").join("tui.toml")),
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
        err.contains("did you mean ui.helpers, ui.bell, ui.mouse, ui.checklist"),
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
    let bare = Settings::new(None, Layers::default(), None);
    assert!(bare.write("ui.bell", Some("false"), false).is_err());
}

/// The mode is written in place, the file's comments kept; a file that
/// still has `enabled` (the mode's spelling before 0.0.7) is not written.
#[test]
fn the_mode_is_written_in_place_and_a_file_with_enabled_is_not_written() {
    let tmp = tempfile::tempdir().unwrap();
    let realm = tmp.path().join("realm").join("tui.toml");
    std::fs::create_dir_all(realm.parent().unwrap()).unwrap();
    std::fs::write(&realm, "# mine\n[auto]\nenabled = false\n").unwrap();
    let held: Layers = Layers {
        workspace: "[auto]\nmode = \"default\"\n".parse().unwrap(),
        ..Layers::default()
    };
    let settings = settings(tmp.path(), held);
    let refused = settings
        .write("auto.mode", Some("plan"), false)
        .unwrap_err();
    assert!(refused.contains("enabled"), "{refused}");
    std::fs::write(&realm, "# mine\n[auto]\nmode = \"default\"\n").unwrap();
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

/// A file changed on disk is found by content: the daemon's own write
/// finds nothing to do; another writer's change (made between two of the
/// daemon's own) is found, not absorbed, with the settings whose value in
/// force moved; a section's change is named; a file that does not read is
/// reported once and its last good values stay until it reads again.
#[test]
fn a_changed_file_is_synced_by_content() {
    let tmp = tempfile::tempdir().unwrap();
    let settings = settings(tmp.path(), Layers::default());
    let user_file = tmp.path().join("user").join("tui.toml");
    let changed = |synced: Synced| match synced {
        Synced::Changed {
            changes,
            permissions,
            later,
        } => (
            changes
                .iter()
                .map(|change| (change.setting.key, change.after.value.as_ref().map(show)))
                .collect::<Vec<_>>(),
            permissions,
            later,
        ),
        other => panic!("not a change: {other:?}"),
    };
    // Nothing yet, nothing to do.
    assert!(matches!(settings.sync(Level::User), Synced::Same));
    // The daemon's own write: the file holds what it holds.
    settings.write("ui.bell", Some("false"), true).unwrap();
    assert!(matches!(settings.sync(Level::User), Synced::Same));
    // Someone else's edit, then the daemon's own write of another key: the
    // edit is still found.
    let text = std::fs::read_to_string(&user_file).unwrap();
    std::fs::write(&user_file, format!("{text}[helpers]\nmax_depth = 3\n")).unwrap();
    settings.write("auto.compact", Some("warn"), true).unwrap();
    assert_eq!(
        changed(settings.sync(Level::User)),
        (
            vec![("helpers.max_depth", Some("3".to_string()))],
            false,
            false
        )
    );
    assert!(matches!(settings.sync(Level::User), Synced::Same));
    // A rule by hand: a permission section changed, no setting; the
    // lasting permissions have it.
    let text = std::fs::read_to_string(&user_file).unwrap();
    std::fs::write(&user_file, format!("{text}[[deny]]\ntool = \"shell\"\n")).unwrap();
    assert_eq!(changed(settings.sync(Level::User)), (vec![], true, false));
    assert_eq!(settings.lasting().rules.len(), 1);
    // A price: a section that applies at the next start.
    let text = std::fs::read_to_string(&user_file).unwrap();
    std::fs::write(
        &user_file,
        format!("{text}[pricing.\"some-model\"]\ninput = 1.0\n"),
    )
    .unwrap();
    assert_eq!(changed(settings.sync(Level::User)), (vec![], false, true));
    // Broken: reported once, the last good values stay.
    std::fs::write(&user_file, "[ui\nbell = true\n").unwrap();
    assert!(
        matches!(settings.sync(Level::User), Synced::Unreadable(why) if why.contains("tui.toml"))
    );
    assert!(matches!(
        settings.sync(Level::User),
        Synced::StillUnreadable
    ));
    let bell = Setting::find("ui.bell").unwrap();
    assert_eq!(settings.resolve(bell).value, Some(Value::Boolean(false)));
    // Fixed: what it says now, against the last good.
    std::fs::write(&user_file, "[ui]\nbell = true\n").unwrap();
    let (changes, permissions, later) = changed(settings.sync(Level::User));
    assert!(permissions && later);
    let keys: Vec<&str> = changes.iter().map(|(key, _)| *key).collect();
    assert_eq!(keys, vec!["auto.compact", "helpers.max_depth", "ui.bell"]);
}

/// A project file changes only what a project may set: its classifier
/// model is never in force, its lower limit is.
#[test]
fn a_project_file_syncs_only_what_a_project_may_set() {
    let tmp = tempfile::tempdir().unwrap();
    let settings = settings(tmp.path(), Layers::default());
    let project = tmp.path().join("project").join(".rkat").join("tui.toml");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(
        &project,
        "[auto]\nclassifier_model = \"cheap:model\"\ntrust_project_builds = true\n[helpers]\nmax_depth = 0\n",
    )
    .unwrap();
    let Synced::Changed { changes, .. } = settings.sync(Level::Project) else {
        panic!("not a change")
    };
    let keys: Vec<&str> = changes.iter().map(|change| change.setting.key).collect();
    assert_eq!(keys, vec!["helpers.max_depth"]);
    // Its rules and statements are taken at start only: never a
    // permission change, and never among the lasting permissions.
    std::fs::write(
        &project,
        "[auto]\nallow = [\"pushing to main\"]\n[[deny]]\ntool = \"shell\"\n[helpers]\nmax_depth = 0\n",
    )
    .unwrap();
    assert!(matches!(
        settings.sync(Level::Project),
        Synced::Changed {
            permissions: false,
            later: true,
            ..
        }
    ));
    let lasting = settings.lasting();
    assert!(lasting.rules.is_empty() && lasting.statements.is_empty());
    assert_eq!(
        settings
            .resolve(Setting::find("auto.classifier_model").unwrap())
            .value,
        None
    );
    assert_eq!(
        settings
            .watched()
            .iter()
            .map(|(level, _)| *level)
            .collect::<Vec<_>>(),
        vec![Level::User, Level::Workspace, Level::Project]
    );
}

/// The helper model lists: `/set` of one while the other is in force is
/// refused naming both, and a hand edit that sets both (here across the
/// user's and the workspace's files) is not applied: reported once, the
/// last good state stays, and removing one applies again.
#[test]
fn both_helper_model_lists_are_refused_on_a_write_and_a_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let settings = settings(tmp.path(), Layers::default());
    settings
        .write("helpers.allow_models", Some("claude-haiku-*"), true)
        .unwrap();
    let refused = settings
        .write("helpers.deny_models", Some("gpt-*"), false)
        .unwrap_err();
    assert!(
        refused.starts_with("helpers.deny_models was not changed")
            && refused.contains("helpers.allow_models (set in your tui.toml)")
            && refused.contains("helpers.deny_models (set in this workspace's tui.toml)"),
        "{refused}"
    );
    let realm_file = tmp.path().join("realm").join("tui.toml");
    assert!(!realm_file.exists(), "nothing was written");
    // A hand edit of the workspace's file sets the other list.
    std::fs::create_dir_all(realm_file.parent().unwrap()).unwrap();
    std::fs::write(&realm_file, "[helpers]\ndeny_models = [\"gpt-*\"]\n").unwrap();
    let Synced::Unreadable(why) = settings.sync(Level::Workspace) else {
        panic!("applied");
    };
    assert!(
        why.contains("helpers.allow_models") && why.contains("helpers.deny_models"),
        "{why}"
    );
    assert!(matches!(
        settings.sync(Level::Workspace),
        Synced::StillUnreadable
    ));
    let deny = Setting::find("helpers.deny_models").unwrap();
    assert_eq!(settings.resolve(deny).value, None, "the last good state");
    // A bad pattern in a file does not read either, naming the entry.
    std::fs::write(
        &realm_file,
        "[helpers]\nmodel = \"x\"\nallow_models = [\"a*b\"]\n",
    )
    .unwrap();
    let Synced::Unreadable(why) = settings.sync(Level::Workspace) else {
        panic!("applied");
    };
    assert!(why.contains("`a*b`"), "{why}");
    // The workspace's allow list replaces the user's: one list, applied.
    std::fs::write(&realm_file, "[helpers]\nallow_models = [\"gpt-5.5\"]\n").unwrap();
    assert!(matches!(
        settings.sync(Level::Workspace),
        Synced::Changed { .. }
    ));
}
