#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use toml::Value;

const COMMENTED: &str = "# my settings, kept by hand\n\n[auto]\n# when to prepare\ncompact_prepare_at = \"70%\"   # was 80%\nclassifier_probe = false\n\n[ui]\nbell = false # quiet\n\n[[allow]]\ntool = \"shell\" # builds\narg = \"cargo nextest run*\"\n";

fn file(dir: &Path) -> SettingsFile {
    SettingsFile::new(Level::User, dir.join("tui").join("tui.toml"))
}

#[test]
fn a_setting_changes_only_its_own_key_and_keeps_comments_and_order() {
    let tmp = tempfile::tempdir().unwrap();
    let file = file(tmp.path());
    std::fs::create_dir_all(file.path().parent().unwrap()).unwrap();
    std::fs::write(file.path(), COMMENTED).unwrap();
    let read = file
        .set(
            "auto.compact_prepare_at",
            Some(&Value::String("65%".into())),
        )
        .unwrap();
    assert_eq!(
        read.auto.unwrap().compact_prepare_at,
        Some(crate::config::settings::PrepareAt::Percent(65))
    );
    assert_eq!(
        std::fs::read_to_string(file.path()).unwrap(),
        COMMENTED.replace("\"70%\"", "\"65%\"")
    );
    // A new key lands in its table; a new table at the end.
    file.set("auto.compact", Some(&Value::String("warn".into())))
        .unwrap();
    file.set("helpers.max_depth", Some(&Value::Integer(2)))
        .unwrap();
    let text = std::fs::read_to_string(file.path()).unwrap();
    assert!(text.starts_with("# my settings, kept by hand\n"), "{text}");
    assert!(
        text.contains("classifier_probe = false\ncompact = \"warn\"\n"),
        "{text}"
    );
    assert!(text.contains("bell = false # quiet"), "{text}");
    assert!(text.ends_with("[helpers]\nmax_depth = 2\n"), "{text}");
    // Removing a key leaves the rest as it was.
    file.set("auto.compact", None).unwrap();
    file.set("helpers.max_depth", None).unwrap();
    let text = std::fs::read_to_string(file.path()).unwrap();
    assert!(
        text.starts_with(&COMMENTED.replace("\"70%\"", "\"65%\"")),
        "{text}"
    );
}

#[test]
fn a_missing_file_is_created_and_one_with_auto_enabled_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let file = SettingsFile::new(Level::Workspace, tmp.path().join("realm").join("tui.toml"));
    file.set("auto.mode", Some(&Value::String("plan".into())))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(file.path()).unwrap(),
        "[auto]\nmode = \"plan\"\n"
    );
    // `enabled` is gone: a file that still has it does not read, and the
    // writer leaves it as it is (no migration).
    std::fs::write(file.path(), "[auto]\nenabled = false\n").unwrap();
    let err = file
        .set("auto.mode", Some(&Value::String("accept-edits".into())))
        .unwrap_err();
    assert!(err.to_string().contains("enabled"), "{err}");
    assert_eq!(
        std::fs::read_to_string(file.path()).unwrap(),
        "[auto]\nenabled = false\n"
    );
    std::fs::write(file.path(), "[auto]\nmode = \"plan\"\n").unwrap();
    // The realm's stays owner-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(file.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn lists_and_rules_add_once_and_remove_exactly() {
    let tmp = tempfile::tempdir().unwrap();
    let file = file(tmp.path());
    std::fs::create_dir_all(file.path().parent().unwrap()).unwrap();
    std::fs::write(file.path(), COMMENTED).unwrap();
    assert!(
        file.add_to_list("auto.allow", "network requests to api.example.com")
            .unwrap()
    );
    assert!(
        !file
            .add_to_list("auto.allow", "network requests to api.example.com")
            .unwrap()
    );
    assert!(
        file.add_to_list("confinement.extra_write", "/data/out")
            .unwrap()
    );
    let entry = RuleEntry {
        tool: "shell".into(),
        arg: Some("git status*".into()),
        literal: None,
    };
    assert!(file.add_rule("allow", &entry).unwrap());
    assert!(!file.add_rule("allow", &entry).unwrap());
    let read = file.read().unwrap();
    assert_eq!(read.allow.len(), 2);
    assert_eq!(read.allow[1], entry);
    assert_eq!(
        read.auto.unwrap().allow,
        vec!["network requests to api.example.com".to_string()]
    );
    assert_eq!(
        read.confinement.unwrap().extra_write,
        vec![PathBuf::from("/data/out")]
    );
    // The hand-written rule keeps its comment.
    let text = std::fs::read_to_string(file.path()).unwrap();
    assert!(text.contains("tool = \"shell\" # builds"), "{text}");
    assert!(file.remove_rule("allow", &entry).unwrap());
    assert!(!file.remove_rule("allow", &entry).unwrap());
    assert!(
        file.remove_from_list("auto.allow", "network requests to api.example.com")
            .unwrap()
    );
    assert!(
        !file
            .remove_from_list("auto.allow", "nothing like it")
            .unwrap()
    );
    let read = file.read().unwrap();
    assert_eq!(read.allow.len(), 1);
    // Inline rule arrays (as --set writes them) take entries too.
    std::fs::write(file.path(), "allow = [{ tool = \"read_file\" }]\n").unwrap();
    assert!(file.add_rule("allow", &entry).unwrap());
    assert_eq!(file.read().unwrap().allow.len(), 2);
}

#[test]
fn a_change_that_would_not_load_or_a_broken_file_is_refused_and_nothing_written() {
    let tmp = tempfile::tempdir().unwrap();
    let file = file(tmp.path());
    std::fs::create_dir_all(file.path().parent().unwrap()).unwrap();
    std::fs::write(file.path(), COMMENTED).unwrap();
    // A key the schema does not know.
    let err = file
        .set("auto.nonsense", Some(&Value::Boolean(true)))
        .unwrap_err();
    assert!(matches!(err, WriteError::Invalid { .. }), "{err}");
    // A key under a value.
    std::fs::write(file.path(), "auto = 3\n").unwrap();
    let err = file
        .set("auto.mode", Some(&Value::String("plan".into())))
        .unwrap_err();
    assert!(matches!(err, WriteError::Shape { .. }), "{err}");
    // A file that does not parse stays as it is.
    std::fs::write(file.path(), "[auto\n").unwrap();
    let err = file
        .set("ui.bell", Some(&Value::Boolean(true)))
        .unwrap_err();
    assert!(matches!(err, WriteError::Parse { .. }), "{err}");
    assert_eq!(std::fs::read_to_string(file.path()).unwrap(), "[auto\n");
}

#[cfg(unix)]
#[test]
fn a_symlinked_file_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let file = file(tmp.path());
    std::fs::create_dir_all(file.path().parent().unwrap()).unwrap();
    let elsewhere = tmp.path().join("dotfiles.toml");
    std::fs::write(&elsewhere, "").unwrap();
    std::os::unix::fs::symlink(&elsewhere, file.path()).unwrap();
    let err = file
        .set("ui.bell", Some(&Value::Boolean(false)))
        .unwrap_err();
    assert!(matches!(err, WriteError::Write(_)), "{err}");
    assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "");
}

/// Two writers of one file (two daemons' handles, each with its own
/// in-process lock) interleave whole changes: neither loses the other's.
#[test]
fn two_writers_of_one_file_lose_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("tui").join("tui.toml");
    let writers: Vec<_> = (0..2)
        .map(|n| {
            let file = SettingsFile::new(Level::User, path.clone());
            std::thread::spawn(move || {
                for i in 0..20 {
                    file.add_to_list("auto.allow", &format!("writer {n} statement {i}"))
                        .unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let file = SettingsFile::new(Level::User, path);
    assert_eq!(file.read().unwrap().auto.unwrap().allow.len(), 40);
}
