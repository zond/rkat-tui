//! `!` shell mode: running the user's command, holding the ones typed
//! while the agent works, and their results.

use std::time::Instant;

use super::{
    App, Block, BlockId, Effect, NoticeLevel, PendingState, RunningShell, ShellOutcome, ShellRunId,
    ShellState, commands, transcript,
};

impl App {
    /// Whether the input is a `!` command (shell mode).
    pub fn shell_mode(&self) -> bool {
        !self.input.is_empty() && commands::parse_shell(&self.input.text()).is_some()
    }

    /// The editor title in shell mode.
    pub fn shell_mode_hint(&self) -> &'static str {
        if self.running || self.shell_busy() {
            " shell mode: Enter runs it after the current work · output joins the context "
        } else {
            " shell mode: Enter runs it as you, no sandbox · output joins the context "
        }
    }

    /// Whether a `!` command runs or waits to.
    pub(super) fn shell_busy(&self) -> bool {
        self.shell.running.is_some() || self.has_held_shell()
    }

    /// Whether a `!` command waits for the turn to end.
    pub(super) fn has_held_shell(&self) -> bool {
        self.pending
            .items()
            .iter()
            .any(|p| p.state == PendingState::Shell)
    }

    /// A `!` command typed (`raw` is the input as typed, for ↑): run it now
    /// when nothing else is, else hold it until the turn (and the commands
    /// before it) end.
    pub(super) fn shell_input(
        &mut self,
        command: String,
        raw: String,
        now: Instant,
        fx: &mut Vec<Effect>,
    ) {
        if command.trim().is_empty() {
            self.transcript.push_notice(
                NoticeLevel::Warn,
                "type a command after ! (\\! sends a literal !)",
            );
            self.scroll.bottom();
            return;
        }
        if self.running || self.shell_busy() {
            self.pending
                .add(&mut self.transcript, raw, PendingState::Shell);
        } else {
            let block = self.transcript.push(Block::Shell {
                command: command.clone(),
                state: ShellState::Running { since: now },
                output: None,
                collapsed: false,
            });
            self.launch_shell(command, block, now, fx);
        }
        self.scroll.bottom();
    }

    /// Send a `!` command whose block is `block` to the runner.
    pub(super) fn launch_shell(
        &mut self,
        command: String,
        block: BlockId,
        now: Instant,
        fx: &mut Vec<Effect>,
    ) {
        self.shell.last_id += 1;
        let id = ShellRunId(self.shell.last_id);
        self.shell.running = Some(RunningShell {
            id,
            block,
            command: command.clone(),
            since: now,
        });
        fx.push(Effect::RunShell { id, command });
    }

    /// Run the oldest held `!` command in place of its placeholder. Returns
    /// whether there was one.
    pub(super) fn run_next_shell(&mut self, now: Instant, fx: &mut Vec<Effect>) -> bool {
        let Some(id) = self
            .pending
            .items()
            .iter()
            .find(|p| p.state == PendingState::Shell)
            .map(|p| p.id)
        else {
            return false;
        };
        let Some(item) = self.pending.remove(id) else {
            return false;
        };
        let command = commands::parse_shell(&item.text)
            .unwrap_or_default()
            .to_string();
        self.transcript.replace_block(
            item.block,
            Block::Shell {
                command: command.clone(),
                state: ShellState::Running { since: now },
                output: None,
                collapsed: false,
            },
        );
        self.launch_shell(command, item.block, now, fx);
        true
    }

    /// [`Msg::ShellFinished`](crate::ui::Msg::ShellFinished): show the result, then run the next held
    /// command, or (with none left and no turn running) send the messages
    /// held behind them.
    pub(super) fn shell_finished(
        &mut self,
        id: ShellRunId,
        outcome: ShellOutcome,
        now: Instant,
        fx: &mut Vec<Effect>,
    ) {
        let Some(run) = self.shell.running.take_if(|run| run.id == id) else {
            return;
        };
        let (state, output) = match outcome {
            ShellOutcome::Ran { run: done, context } => {
                if let Err(err) = context {
                    self.transcript.push_notice(
                        NoticeLevel::Warn,
                        format!("the command's output could not join the context: {err}"),
                    );
                }
                (
                    ShellState::Done {
                        exit: done.exit,
                        duration_ms: u64::try_from(done.duration.as_millis()).unwrap_or(u64::MAX),
                    },
                    transcript::shell_output_preview(&done.output),
                )
            }
            ShellOutcome::Failed(reason) => (ShellState::Failed(reason), None),
            ShellOutcome::Cancelled => (ShellState::Cancelled, None),
        };
        self.transcript.replace_block(
            run.block,
            Block::Shell {
                command: run.command,
                state,
                output,
                collapsed: false,
            },
        );
        self.scroll.bottom();
        if self.running || self.run_next_shell(now, fx) {
            return;
        }
        let held = self
            .pending
            .take_where(|p| p.state == PendingState::Deferred);
        if !held.is_empty() || self.send_next.is_some() {
            self.next_after_turn(held, false, true, now, fx);
        }
    }
}

/// The `!` commands.
#[derive(Debug, Default)]
pub(super) struct ShellRuns {
    /// The command running now, if any.
    pub(super) running: Option<RunningShell>,
    /// The last allocated [`ShellRunId`].
    pub(super) last_id: u64,
}
