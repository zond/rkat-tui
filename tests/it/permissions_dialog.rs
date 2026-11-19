//! The `/permissions` dialog's In force additions through the daemon: a
//! statement typed with `n` is in the classifier's environment from the
//! next review on, the agent is told once, and an "always" one is written
//! to the realm's or the user's own `tui.toml` by its scope; a rule made
//! with `r` is written to its file and applies at once.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::path::Path;
use std::sync::Arc;

use common::{ScriptedLlmClient, Step, Workspace, answers, leak};
use meerkat_core::Message;
use parking_lot::Mutex;
use rig::{Attached, Setup};
use rkat_tui::approval::classifier::{
    ActionClassifier, ClassifierError, ClassifierInput, ClassifierVerdict,
};
use rkat_tui::approval::grants::GrantScope;
use rkat_tui::approval::mode::PermissionMode;
use rkat_tui::ui::{Effect, Msg, PickerKind, RuleForm, Section};
use serde_json::json;

/// Allows every call, recording the statements of each review's
/// environment and the tool reviewed.
#[derive(Default)]
struct Recorder {
    /// One entry per review: its tool and the statements it read.
    reviews: Mutex<Vec<(String, Vec<String>)>>,
}

#[async_trait::async_trait]
impl ActionClassifier for Recorder {
    async fn classify(
        &self,
        input: ClassifierInput<'_>,
    ) -> Result<ClassifierVerdict, ClassifierError> {
        self.reviews
            .lock()
            .push((input.tool.to_string(), input.environment.statements.clone()));
        Ok(ClassifierVerdict::Allow {
            reason: "recorded".into(),
        })
    }
}

/// The shell call `id` touching `path` (outside the project: reviewed).
fn touch(id: &'static str, path: &Path) -> Step {
    Step::Tool {
        id,
        name: "shell",
        args: json!({"command": format!("touch {}", path.display())}),
    }
}

/// A turn that touches `path` once, then ends: the step for each request
/// of the script, by whether it answers the call's result.
fn touch_then_done(path: &Path) -> Step {
    let path = path.to_path_buf();
    Step::Dynamic(Arc::new(move |messages: &[Message]| {
        if answers(messages, None) {
            Step::Text("done")
        } else {
            let n = messages
                .iter()
                .filter(|m| matches!(m, Message::ToolResults { .. }))
                .count();
            touch(leak(format!("t{n}")), &path)
        }
    }))
}

/// The notice that starts with `start`, once the client shows it.
async fn notice(rig: &mut Attached, start: &str) -> String {
    let start = start.to_string();
    let msg = rig
        .wait_for(move |msg| matches!(msg, Msg::Notice { text, .. } if text.starts_with(&start)))
        .await;
    let Msg::Notice { text, .. } = msg else {
        unreachable!()
    };
    text
}

/// The In force rows of the next refreshed dialog, as text.
async fn in_force(rig: &mut Attached) -> Vec<String> {
    let msg = rig
        .wait_for(|msg| {
            matches!(msg, Msg::ShowPicker { picker, refresh: true } if picker.kind == PickerKind::Permissions)
        })
        .await;
    let Msg::ShowPicker { picker, .. } = msg else {
        unreachable!()
    };
    picker
        .section_rows(Section::InForce.index())
        .unwrap()
        .iter()
        .map(|row| row.text.clone())
        .collect()
}

/// A statement from `n` for this session reaches the classifier's
/// environment at the next review and the agent's next request (one
/// note), shows in In force at once, and is written nowhere; one for
/// always is written to the realm's `tui.toml`, one for every workspace
/// to the user's own; "anything" is refused with its reason, on the
/// dialog too, and adds nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_statement_typed_with_n_reaches_the_classifier_and_its_file() {
    const SESSION: &str = "pushing to my own fork of this repository";
    const ALWAYS: &str = "building the project after a dependency change";
    const EVERYWHERE: &str = "network requests to api.example.com";
    let ws = Workspace::new();
    let path = ws.sibling("n-marker");
    let llm = ScriptedLlmClient::new(vec![touch_then_done(&path); 4]);
    let recorder = Arc::new(Recorder::default());
    let mut rig = Attached::start_with(
        &ws.root,
        &llm,
        Setup::default().classifier(Arc::clone(&recorder) as Arc<dyn ActionClassifier>),
    )
    .await;
    rig.set_mode(PermissionMode::Auto).await;
    rig.send(Effect::PickedStatement {
        scope: GrantScope::Session,
        text: SESSION.into(),
    });
    let text = notice(&mut rig, "allowed").await;
    assert_eq!(
        text,
        format!(
            "allowed for this session: \"{SESSION}\" (judged by the classifier, read narrowly)"
        )
    );
    let rows = in_force(&mut rig).await;
    assert!(
        rows.contains(&format!("allow kind \"{SESSION}\" · session")),
        "{rows:?}"
    );
    rig.wait_for(|msg| {
        matches!(msg, Msg::PickerDone { kind: PickerKind::Permissions, text } if text.starts_with("allowed for this session"))
    })
    .await;
    // The next review reads it.
    rig.send(Effect::StartTurn("touch the marker".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "done");
    let reviews = recorder.reviews.lock().clone();
    let (tool, statements) = reviews.last().unwrap_or_else(|| panic!("no review"));
    assert_eq!(tool, "shell");
    assert_eq!(statements, &vec![SESSION.to_string()]);
    // The agent was told, in one note.
    let last = format!("{:?}", llm.recorded().last().unwrap().messages);
    assert_eq!(
        last.matches("The user now allows").count(),
        1,
        "the note is in the agent's context once: {last}"
    );
    assert!(last.contains(SESSION), "{last}");
    let realm_toml = common::realm_of(&ws.root).join("tui.toml");
    let user_toml = common::config_dir_for(&ws.root).join("tui.toml");
    let read = |file: &Path| std::fs::read_to_string(file).unwrap_or_default();
    assert!(
        !read(&realm_toml).contains(SESSION),
        "{}",
        read(&realm_toml)
    );
    assert!(!read(&user_toml).contains(SESSION), "{}", read(&user_toml));

    // Always for this workspace: the realm's file; in every workspace:
    // the user's own.
    rig.send(Effect::PickedStatement {
        scope: GrantScope::Always,
        text: ALWAYS.into(),
    });
    notice(&mut rig, "allowed always").await;
    let rows = in_force(&mut rig).await;
    assert!(
        rows.contains(&format!("allow kind \"{ALWAYS}\" · always")),
        "{rows:?}"
    );
    rig.send(Effect::PickedStatement {
        scope: GrantScope::AlwaysUser,
        text: EVERYWHERE.into(),
    });
    notice(&mut rig, "allowed always").await;
    in_force(&mut rig).await;
    let realm: toml::Table = toml::from_str(&read(&realm_toml)).unwrap();
    assert_eq!(
        realm["auto"]["allow"],
        toml::Value::Array(vec![toml::Value::String(ALWAYS.into())])
    );
    let user: toml::Table = toml::from_str(&read(&user_toml)).unwrap();
    assert_eq!(
        user["auto"]["allow"],
        toml::Value::Array(vec![toml::Value::String(EVERYWHERE.into())])
    );
    rig.send(Effect::StartTurn("touch it again".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "done");
    let reviews = recorder.reviews.lock().clone();
    let statements = &reviews.last().unwrap().1;
    for statement in [SESSION, ALWAYS, EVERYWHERE] {
        assert!(
            statements.iter().any(|s| s == statement),
            "{statement}: {statements:?}"
        );
    }
    // "anything" is refused: the notice and the dialog say why, nothing
    // is added.
    rig.send(Effect::PickedStatement {
        scope: GrantScope::Session,
        text: "anything".into(),
    });
    let msg = rig
        .wait_for(|msg| {
            matches!(
                msg,
                Msg::PickerRefused {
                    kind: PickerKind::Permissions,
                    ..
                }
            )
        })
        .await;
    let Msg::PickerRefused { why, .. } = msg else {
        unreachable!()
    };
    assert!(why.contains("anything"), "{why}");
    assert_eq!(
        rig.daemon
            .engine()
            .statements()
            .rows(Some(&rig.daemon.session))
            .len(),
        3
    );
    let audit = std::fs::read_to_string(common::realm_of(&ws.root).join("audit.jsonl")).unwrap();
    assert_eq!(audit.matches("statement_added").count(), 3, "{audit}");
    rig.quit().await;
}

/// A rule from `r` is written to the realm's `tui.toml` (or with "your
/// tui.toml" to the user's own) and applies at once: a literal allow rule
/// lets its exact call run with no review, a deny rule is in force; a
/// pattern that does not parse is refused with the reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rule_made_with_r_is_written_and_applies_at_once() {
    let ws = Workspace::new();
    let path = ws.sibling("r-marker");
    let llm = ScriptedLlmClient::new(vec![touch_then_done(&path); 2]);
    let recorder = Arc::new(Recorder::default());
    let mut rig = Attached::start_with(
        &ws.root,
        &llm,
        Setup::default().classifier(Arc::clone(&recorder) as Arc<dyn ActionClassifier>),
    )
    .await;
    rig.set_mode(PermissionMode::Auto).await;
    let command = format!("touch {}", path.display());
    rig.send(Effect::PickedRule(RuleForm {
        deny: false,
        tool: "shell".into(),
        pattern: Some(command.clone()),
        literal: true,
        user: false,
    }));
    assert_eq!(notice(&mut rig, "rule added").await, "rule added");
    let rows = in_force(&mut rig).await;
    assert!(
        rows.iter()
            .any(|row| row.ends_with(&format!("allow shell literal `{command}` · realm"))),
        "{rows:?}"
    );
    let realm: toml::Table = toml::from_str(
        &std::fs::read_to_string(common::realm_of(&ws.root).join("tui.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        realm["allow"][0]["literal"].as_str(),
        Some(command.as_str()),
        "{realm:?}"
    );
    // At once: the exact call runs with no review.
    rig.send(Effect::StartTurn("touch the marker".into()));
    assert_eq!(rig.turn_finished().await.unwrap().text, "done");
    assert!(path.exists());
    assert!(
        recorder.reviews.lock().is_empty(),
        "{:?}",
        recorder.reviews.lock()
    );
    // A deny rule in the user's own file.
    rig.send(Effect::PickedRule(RuleForm {
        deny: true,
        tool: "shell".into(),
        pattern: Some("git push*".into()),
        literal: false,
        user: true,
    }));
    assert_eq!(
        notice(&mut rig, "rule added").await,
        "rule added to your tui.toml (every workspace)"
    );
    in_force(&mut rig).await;
    let user: toml::Table = toml::from_str(
        &std::fs::read_to_string(common::config_dir_for(&ws.root).join("tui.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(user["deny"][0]["arg"].as_str(), Some("git push*"));
    let rules = rig.daemon.engine().rules();
    assert!(
        rules
            .deny()
            .any(|(_, rule)| format!("{rule:?}").contains("git push*")),
        "in force at once"
    );
    // A pattern that does not parse: refused, on the dialog too.
    rig.send(Effect::PickedRule(RuleForm {
        deny: false,
        tool: "shell".into(),
        pattern: Some("[unclosed".into()),
        literal: false,
        user: false,
    }));
    let msg = rig
        .wait_for(|msg| {
            matches!(
                msg,
                Msg::PickerRefused {
                    kind: PickerKind::Permissions,
                    ..
                }
            )
        })
        .await;
    let Msg::PickerRefused { why, .. } = msg else {
        unreachable!()
    };
    assert!(why.starts_with("rule not added"), "{why}");
    rig.quit().await;
}
