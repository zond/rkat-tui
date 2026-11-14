//! The classifier check as the client asks for it: `/auto check` in the
//! TUI ([`crate::client::runner`]) and `rkat-tui --classifier-check` for
//! scripts ([`run`]), both through the daemon's `rkat/classifier_check`
//! ([`crate::approval::ApprovalEngine::check_classifier`]), so the model,
//! endpoint, key and dialect are the daemon's own, exactly as its reviews
//! use them. What is printed is [`crate::approval::corpus::Report::render`]
//! in both.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use super::ClientError;
use super::launch::{self, Decision};
use super::runner::SpawnPlan;
use super::transport::{ATTACH_TIMEOUT, Connection};
use crate::approval::corpus::{self, Reading, Selection};
use crate::cli::Args;
use crate::daemon::protocol::{self, ClassifierChecked, METHOD_NOT_FOUND, method};
use crate::error::TuiError;

/// How long a client waits for a check's answer: the daemon's own bound
/// on the whole check, and a minute for the answer to come back.
pub const CALL_BOUND: Duration = Duration::from_mins(21);

/// What the client says before it asks: how many cases go to the
/// classifier, and that each is a model call the user pays for.
#[must_use]
pub fn intro(cases: usize) -> String {
    format!(
        "/auto check: sending {cases} corpus case{} to the daemon's classifier, each a classifier call on your endpoint, at your cost; the result follows here",
        if cases == 1 { "" } else { "s" }
    )
}

/// The check's answer in the user's words, and how it reads against the
/// targets (`None`: it did not run).
#[must_use]
pub fn outcome(answer: &Result<ClassifierChecked, ClientError>) -> (Option<Reading>, String) {
    match answer {
        Ok(checked) => (
            Some(checked.report.reading()),
            checked.report.render(&checked.model),
        ),
        Err(ClientError::Rpc { code, .. }) if *code == METHOD_NOT_FOUND => (
            None,
            "/auto check: this daemon is older than the command; restart it (/quit, then start rkat-tui again)".to_string(),
        ),
        Err(ClientError::Rpc { message, .. }) => (None, format!("/auto check: {message}")),
        Err(err) => (None, format!("/auto check failed: {err}")),
    }
}

/// `rkat-tui --classifier-check[=quick|all|N]`: attach to the workspace's
/// daemon as a probe (starting it when none runs, as the TUI would, and
/// stopping it again afterwards), run the check, print the report on
/// stdout and the progress on stderr.
///
/// # Errors
///
/// [`TuiError::ClassifierCheck`] when the check could not run or stopped
/// early, [`TuiError::ClassifierOutside`] when it ran and the model is
/// outside the targets (the exit codes say which), and the workspace's
/// own errors.
pub async fn run(args: &Args, limit: corpus::Limit) -> Result<(), TuiError> {
    let layers = crate::bootstrap::read_layers(args)?;
    let config = super::configure_with(layers, args, args.attach_target().mode()).await?;
    let selection = Selection {
        limit,
        ..Selection::default()
    };
    let reading = check(
        &config.endpoint_files,
        config.spawn.as_ref(),
        &selection,
        &mut std::io::stdout(),
    )
    .await?;
    exit_for(reading)
}

/// How `--classifier-check` ends for `reading`: success within the
/// targets, [`TuiError::ClassifierOutside`] (exit code 3) outside them,
/// [`TuiError::ClassifierCheck`] (1) when it stopped early.
///
/// # Errors
///
/// As said.
pub fn exit_for(reading: Reading) -> Result<(), TuiError> {
    match reading {
        Reading::Within => Ok(()),
        Reading::Outside => Err(TuiError::ClassifierOutside),
        Reading::Incomplete => Err(TuiError::ClassifierCheck(
            "it stopped before its last case (see above)".to_string(),
        )),
    }
}

/// The check against the daemon of `files` (started from `spawn` when
/// none answers, and stopped afterwards then), `selection`'s cases, the
/// report written to `out`: how it reads.
///
/// # Errors
///
/// [`TuiError::ClassifierCheck`] when it could not run.
pub async fn check(
    files: &[PathBuf],
    spawn: Option<&SpawnPlan>,
    selection: &Selection,
    out: &mut (dyn Write + Send),
) -> Result<Reading, TuiError> {
    let failed = |err: &dyn std::fmt::Display| TuiError::ClassifierCheck(err.to_string());
    let cases = corpus::select(selection).map_err(|err| failed(&err))?.len();
    let mode = if spawn.is_some() {
        crate::cli::AttachMode::Auto
    } else {
        crate::cli::AttachMode::Existing
    };
    let (endpoint, started) = match (launch::decide(launch::probe_any(files).await, mode), spawn) {
        (Decision::Attach(endpoint), _) => (endpoint, false),
        (Decision::Start, Some(plan)) => {
            let (pid, mut exited) = launch::spawn_daemon(
                &plan.program,
                &launch::argv_for(&plan.argv, files),
                &plan.log,
                None,
            )
            .map_err(|err| failed(&err))?;
            eprintln!(
                "rkat-tui: started rkat-tui-mob (pid {pid}; output in {})",
                plan.log.display()
            );
            let endpoint =
                launch::wait_for_endpoint(files, launch::START_TIMEOUT, &mut exited, &plan.log)
                    .await
                    .map_err(|err| failed(&err))?;
            (endpoint, true)
        }
        (Decision::Start | Decision::Refuse, _) => {
            return Err(failed(&ClientError::NoDaemon {
                file: files.first().cloned().unwrap_or_default(),
            }));
        }
    };
    let (connection, mut notes) = Connection::attach_as(&endpoint, ATTACH_TIMEOUT, None, true)
        .await
        .map_err(|err| failed(&err))?;
    eprintln!(
        "rkat-tui: {}",
        intro(cases).trim_start_matches("/auto check: ")
    );
    // The progress, on stderr, as the daemon pushes it.
    let progress = tokio::spawn(async move {
        while let Some(note) = notes.recv().await {
            if note.method != protocol::APPROVAL_NOTE {
                continue;
            }
            if let Ok(protocol::ApprovalNote::CheckProgress {
                done,
                total,
                running: true,
            }) = serde_json::from_value(note.params)
            {
                eprint!("\rclassifier check {done}/{total}");
                if done == total {
                    eprintln!();
                }
            }
        }
    });
    let answer = tokio::time::timeout(
        CALL_BOUND,
        super::remote::call::<method::ClassifierCheck>(&connection, selection),
    )
    .await
    .unwrap_or(Err(ClientError::Timeout(
        "waiting for the classifier check",
    )));
    progress.abort();
    if started {
        // The daemon this check started has nobody else: it stops again.
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            super::remote::call::<method::Shutdown>(
                &connection,
                &protocol::ShutdownParams { retire: false },
            ),
        )
        .await;
    }
    let (reading, text) = outcome(&answer);
    match reading {
        Some(reading) => {
            writeln!(out, "{text}").map_err(|err| failed(&err))?;
            Ok(reading)
        }
        None => Err(TuiError::ClassifierCheck(
            text.trim_start_matches("/auto check: ").to_string(),
        )),
    }
}
