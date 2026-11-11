//! The guard on stored state: every file rkat-tui stores (`docs/state.md`)
//! as 0.0.1 wrote it (`tests/fixtures/state-0.0.1/`, taken from a run of
//! the test rig at 3bb9a74; the two `tui.toml` files a user and a project
//! write by hand cover every key 0.0.1 reads) still opens with the readers
//! of this build, strictly where the reader is strict and with the values
//! that were written. A failure here means a change broke the workspaces
//! users already have: undo it, or get the owner's explicit yes first.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use clap::Parser;
use meerkat_core::SessionId;

use crate::approval::mode::PermissionMode;
use crate::config::RulesFile;

/// The fixtures' directory.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/state-0.0.1")
}

/// The id of the captured realm.
fn realm_id() -> String {
    std::fs::read_to_string(fixtures().join("realm-id"))
        .unwrap()
        .trim()
        .to_string()
}

/// The session of the captured conversation, and of its helper.
const SESSION: &str = "01a1cc1a-3977-7480-acbc-0330b9c34d18";
const HELPER_SESSION: &str = "01a1cc1a-3cb6-75a9-9653-f5c3eda5b2f8";

/// The fixtures laid out as a user's machine holds them, in a temporary
/// directory: `cfg/` the config directory (the realm under
/// `workspaces/<id>/`), `cache/`, `state/`, and the workspace `project/`.
/// The workspace path the capture recorded is replaced by the new one
/// everywhere it was written, as a re-home would leave it.
struct Machine {
    _dir: tempfile::TempDir,
    cfg: PathBuf,
    cache: PathBuf,
    state: PathBuf,
    project: PathBuf,
    realm: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let from = fixtures();
        let recorded = toml::from_str::<toml::Value>(
            &std::fs::read_to_string(from.join("realm/workspace.toml")).unwrap(),
        )
        .unwrap()["path"]
            .as_str()
            .unwrap()
            .to_string();
        let project = base.join("project");
        let replace = |text: String| text.replace(&recorded, project.to_str().unwrap());
        let cfg = base.join("cfg");
        let realm = cfg.join("workspaces").join(realm_id());
        copy(&from.join("config"), &cfg, &replace);
        copy(&from.join("realm"), &realm, &replace);
        copy(&from.join("project"), &project, &replace);
        copy(&from.join("cache"), &base.join("cache"), &replace);
        copy(&from.join("state"), &base.join("state"), &replace);
        // The project input the trust record holds the digest of (empty).
        std::fs::write(project.join(".rkat/mcp.toml"), "").unwrap();
        Self {
            _dir: dir,
            cfg,
            cache: base.join("cache"),
            state: base.join("state"),
            project,
            realm,
        }
    }

    fn args(&self) -> crate::Args {
        crate::Args::try_parse_from([
            "rkat-tui",
            "-w",
            self.project.to_str().unwrap(),
            "--config-dir",
            self.cfg.to_str().unwrap(),
            "--user-config-root",
            self.project.to_str().unwrap(),
            "--no-pricing-fetch",
            "--no-confinement",
        ])
        .unwrap()
    }
}

/// Copy the tree `from` to `to`, every file's text through `edit`.
fn copy(from: &Path, to: &Path, edit: &dyn Fn(String) -> String) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target, edit);
        } else {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            std::fs::write(&target, edit(text)).unwrap();
        }
    }
}

/// The three `tui.toml` layers parse strictly, and layered they find the
/// existing realm (not a new one) with what the realm file recorded.
#[test]
fn the_tui_toml_layers_and_the_realm_open() {
    let machine = Machine::new();
    for file in [
        machine.cfg.join("tui.toml"),
        machine.realm.join("tui.toml"),
        machine.project.join(".rkat/tui.toml"),
    ] {
        RulesFile::load(&file).unwrap_or_else(|err| panic!("{}: {err}", file.display()));
    }
    let layers = crate::bootstrap::read_layers(&machine.args()).unwrap();
    assert_eq!(layers.scope.realm_root(), machine.realm);
    let layered = &layers.layered;
    // The realm's mode: the project file may only lower auto.
    assert_eq!(layered.mode, PermissionMode::Plan);
    assert!(
        layered
            .rules
            .matches_allow("shell", &["cargo test --lib".to_string()])
            .is_some(),
        "{:?}",
        layered.rules
    );
    assert!(
        layered
            .rules
            .matches_deny("write_file", &["migrations/0001.sql".to_string()])
            .is_some()
    );
    assert!(layered.endpoints.contains_key("work"));
    assert_eq!(layered.helpers.max_depth, 2);
    assert_eq!(layered.shell.user_timeout_secs, 120);
    assert_eq!(
        layered.ui.checklist,
        crate::config::settings::ChecklistView::Shown
    );
    let manifest =
        crate::bootstrap::realm_home::find(&machine.cfg.join("workspaces"), &machine.project)
            .unwrap()
            .expect("the realm's manifest names the workspace");
    assert_eq!(manifest.id, realm_id());
}

/// The host trust record still trusts the project input it recorded.
#[test]
fn the_trust_record_opens() {
    let machine = Machine::new();
    let record =
        crate::bootstrap::trust::HostTrustRecord::open(&machine.cfg, &machine.project, &realm_id());
    assert!(record.holds_digest(
        Path::new(".rkat/mcp.toml"),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    ));
}

/// The input history files, the price cache, the notes and the unsent
/// messages.
#[test]
fn the_client_files_open() {
    let machine = Machine::new();
    let global = crate::ui::history::History::load(machine.cfg.join("history"));
    assert_eq!(
        global.entries(),
        [
            "first prompt",
            "a prompt\nwith two lines and a \\ backslash"
        ]
    );
    let local = crate::ui::history::History::load(machine.realm.join("tui_history"));
    assert_eq!(local.entries(), ["workspace prompt"]);

    let prices = crate::pricing::CommunityPrices::load_cache(
        &machine.cache.join("pricing-cache.json"),
        1_794_400_000,
    )
    .expect("the price cache opens");
    assert_eq!(prices.len(), 1);
    assert!(!prices.is_stale());

    let user =
        crate::memory::NoteStore::open(machine.cfg.join("notes"), crate::memory::Scope::User);
    assert_eq!(
        user.read("style").unwrap(),
        "Prefer short commits.\nMore.\n"
    );
    assert_eq!(user.list().unwrap().len(), 1);
    let workspace = crate::memory::NoteStore::open(
        machine.realm.join("notes"),
        crate::memory::Scope::Workspace,
    );
    assert_eq!(workspace.read("build").unwrap(), "Build with cargo.\n");

    assert_eq!(
        crate::workbench::unsent::count_unsent(
            &machine.realm.join(crate::workbench::unsent::UNSENT_FILE)
        ),
        1
    );

    let cursor = crate::client::catchup::CursorFile::of_realm(&machine.realm)
        .for_agent("main-ujb2")
        .load()
        .expect("the attach cursor opens");
    assert_eq!(cursor.session_id, SESSION);
    assert_eq!(cursor.sequence, 27);
    assert!(cursor.epoch.is_some() && cursor.daemon.is_some());
}

/// The daemon's files: the endpoint file, the conversation log, the
/// audit log, the usage record, the revealed tools and the helper
/// bindings.
#[tokio::test]
async fn the_daemon_files_open() {
    let machine = Machine::new();
    let endpoint =
        crate::daemon::endpoint::Endpoint::read(&machine.state.join("mob.endpoint")).unwrap();
    assert_eq!(endpoint.workspace, Some(machine.project.clone()));

    let log = crate::daemon::conversations::Log::read(&crate::daemon::conversations::Log::path(
        &machine.realm,
    ))
    .unwrap()
    .expect("the conversation log exists");
    let session = SessionId::parse(SESSION).unwrap();
    assert_eq!(log.agent_of(&session), Some("main-ujb2"));

    let audit = machine.realm.join("audit.jsonl");
    let text = std::fs::read_to_string(&audit).unwrap();
    for line in text.lines() {
        serde_json::from_str::<crate::daemon::audit::Line>(line)
            .unwrap_or_else(|err| panic!("{line}: {err}"));
    }
    let tail = crate::daemon::audit::tail(&audit, 100, |_| true).unwrap();
    assert_eq!(tail.len(), text.lines().count());

    let usage_dir = machine.realm.join("tui_usage");
    let record = crate::daemon::usage::read_record(&usage_dir.join(format!("{SESSION}.toml")));
    let totals = &record.cost.per_model["claude-sonnet-4-6"];
    assert_eq!((totals.input, totals.output), (40, 20));
    assert_eq!(record.context.map(|c| c.tokens), Some(15));
    assert_eq!(
        std::fs::read_dir(&usage_dir).unwrap().count(),
        1,
        "nothing was set aside as unreadable"
    );

    let reveals = crate::daemon::deferral::reveal::Reveals::new(&machine.realm);
    reveals.load().await;
    assert!(
        reveals
            .revealed(&session)
            .contains("meerkat_schedule_create")
    );

    let bindings = crate::mobs::bindings::HelperBindings::open(&machine.realm).unwrap();
    let (key, binding) = bindings
        .member_by_session(&SessionId::parse(HELPER_SESSION).unwrap())
        .expect("the helper's binding opens");
    assert_eq!(key.member.as_str(), "scout");
    assert_eq!(binding.tag.owner, session);
}
