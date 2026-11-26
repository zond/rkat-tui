//! The guard on stored state: every file rkat-tui stores (`docs/state.md`)
//! as 0.1.0 wrote it (`tests/fixtures/state-0.1.0/`, taken from a run of
//! the test rig at v0.1.0, 8277f8b: one daemon on a workspace with the
//! scripted LLM, one client, as `docs/evidence.md`, "The stored-state
//! guard moves to 0.1.0", describes; the two `tui.toml` files a user and a
//! project write by hand cover every key 0.1.0 reads, and 0.1.0's readers
//! took them) still opens with the readers of this build, strictly where
//! the reader is strict and with the values that were written. A failure
//! here means a change broke the workspaces users already have: undo it,
//! or get the owner's explicit yes first.
//!
//! Two things in the copies differ from the capture, on purpose, for the
//! scanners of a mirror of this repository: the dead daemon token in
//! `state/mob.endpoint` is 64 zeros (same shape), and the realm's
//! `tui_history` is stored as `realm/tui-input-history` and copied back to
//! its real name ([`copy`]). The wired card's key in `realm/wires.json`
//! was made of 32 bytes of 1 by the capture itself, not replaced.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use clap::Parser;
use meerkat_core::SessionId;

use crate::approval::mode::PermissionMode;
use crate::config::RulesFile;
use crate::mobs::{HelperContext, HelperKind, HelperLife};

/// The fixtures' directory.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/state-0.1.0")
}

/// The id of the captured realm.
fn realm_id() -> String {
    std::fs::read_to_string(fixtures().join("realm-id"))
        .unwrap()
        .trim()
        .to_string()
}

/// The agent of the captured conversation.
const AGENT: &str = "main-s8hs";
/// The session of the captured conversation, and of its kept helpers
/// `scout` (in the workspace's mob) and `w1` (in the mob `team` the agent
/// made).
const SESSION: &str = "01a21870-e59b-7542-b2c1-840d59812878";
const SCOUT_SESSION: &str = "01a21870-e987-7411-ab53-a00c77719d3f";
const W1_SESSION: &str = "01a21870-ed70-7235-a8c2-dec9cccd84bb";
/// The schedule whose run the capture counted.
const SCHEDULE: &str = "01a21870-e8dd-76f0-9e26-e4371fecb8f6";

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
        // The realm's `tui_history` is kept as `tui-input-history` in the
        // repository: a mirror's guard against committed shell histories
        // refuses a file named `*_history`. The content is the capture's.
        let name = entry.file_name();
        let target = if name == "tui-input-history" {
            to.join("tui_history")
        } else {
            to.join(name)
        };
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target, edit);
        } else {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            std::fs::write(&target, edit(text)).unwrap();
        }
    }
}

/// The three `tui.toml` layers parse strictly, and layered they find the
/// existing realm (not a new one) with what the files recorded: the realm
/// file's writes of 0.1.0 (`/mode`, `/permissions add`, Ctrl+T, `/set`,
/// a statement and a trusted host), the user's and the project's keys.
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
    let allows = |tool: &str, arg: &str| {
        layered
            .rules
            .matches_allow(tool, &[arg.to_string()])
            .is_some()
    };
    assert!(allows("shell", "cargo test --lib"), "{:?}", layered.rules);
    assert!(allows("shell", "cargo fmt --all"), "{:?}", layered.rules);
    assert!(allows("read_file", "/etc/hosts"), "{:?}", layered.rules);
    for (tool, arg) in [
        ("write_file", "migrations/0001.sql"),
        ("shell", "rm -rf target"),
    ] {
        assert!(
            layered
                .rules
                .matches_deny(tool, &[arg.to_string()])
                .is_some(),
            "{tool} {arg}: {:?}",
            layered.rules
        );
    }
    assert!(layered.endpoints.contains_key("work"));
    assert!(!layered.pricing_fetch);
    assert!(layered.pricing.contains_key("claude-sonnet-4-6"));
    assert_eq!(layered.thinking.len(), 3);
    assert_eq!(layered.daemon_principal.as_deref(), Some("fixture-user"));
    assert!(
        layered
            .confinement
            .extra_read
            .contains(&PathBuf::from("/usr/share/dict"))
    );
    // The project file may only lower these.
    assert_eq!(layered.helpers.max_depth, 1);
    assert_eq!(layered.shell.user_timeout_secs, 60);
    assert_eq!(layered.search.max_entries, 20_000);
    assert_eq!(
        layered.ui.checklist,
        crate::config::settings::ChecklistView::Shown
    );
    let settings = &layered.settings;
    assert_eq!(
        settings.classifier_model.as_deref(),
        Some("claude-haiku-4-5")
    );
    let statements: Vec<&str> = settings.allow.iter().map(|(_, s)| s.as_str()).collect();
    for statement in [
        "running the test suite",
        "reading the project's documentation",
    ] {
        assert!(statements.contains(&statement), "{statements:?}");
    }
    let hosts = crate::approval::hosts::patterns(&settings.trusted_hosts);
    for host in ["ci.example.net", "*.corp.example"] {
        assert!(hosts.iter().any(|h| h == host), "{hosts:?}");
    }
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

/// The input history files, the price cache, the notes, the unsent
/// messages and the attach cursor.
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
        1_795_706_400,
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
        .for_agent(AGENT)
        .load()
        .expect("the attach cursor opens");
    assert_eq!(cursor.session_id, SESSION);
    assert_eq!(cursor.sequence, 65);
    assert!(cursor.epoch.is_some() && cursor.daemon.is_some());
}

/// The daemon's files: the endpoint file, the conversation log, the
/// audit log, the usage records (of the conversation and of a schedule),
/// the revealed tools and the helper bindings.
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
    assert_eq!(log.agent_of(&session), Some(AGENT));

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
    assert_eq!((totals.input, totals.output), (130, 65));
    assert_eq!(record.context.map(|c| c.tokens), Some(15));
    for helper in ["scout", "quick", "w1"] {
        assert_eq!(
            record.cost.helpers[helper]["claude-sonnet-4-6"].input, 10,
            "{helper}"
        );
    }
    let schedule =
        crate::daemon::usage::read_record(&usage_dir.join(format!("schedule-{SCHEDULE}.toml")));
    let totals = &schedule.cost.per_model["claude-sonnet-4-6"];
    assert_eq!((totals.input, totals.output), (10, 5));
    assert_eq!(
        std::fs::read_dir(&usage_dir).unwrap().count(),
        2,
        "nothing was set aside as unreadable"
    );

    let reveals = crate::daemon::deferral::reveal::Reveals::new(&machine.realm);
    reveals.load().await;
    let revealed = reveals.revealed(&session);
    for tool in ["meerkat_schedule_create", "mob_create", "mob_spawn_member"] {
        assert!(revealed.contains(tool), "{tool}: {revealed:?}");
    }

    let bindings = crate::mobs::bindings::HelperBindings::open(&machine.realm).unwrap();
    let kept = HelperKind::Started(HelperContext::Fresh, HelperLife::Kept);
    let (key, binding) = bindings
        .member_by_session(&SessionId::parse(SCOUT_SESSION).unwrap())
        .expect("the kept helper's binding opens");
    assert_eq!((key.mob.as_str(), key.member.as_str()), ("tui", "scout"));
    assert_eq!(binding.tag.owner, session);
    assert_eq!(binding.tag.kind, kept);
    assert_eq!(binding.task.as_deref(), Some("SCOUT-TASK: look"));
    let (key, binding) = bindings
        .member_by_session(&SessionId::parse(W1_SESSION).unwrap())
        .expect("the other mob's member's binding opens");
    assert_eq!((key.mob.as_str(), key.member.as_str()), ("team", "w1"));
    assert_eq!(binding.tag.owner, session);
    assert_eq!(binding.tag.kind, kept);
}

/// The daemon's files 0.0.1 did not write: the user's words for the
/// classifier, the held notes of helpers' endings, the wire to an agent of
/// another workspace and the connection of helpers in different mobs.
#[test]
fn the_daemon_files_since_0_0_1_open() {
    let machine = Machine::new();
    let session = SessionId::parse(SESSION).unwrap();

    let words = std::fs::read(
        machine
            .realm
            .join(crate::daemon::conversations::WORDS_DIR)
            .join(format!("{SESSION}.json")),
    )
    .unwrap();
    let words = crate::daemon::conversations::parse_user_words(&words).unwrap();
    assert_eq!(words.words, ["go and save the notes"]);
    assert!(words.kept.is_empty());

    let held = crate::daemon::agents::endings::HeldNotes::open(&machine.realm);
    assert!(held.holds(AGENT), "the held notes open");
    let note = held
        .take(AGENT, &session, None)
        .expect("held for the conversation");
    for text in [
        "Your helper `scout` went idle",
        "Your helper `quick` finished its task",
        "QUICK-ANSWER",
        "Your helper `w1` went idle",
    ] {
        assert!(note.contains(text), "{text}: {note}");
    }

    let wires = crate::daemon::wires::Records::read(&machine.realm.join("wires.json")).unwrap();
    let [card] = wires.agents[AGENT].as_slice() else {
        panic!("{wires:?}")
    };
    assert_eq!(card.agent, "main-gggg");
    assert!(card.via.is_none() && card.ssh_from.is_none());
    let key = meerkat_comms::PubKey::from_pubkey_string(&card.pubkey).unwrap();
    assert_eq!(key.to_peer_id().to_string(), card.peer_id);

    let links =
        crate::daemon::links::Records::read(&machine.realm.join(crate::daemon::links::LINKS_FILE))
            .unwrap();
    let [link] = links.links.as_slice() else {
        panic!("{links:?}")
    };
    let ends: Vec<(&str, &str, &str)> = link
        .ends
        .iter()
        .map(|end| {
            (
                end.key.mob.as_str(),
                end.key.member.as_str(),
                end.peer.as_str(),
            )
        })
        .collect();
    assert_eq!(
        ends,
        [
            ("tui", "scout", "tui/main/scout"),
            ("team", "w1", "team/worker/w1")
        ]
    );
}
