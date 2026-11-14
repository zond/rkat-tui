//! `/auto check` (`rkat/classifier_check`) through the daemon with a
//! scripted classifier: the totals of a scripted mix of verdicts, the
//! bounds (how many cases, one check at a time, dropped when its client
//! leaves), a provider error reported as one, nothing reaching a session's
//! counters, the audit line, the client's notice and the
//! `--classifier-check` exit code. No model is called.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;
use crate::rig;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{ScriptedLlmClient, Workspace, realm_of, until, until_async, within};
use rig::{Attached, Running, ScriptedClassifier, Setup};
use rkat_tui::approval::classifier::{
    ActionClassifier, ClassifierError, ClassifierInput, ClassifierVerdict,
};
use rkat_tui::approval::corpus::{Limit, Reading, Selection};
use rkat_tui::client::transport::Connection;
use rkat_tui::daemon::protocol::{self, ClassifierChecked};
use rkat_tui::ui::{Effect, Msg};
use serde_json::json;

fn block(rule: &str) -> Result<ClassifierVerdict, ClassifierError> {
    Ok(ClassifierVerdict::Block {
        rule: Some(rule.into()),
        reason: "scripted".into(),
        annotation: None,
        path: None,
    })
}

fn allow() -> Result<ClassifierVerdict, ClassifierError> {
    Ok(ClassifierVerdict::allow("scripted"))
}

fn setup(classifier: Arc<dyn ActionClassifier>) -> Setup {
    Setup {
        classifier_model: Some("m-scripted".into()),
        ..Setup::default().classifier(classifier)
    }
}

async fn start(ws: &Workspace, setup: &Setup) -> Running {
    let llm = ScriptedLlmClient::new(vec![]);
    Running::start_with(&setup.command_line(&ws.root), setup.options(&ws.root, &llm)).await
}

async fn check(
    connection: &Connection,
    selection: &Selection,
) -> Result<ClassifierChecked, String> {
    connection
        .call(
            "rkat/classifier_check",
            serde_json::to_value(selection).unwrap(),
        )
        .await
        .map(|answer| serde_json::from_value(answer).unwrap())
        .map_err(|err| err.to_string())
}

fn ids(ids: &[&str]) -> Selection {
    Selection {
        cases: ids.iter().map(ToString::to_string).collect(),
        ..Selection::default()
    }
}

/// A scripted mix: a routine case wrongly blocked, a harmful one caught,
/// a scope pair with its unnamed half missed. The totals count each, the
/// session counters and review numbers stay at zero, and one audit line
/// records the totals.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_totals_of_a_scripted_mix_and_nothing_reaches_a_session() {
    let ws = Workspace::new();
    let classifier =
        ScriptedClassifier::new(vec![block("T3"), block("D2"), allow(), allow(), allow()]);
    let daemon = start(&ws, &setup(Arc::clone(&classifier) as _)).await;
    let connection = daemon.connect().await;
    let checked = within(check(
        &connection,
        &ids(&[
            "a05-test-suite-with-temp-files",
            "b01-force-push-main",
            "s01a-delete-old-checkout-named",
            "s01b-delete-old-checkout-unnamed",
            "a02-redirect-into-shared-tmp",
        ]),
    ))
    .await
    .unwrap();
    assert_eq!(checked.model, "m-scripted");
    let report = &checked.report;
    let totals = report.totals();
    assert_eq!(
        (
            totals.cases,
            totals.wrong_blocks,
            totals.routine,
            totals.misses,
            totals.harmful,
            totals.pairs_wrong,
            totals.pairs
        ),
        (5, 1, 2, 0, 1, 1, 1)
    );
    let wrong: Vec<&str> = report.wrong().map(|o| o.id.as_str()).collect();
    assert_eq!(
        wrong,
        [
            "a05-test-suite-with-temp-files",
            "s01b-delete-old-checkout-unnamed"
        ]
    );
    assert_eq!(report.outcomes[0].rule.as_deref(), Some("T3"));
    assert_eq!(
        report.reading(),
        Reading::Outside,
        "1 wrong block in 5 cases"
    );
    // The classifier saw each case as a review shows it: the user's words
    // and the fixed machine's paths.
    let seen = classifier.seen();
    assert_eq!(seen.len(), 5);
    assert_eq!(seen[0].1, json!({"command": "cargo test"}));
    assert!(
        seen[0].2.contains("Fix the failing parser test"),
        "{}",
        seen[0].2
    );
    assert_eq!(seen[2].1, json!({"command": "rm -rf /home/dev/old-proj"}));
    // No session moved.
    let engine = daemon.engine();
    let (counters, pause) = engine.auto_state();
    assert_eq!(
        counters,
        rkat_tui::approval::counters::DenialCounters::default()
    );
    assert!(pause.is_none());
    assert_eq!(engine.review_stats(None).0.reviews, 0);
    let messages = daemon.transcript(&daemon.current_session().await).await;
    assert!(
        !format!("{messages:?}").contains("old-proj"),
        "the conversation is untouched"
    );
    // One audit line with the totals.
    let audit = realm_of(&ws.root).join("audit.jsonl");
    until(|| {
        std::fs::read_to_string(&audit).is_ok_and(|text| {
            text.contains("\"classifier_check\"")
                && text.contains("wrongly blocked 1 of 2 routine actions")
        })
    })
    .await;
    daemon.stop().await;
}

/// A count sends that many cases, the quick sample thirty; a provider
/// error stops the check and is reported as an error, not as a block.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_case_bound_and_a_provider_error() {
    let ws = Workspace::new();
    let classifier = ScriptedClassifier::allowing(vec![]);
    let daemon = start(&ws, &setup(Arc::clone(&classifier) as _)).await;
    let connection = daemon.connect().await;
    let five = within(check(
        &connection,
        &Selection {
            limit: Limit::Count(5),
            ..Selection::default()
        },
    ))
    .await
    .unwrap();
    assert_eq!((five.report.planned, five.report.outcomes.len()), (5, 5));
    assert_eq!(classifier.seen().len(), 5);
    let quick = within(check(&connection, &Selection::default()))
        .await
        .unwrap();
    assert_eq!(quick.report.outcomes.len(), 30);
    assert_eq!(classifier.seen().len(), 35);
    daemon.stop().await;

    let ws = Workspace::new();
    let failing = ScriptedClassifier::new(vec![allow()]);
    let daemon = start(&ws, &setup(Arc::clone(&failing) as _)).await;
    let connection = daemon.connect().await;
    let stopped = within(check(&connection, &Selection::default()))
        .await
        .unwrap();
    assert_eq!(stopped.report.outcomes.len(), 1);
    assert_eq!(stopped.report.reading(), Reading::Incomplete);
    let text = stopped.report.render(&stopped.model);
    assert!(
        text.contains("script exhausted") && text.contains("not a block"),
        "{text}"
    );
    assert_eq!(stopped.report.totals().wrong_blocks, 0);
    daemon.stop().await;
}

/// A classifier that waits for the test before each verdict.
struct Gated {
    entered: AtomicUsize,
    open: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl ActionClassifier for Gated {
    async fn classify(
        &self,
        _input: ClassifierInput<'_>,
    ) -> Result<ClassifierVerdict, ClassifierError> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        // One permit, one verdict.
        self.open.acquire().await.unwrap().forget();
        allow()
    }
}

/// One check at a time: a second is refused while the first runs; the
/// first is dropped when its client leaves (no further case is sent), and
/// then another may start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_check_at_a_time_and_a_departed_clients_check_is_dropped() {
    let ws = Workspace::new();
    let gated = Arc::new(Gated {
        entered: AtomicUsize::new(0),
        open: tokio::sync::Semaphore::new(0),
    });
    let daemon = start(&ws, &setup(Arc::clone(&gated) as _)).await;
    let first = daemon.connect().await;
    let running = tokio::spawn(async move {
        let answer = check(&first, &Selection::default()).await;
        drop(first);
        answer
    });
    until(|| gated.entered.load(Ordering::SeqCst) == 1).await;
    let second = daemon.connect().await;
    let refused = within(check(&second, &Selection::default()))
        .await
        .unwrap_err();
    assert!(
        refused.contains(&protocol::CHECK_REFUSED.to_string())
            && refused.contains("already running"),
        "{refused}"
    );
    // The first client leaves mid-check.
    running.abort();
    let _ = running.await;
    // One verdict every 20 ms: a check that went on would take its thirty
    // cases' 600 ms before another could start.
    let feeder = tokio::spawn({
        let gated = Arc::clone(&gated);
        async move {
            loop {
                gated.open.add_permits(1);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    });
    until_async(async || check(&second, &ids(&["b01-force-push-main"])).await.is_ok()).await;
    feeder.abort();
    let entered = gated.entered.load(Ordering::SeqCst);
    assert!(
        entered < 10,
        "the dropped check sent no more cases ({entered} classifier calls)"
    );
    daemon.stop().await;
}

/// Without a classifier model the check says so and how to set one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_classifier_model_the_check_says_how_to_configure_one() {
    let ws = Workspace::new();
    let daemon = start(&ws, &Setup::default()).await;
    let connection = daemon.connect().await;
    let refused = within(check(&connection, &Selection::default()))
        .await
        .unwrap_err();
    assert!(refused.contains("[auto] classifier_model"), "{refused}");
    daemon.stop().await;
}

/// `/auto check` in the client: it says what it sends and what that
/// costs, then shows the report as a notice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_client_announces_the_check_and_shows_the_report() {
    let ws = Workspace::new();
    let classifier = ScriptedClassifier::allowing(vec![block("T3").unwrap()]);
    let llm = ScriptedLlmClient::new(vec![]);
    let mut rig = Attached::start_with(&ws.root, &llm, setup(classifier as _)).await;
    rig.send(Effect::ClassifierCheck(Selection {
        limit: Limit::Count(3),
        ..Selection::default()
    }));
    rig.wait_for(|msg| {
        matches!(msg, Msg::Notice { text, .. } if text.contains("sending 3 corpus cases") && text.contains("at your cost"))
    })
    .await;
    rig.wait_for(|msg| {
        matches!(msg, Msg::Notice { text, .. } if text.starts_with("classifier m-scripted: 3 cases; wrongly blocked 1 of"))
    })
    .await;
    rig.quit().await;
}

/// `--classifier-check`: the report on its output, and the exit code: 0
/// within the targets, 3 outside them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_command_line_form_exits_non_zero_outside_the_targets() {
    let ws = Workspace::new();
    let classifier = ScriptedClassifier::allowing(vec![
        block("T3").unwrap(),
        block("D1").unwrap(),
        allow().unwrap(),
        block("D2").unwrap(),
    ]);
    let setup = setup(classifier as _);
    let daemon = start(&ws, &setup).await;
    let files = [setup.endpoint_file(&ws.root)];
    let mut out = Vec::new();
    let reading = rkat_tui::client::check::check(
        &files,
        None,
        &Selection {
            limit: Limit::Count(2),
            ..Selection::default()
        },
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(reading, Reading::Outside);
    let err = rkat_tui::client::check::exit_for(reading).unwrap_err();
    assert_eq!(err.exit_code(), 3);
    let printed = String::from_utf8(out).unwrap();
    assert!(
        printed.starts_with("classifier m-scripted: 2 cases; wrongly blocked 1 of 1"),
        "{printed}"
    );
    let reading = rkat_tui::client::check::check(
        &files,
        None,
        &ids(&["a05-test-suite-with-temp-files", "b01-force-push-main"]),
        &mut Vec::new(),
    )
    .await
    .unwrap();
    assert!(
        rkat_tui::client::check::exit_for(reading).is_ok(),
        "{reading:?}"
    );
    daemon.stop().await;
}
