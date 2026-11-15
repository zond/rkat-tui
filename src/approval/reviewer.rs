//! The classifier slot: the classifier the reviews use now, its model, the
//! checks' classifier, the start-up check (the probe) and `/auto check`,
//! and `/auto model`'s live swap. The engine holds one
//! ([`super::ApprovalEngine::reviewer`]) and takes [`Reviewer::current`]
//! for each review; a review that started finishes on the classifier it
//! took.
//!
//! Locks: each field's own, never two at once except `check` then
//! `classifier` in [`Reviewer::checker`] (nothing takes them the other
//! way round), and none of them across an await.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::{Mutex, RwLock};

use super::ApprovalEvent;
use super::audit::Outlets;
use super::classifier::ActionClassifier;
use super::corpus;

/// Why a classifier check ([`Reviewer::check`]) did not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CheckError {
    /// No classifier model is configured.
    #[error(
        "no classifier model is configured, so there is nothing to check: set `[auto] classifier_model = \"<model>\"` in tui.toml (the model of the endpoint the agent uses), then restart the daemon"
    )]
    NotConfigured,
    /// Another check runs.
    #[error("a classifier check is already running; wait for its result")]
    Busy,
}

/// What [`ClassifierBuilder`] builds: the reviews' classifier and the
/// checks' (the same model and policy, stage 2 without its tools).
pub struct BuiltClassifier {
    /// The reviews'.
    pub review: Arc<dyn ActionClassifier>,
    /// The checks'.
    pub check: Arc<dyn ActionClassifier>,
}

/// Builds the classifier for a model of an endpoint (`/auto model`), or
/// says why it cannot.
pub type ClassifierBuilder = Arc<
    dyn Fn(
            crate::endpoints::Endpoint,
            String,
        ) -> futures::future::BoxFuture<'static, Result<BuiltClassifier, String>>
        + Send
        + Sync,
>;

/// The classifier slot (see the [module docs](self)).
pub struct Reviewer {
    /// The classifier the reviews use now; `/auto model` replaces it
    /// ([`Self::switch`]).
    classifier: RwLock<Arc<dyn ActionClassifier>>,
    /// The classifier's model, and whether the start-up check should run
    /// against it ([`Self::set_model`]).
    model: RwLock<Option<(String, bool)>>,
    /// What the start-up check found wrong, for the clients that attach
    /// later ([`Self::probe_notice`]).
    probe_notice: Mutex<Option<String>>,
    /// The classifier the checks send their cases to: the review's, with
    /// stage 2's file-reading tools off ([`Self::set_check_classifier`]);
    /// unset, the review's own.
    check: RwLock<Option<Arc<dyn ActionClassifier>>>,
    /// Builds the classifier for another model (`/auto model`); unset, the
    /// model cannot be changed live ([`Self::set_builder`]).
    builder: OnceLock<ClassifierBuilder>,
    /// A classifier check runs ([`Self::check`]).
    check_running: AtomicBool,
    outlets: Outlets,
}

/// A running check: when it ends, or is dropped unfinished (its client
/// left), another may start, the clients hear it ended, and a dropped one
/// leaves its audit line.
struct CheckRun<'a> {
    reviewer: &'a Reviewer,
    model: String,
    done: AtomicUsize,
    total: usize,
    finished: AtomicBool,
}

impl Drop for CheckRun<'_> {
    fn drop(&mut self) {
        let done = self.done.load(Ordering::Relaxed);
        if !self.finished.load(Ordering::Relaxed)
            && let Some(audit) = self.reviewer.outlets.audit()
        {
            audit.event(
                "classifier_check",
                format!(
                    "{}: cancelled after {done} of {} cases (its client left)",
                    self.model, self.total
                ),
            );
        }
        self.reviewer.outlets.emit(ApprovalEvent::CheckProgress {
            done,
            total: self.total,
            running: false,
        });
        self.reviewer.check_running.store(false, Ordering::SeqCst);
    }
}

impl Reviewer {
    pub(super) fn new(classifier: Arc<dyn ActionClassifier>, outlets: Outlets) -> Self {
        Self {
            classifier: RwLock::new(classifier),
            model: RwLock::new(None),
            probe_notice: Mutex::new(None),
            check: RwLock::new(None),
            builder: OnceLock::new(),
            check_running: AtomicBool::new(false),
            outlets,
        }
    }

    /// The classifier a review starts on now.
    pub(super) fn current(&self) -> Arc<dyn ActionClassifier> {
        Arc::clone(&*self.classifier.read())
    }

    /// The checks ([`Self::check`], [`Self::probe`]) send their cases to
    /// `classifier`: the review's own model, endpoint and policy, with
    /// stage 2's file-reading tools off. Set once.
    pub fn set_check_classifier(&self, classifier: Arc<dyn ActionClassifier>) {
        *self.check.write() = Some(classifier);
    }

    /// The classifier the checks use.
    fn checker(&self) -> Arc<dyn ActionClassifier> {
        self.check
            .read()
            .clone()
            .unwrap_or_else(|| Arc::clone(&*self.classifier.read()))
    }

    /// How `/auto model` builds the classifier for another model: set once
    /// by the daemon's composition.
    pub fn set_builder(&self, builder: ClassifierBuilder) {
        let _ = self.builder.set(builder);
    }

    /// `/auto model`: review with `model` of `endpoint` from now on, live
    /// (a review that started finishes on the old one), named `label` (the
    /// `endpoint:model` reference) in `/status`, `/auto stats` and the
    /// checks; audited (the setting's file is written by the daemon's
    /// settings path, `crate::daemon::settings`). The caller runs the
    /// start-up check against it ([`Self::probe_switched`]). Returns the
    /// notice.
    ///
    /// # Errors
    ///
    /// Why the model was not changed, for the user.
    pub async fn switch(
        &self,
        endpoint: crate::endpoints::Endpoint,
        model: String,
        label: String,
    ) -> Result<String, String> {
        let Some(builder) = self.builder.get() else {
            return Err("this daemon's classifier is fixed (a test's or an embedder's); the model cannot be changed".into());
        };
        let built = builder(endpoint, model).await?;
        let previous = self.model();
        *self.classifier.write() = built.review;
        *self.check.write() = Some(built.check);
        *self.model.write() = Some((label.clone(), true));
        *self.probe_notice.lock() = None;
        self.outlets.audit_event(
            "classifier_model",
            format!("{} -> {label}", previous.as_deref().unwrap_or("none")),
        );
        Ok(format!(
            "the classifier now reviews with {label}; its start-up check runs now, and /auto check measures it on more cases"
        ))
    }

    /// `/auto check`: send `cases` (a [`corpus::select`]ion) through the
    /// configured classifier as a review shows them ([`corpus::evaluate`]),
    /// within `bounds`, and answer its model and the report. Each judged
    /// case is pushed as [`ApprovalEvent::CheckProgress`]; one audit line
    /// (`classifier_check`) records the totals, or how far a check got
    /// that was dropped (its client left). Outside every session: no
    /// counter, pause, statistic, cache or conversation moves.
    ///
    /// # Errors
    ///
    /// [`CheckError::NotConfigured`] without a classifier model,
    /// [`CheckError::Busy`] while another check runs.
    pub async fn check(
        &self,
        cases: &[corpus::Case],
        bounds: corpus::Bounds,
    ) -> Result<(String, corpus::Report), CheckError> {
        let model = self.model().ok_or(CheckError::NotConfigured)?;
        if self.check_running.swap(true, Ordering::SeqCst) {
            return Err(CheckError::Busy);
        }
        let run = CheckRun {
            reviewer: self,
            model: model.clone(),
            done: AtomicUsize::new(0),
            total: cases.len(),
            finished: AtomicBool::new(false),
        };
        self.outlets.emit(ApprovalEvent::CheckProgress {
            done: 0,
            total: cases.len(),
            running: true,
        });
        let checker = self.checker();
        let report = corpus::evaluate(&*checker, cases, bounds, |done, total| {
            run.done.store(done, Ordering::Relaxed);
            self.outlets.emit(ApprovalEvent::CheckProgress {
                done,
                total,
                running: true,
            });
        })
        .await;
        run.finished.store(true, Ordering::Relaxed);
        if let Some(audit) = self.outlets.audit() {
            let totals = report.totals();
            let held = report.held_out();
            let stopped = match &report.stopped {
                Some(corpus::Stopped::Failed { case, error }) => {
                    format!("; stopped at {case}: {error}")
                }
                Some(corpus::Stopped::Bound { seconds }) => {
                    format!("; stopped at its bound of {seconds} s")
                }
                None => String::new(),
            };
            audit.event(
                "classifier_check",
                format!(
                    "{model}: {} of {} cases; {}; held out: {}; {} tokens in, {} out; {} ms{stopped}",
                    totals.cases,
                    report.planned,
                    totals.describe(),
                    held.describe(),
                    report.input_tokens,
                    report.output_tokens,
                    report.wall_ms
                ),
            );
        }
        Ok((model, report))
    }

    /// The classifier runs on `model`; `probe`: the start-up check should
    /// run against it. Set once.
    pub fn set_model(&self, model: String, probe: bool) {
        let mut slot = self.model.write();
        if slot.is_none() {
            *slot = Some((model, probe));
        }
    }

    /// The classifier's model, when one is configured.
    #[must_use]
    pub fn model(&self) -> Option<String> {
        self.model.read().as_ref().map(|(model, _)| model.clone())
    }

    /// What the start-up check found wrong, if it ran and found anything.
    #[must_use]
    pub fn probe_notice(&self) -> Option<String> {
        self.probe_notice.lock().clone()
    }

    /// The start-up check (when [`Self::set_model`] asked for it): two
    /// corpus cases ([`corpus::PROBE_CASES`]), a routine test run that
    /// must be allowed and a force push to the default branch that must be
    /// blocked, judged as `/auto check` judges them ([`corpus::evaluate`])
    /// outside any session (no counter, cache or audit line moves). A
    /// wrong verdict, or a check that cannot complete, becomes a notice
    /// for the user ([`ApprovalEvent::Notice`], and [`Self::probe_notice`]
    /// for clients that attach later; `/auto check` measures more).
    pub async fn probe(&self) {
        self.run_probe(false).await;
    }

    /// The start-up check against the model `/auto model` just chose
    /// ([`Self::switch`]): as [`Self::probe`], and a check that passes
    /// says so too.
    pub async fn probe_switched(&self) {
        self.run_probe(true).await;
    }

    async fn run_probe(&self, announce: bool) {
        let Some((model, true)) = self.model.read().clone() else {
            return;
        };
        let selection = corpus::Selection {
            cases: corpus::PROBE_CASES.map(str::to_string).to_vec(),
            ..corpus::Selection::default()
        };
        let notice = match corpus::select(&selection) {
            Err(err) => Some(format!(
                "the start-up check of the classifier model {model} could not run: {err}"
            )),
            Ok(cases) => {
                let checker = self.checker();
                let report =
                    corpus::evaluate(&*checker, &cases, corpus::Bounds::default(), |_, _| {}).await;
                match (&report.stopped, report.wrong().next()) {
                    (Some(corpus::Stopped::Failed { error, .. }), _) => Some(format!(
                        "the start-up check of the classifier model {model} could not complete ({error}); reviewed actions may ask you or fail until it answers"
                    )),
                    (Some(corpus::Stopped::Bound { .. }), _) => Some(format!(
                        "the start-up check of the classifier model {model} could not complete in time; reviewed actions may ask you or fail until it answers"
                    )),
                    (None, Some(wrong)) if wrong.label == corpus::Label::Allow => Some(format!(
                        "the classifier model {model} misjudged a routine case (it blocked {}): auto mode may block ordinary work; /auto check measures it on more cases, and `[auto] classifier_model` chooses another",
                        wrong.action
                    )),
                    (None, Some(wrong)) => Some(format!(
                        "the classifier model {model} misjudged a risky case (it allowed {}): auto mode may let harmful actions run; /auto check measures it on more cases, and `[auto] classifier_model` chooses another",
                        wrong.action
                    )),
                    (None, None) => {
                        if announce {
                            self.outlets.emit(ApprovalEvent::Notice(format!(
                                "the classifier model {model} passed the start-up check (a routine test run allowed, a force push to the default branch blocked); /auto check measures it on more cases"
                            )));
                        }
                        None
                    }
                }
            }
        };
        if let Some(text) = notice {
            tracing::warn!(%model, %text, "classifier start-up check");
            *self.probe_notice.lock() = Some(text.clone());
            self.outlets.emit(ApprovalEvent::Notice(text));
        }
    }
}
