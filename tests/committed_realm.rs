//! A committed workspace realm (`.rkat/tui/` tracked by git) refuses the
//! start (S1/E1 of the security review): `bootstrap::committed` reads the
//! git index itself, in every index version.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use rkat_tui::bootstrap::committed::{Committed, check};

/// Run `git` in `dir` (tests only: library code never launches git).
fn git(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .is_ok_and(|out| out.status.success())
}

fn repo() -> Option<(tempfile::TempDir, PathBuf)> {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    if !git(&root, &["init", "-q"]) {
        eprintln!("SKIPPED: git is not available");
        return None;
    }
    Some((dir, root))
}

#[test]
fn a_tracked_realm_file_refuses_the_start_in_every_index_version() {
    for version in ["2", "3", "4"] {
        let Some((_dir, root)) = repo() else { return };
        std::fs::write(root.join("README"), "x").unwrap();
        std::fs::create_dir_all(root.join("a/deep/dir")).unwrap();
        std::fs::write(root.join("a/deep/dir/f"), "x").unwrap();
        let realm = root.join(".rkat/tui");
        std::fs::create_dir_all(&realm).unwrap();
        std::fs::write(realm.join(".gitignore"), "*\n").unwrap();
        std::fs::write(realm.join("config.toml"), "[self_hosted]\n").unwrap();
        assert!(git(&root, &["add", "README", "a"]));
        assert!(git(&root, &["update-index", "--index-version", version]));
        // Untracked (ignored) realm: the start goes on.
        assert_eq!(check(&root, &realm), Ok(()), "index v{version}");
        assert!(git(&root, &["add", "-f", ".rkat/tui/config.toml"]));
        assert!(git(&root, &["update-index", "--index-version", version]));
        let err = check(&root, &realm).unwrap_err();
        assert!(
            matches!(&err, Committed::Tracked { files, .. } if files == &[".rkat/tui/config.toml".to_string()]),
            "index v{version}: {err:?}"
        );
        let text = err.to_string();
        assert!(text.contains("git rm -r --cached .rkat/tui"), "{text}");
        assert!(text.contains("--trust-project"), "{text}");
    }
}

#[test]
fn a_tracked_realm_in_a_subdirectory_workspace_is_found() {
    let Some((_dir, root)) = repo() else { return };
    let ws = root.join("sub");
    let realm = ws.join(".rkat/tui");
    std::fs::create_dir_all(&realm).unwrap();
    std::fs::write(realm.join(".gitignore"), "*\n").unwrap();
    std::fs::write(realm.join("tui.toml"), "").unwrap();
    assert!(git(&root, &["add", "-f", "sub/.rkat/tui/tui.toml"]));
    assert!(matches!(check(&ws, &realm), Err(Committed::Tracked { .. })));
}

/// The start itself refuses (the scope is resolved before anything of the
/// realm is read or made), and `--trust-project` starts anyway.
#[test]
fn the_start_refuses_a_tracked_realm_unless_trusted() {
    use clap::Parser as _;
    let Some((_dir, root)) = repo() else { return };
    let realm = root.join(".rkat/tui");
    std::fs::create_dir_all(&realm).unwrap();
    std::fs::write(realm.join(".gitignore"), "*\n").unwrap();
    std::fs::write(realm.join("config.toml"), "").unwrap();
    assert!(git(&root, &["add", "-f", ".rkat/tui/config.toml"]));
    let args = |extra: &[&str]| {
        let ws = root.to_str().unwrap();
        let cfg = format!("{ws}/.cfg");
        let mut argv = vec!["rkat-tui", "-w", ws, "--config-dir", &cfg];
        argv.extend_from_slice(extra);
        rkat_tui::Args::try_parse_from(argv).unwrap()
    };
    let err = rkat_tui::bootstrap::resolve_scope(&args(&[])).unwrap_err();
    assert!(
        matches!(
            err,
            rkat_tui::error::TuiError::CommittedRealm(Committed::Tracked { .. })
        ),
        "{err}"
    );
    assert!(rkat_tui::bootstrap::resolve_scope(&args(&["--trust-project"])).is_ok());
}
