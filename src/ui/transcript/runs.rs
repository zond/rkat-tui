//! Runs: consecutive blocks of one kind drawn as one folded line, a view
//! over the blocks that changes none of them.
//!
//! A run is two or more blocks with the same [`RunKey`] with nothing drawn
//! between them: a hidden block (inside a folded helper) or one that draws
//! nothing (a model's whitespace-only text) does not part it, any other
//! block does. What joins one:
//!
//! - a message one of the agent's own helpers sent (a sender the
//!   transcript knows as a helper of its tree: a header, a call that
//!   started it, the daemon's seated list), folded to its line, of the
//!   same scope (the agent's, or the helper that received it). A message
//!   from another agent of the workspace or from an agent wired with
//!   `/wire` stays its own line and parts a run;
//! - a finished, successful call of one tool of one scope, folded to its
//!   line (`✓ read 5 files: a.rs, b.rs, c.rs and 2 more ▸`): a call that
//!   failed, was denied, waits for approval or still runs is its own line
//!   and parts a run, and so does one the user opened. Shell calls join
//!   only with the same command line (each other command is what the
//!   user wants to see).
//!
//! The run draws as one line in the style of its blocks' own line,
//! folded (`▸`); a click on it opens it: the line, `▾`, then each block's
//! own lines as they draw alone, each still opened and folded by its own
//! click. A click on the opened run's line, or Ctrl-O off, folds it
//! again. Ctrl-O on shows every block in full, so nothing is in a run.
//!
//! The roles are computed when the transcript or a helper's view is laid
//! out, over the blocks it draws, and kept with each block's layout: a
//! block whose role changed is laid out again.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::{Block, BlockId, ToolState, Transcript, tool_line};
use crate::ui::markdown::{display_width, sanitize, truncate_to_width};
use crate::ui::render::block_lines;

/// Senders a run's line names before `and N more`.
const MAX_RUN_NAMES: usize = 3;

/// The narrowest room the latest message's first words take on a run's
/// line; with less the line names the senders only.
const MIN_LATEST: usize = 8;

/// What a block draws as part of a run (or alone).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum RunRole {
    /// Not in a run: the block's own lines.
    #[default]
    Alone,
    /// The first block of a run: the run's line, and while `open` its
    /// own lines after it.
    Head {
        line: RunLine,
        /// Whether the run is opened.
        open: bool,
        /// Every block of the run, in order (this one first).
        members: Vec<BlockId>,
    },
    /// A later block of a run: nothing while the run is folded, its own
    /// lines while it is opened.
    Member {
        /// Whether the run is opened.
        open: bool,
    },
}

/// What a run's line says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunLine {
    /// Messages from the agent's helpers.
    Messages {
        /// The indent depth (a helper's received messages: its depth).
        depth: usize,
        /// The receiving helper's tag, when a helper received them.
        tag: Option<String>,
        count: usize,
        /// The senders, each once, in the order they first sent.
        senders: Vec<String>,
        /// The latest message, on one line.
        latest: String,
    },
    /// Finished calls of one tool.
    Tools {
        /// The indent depth (a helper's calls: its depth).
        depth: usize,
        /// What the calls did (`read 5 files: a.rs, b.rs, c.rs and 2
        /// more`, `run cargo test ×3`).
        text: String,
        /// Their diffs' summed numbers (`+31 −7`), when every call has one.
        numbers: Option<String>,
    },
}

/// Which blocks join one run: blocks of equal keys next to each other.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RunKey {
    /// Messages from the agent's helpers, of one scope.
    Messages {
        owner: Option<BlockId>,
        to: Option<String>,
    },
    /// Finished calls of one tool by one caller (a helper's by its tag),
    /// of one scope; a shell call's with its command line.
    Tools {
        owner: Option<BlockId>,
        tool: String,
        tag: Option<String>,
        command: Option<String>,
    },
}

/// The tools whose calls act on one file each: a run of them counts the
/// files.
const FILE_TOOLS: &[&str] = &["read_file", "edit_file", "write_file"];

/// The shell tool: its calls join a run only with the same command.
const SHELL_TOOL: &str = "shell";

impl Transcript {
    /// The key of the block at `index`, when it may join a run.
    fn run_key(&self, index: usize) -> Option<RunKey> {
        let entry = self.entries.get(index)?;
        let open = self.open_runs.contains(&entry.id);
        match &entry.block {
            Block::Peer {
                from,
                text,
                collapsed,
                to,
            } if (*collapsed || open) && !text.trim().is_empty() && self.helpers.knows(from) => {
                Some(RunKey::Messages {
                    owner: entry.owner,
                    to: to.as_ref().map(|to| to.tag.clone()),
                })
            }
            Block::Tool {
                tool,
                tag,
                args,
                outcome,
                state: ToolState::Done {
                    is_error: false, ..
                },
                collapsed,
                ..
            } if (*collapsed || open) && !outcome.failed => Some(RunKey::Tools {
                owner: entry.owner,
                tool: tool.clone(),
                tag: tag.clone(),
                command: (tool == SHELL_TOOL).then(|| {
                    args.get("command")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string()
                }),
            }),
            _ => None,
        }
    }

    /// Whether the block at `index` draws nothing (it parts no run).
    fn draws_nothing(&self, index: usize) -> bool {
        matches!(
            self.entries.get(index).map(|entry| &entry.block),
            Some(Block::Assistant { md }) if md.trim().is_empty()
        )
    }

    /// The role of each block of `seq` (positions in `entries`, in the
    /// order a view draws them, every one drawn).
    pub(super) fn run_roles(&self, seq: &[usize]) -> Vec<RunRole> {
        let mut roles = vec![RunRole::Alone; seq.len()];
        let mut at = 0;
        while at < seq.len() {
            let Some(key) = self.run_key(seq[at]) else {
                at += 1;
                continue;
            };
            let mut members = vec![at];
            let mut next = at + 1;
            while next < seq.len() {
                if self.draws_nothing(seq[next]) {
                    next += 1;
                    continue;
                }
                if self.run_key(seq[next]).as_ref() != Some(&key) {
                    break;
                }
                members.push(next);
                next += 1;
            }
            if members.len() >= 2 {
                let indices: Vec<usize> = members.iter().map(|m| seq[*m]).collect();
                let ids: Vec<BlockId> = indices.iter().map(|i| self.entries[*i].id).collect();
                let open = ids.iter().any(|id| self.open_runs.contains(id));
                let line = self.run_line(&indices);
                for (n, m) in members.iter().enumerate() {
                    roles[*m] = if n == 0 {
                        RunRole::Head {
                            line: line.clone(),
                            open,
                            members: ids.clone(),
                        }
                    } else {
                        RunRole::Member { open }
                    };
                }
            }
            at = next;
        }
        roles
    }

    /// The line of the run of the blocks at `indices`.
    fn run_line(&self, indices: &[usize]) -> RunLine {
        let blocks: Vec<&Block> = indices
            .iter()
            .filter_map(|index| self.entries.get(*index).map(|entry| &entry.block))
            .collect();
        if let Some(Block::Tool { .. }) = blocks.first() {
            return tools_line(&blocks);
        }
        let mut senders: Vec<String> = Vec::new();
        let mut latest = String::new();
        let mut depth = 0;
        let mut tag = None;
        for index in indices {
            if let Some(Block::Peer { from, text, to, .. }) =
                self.entries.get(*index).map(|entry| &entry.block)
            {
                if !senders.contains(from) {
                    senders.push(from.clone());
                }
                latest = text.split_whitespace().collect::<Vec<_>>().join(" ");
                depth = to.as_ref().map_or(0, |to| to.depth);
                tag = to.as_ref().map(|to| to.tag.clone());
            }
        }
        RunLine::Messages {
            depth,
            tag,
            count: indices.len(),
            senders,
            latest,
        }
    }

    /// The roles of the blocks from `start` on as the transcript draws
    /// them (a hidden block is alone: it draws nothing).
    pub(super) fn transcript_roles(&self, start: usize) -> Vec<RunRole> {
        let shown: Vec<usize> = (start..self.entries.len())
            .filter(|i| !self.is_hidden(self.entries[*i].id))
            .collect();
        let mut roles = vec![RunRole::Alone; self.entries.len().saturating_sub(start)];
        for (index, role) in shown.iter().zip(self.run_roles(&shown)) {
            roles[index - start] = role;
        }
        roles
    }

    /// Where laying out from block `from` must begin for the runs' sake:
    /// the first block of the run the last drawn block before `from` is
    /// in (its line counts the blocks after it), else `from`.
    pub(super) fn run_start(&self, from: usize) -> usize {
        let drawn = |i: &usize| !self.is_hidden(self.entries[*i].id) && !self.draws_nothing(*i);
        let Some(last) = (0..from.min(self.entries.len())).rev().find(drawn) else {
            return from;
        };
        let Some(key) = self.run_key(last) else {
            return from;
        };
        let mut start = last;
        for i in (0..last).rev().filter(drawn) {
            if self.run_key(i).as_ref() != Some(&key) {
                break;
            }
            start = i;
        }
        start
    }

    /// A click on line `line` of block `id` as the transcript (`view`
    /// `false`) or a helper's view last drew it: a folded run's line opens
    /// the run, an opened run's line folds it. Returns whether the run is
    /// open now, or `None` when the click is on no run's line.
    pub fn toggle_run(&mut self, id: BlockId, line: usize, view: bool) -> Option<bool> {
        let entry = self.entry(id)?;
        let role = if view { &entry.view_role } else { &entry.role };
        let RunRole::Head { open, members, .. } = role else {
            return None;
        };
        if *open && line > 0 {
            return None;
        }
        let open = !*open;
        let members = members.clone();
        for member in &members {
            if open {
                self.open_runs.insert(*member);
            } else {
                self.open_runs.remove(member);
            }
        }
        if let Some(index) = members.first().and_then(|first| self.index_of(*first)) {
            self.layout.touch(index);
        }
        Some(open)
    }
}

/// The lines of `block` in `role` at `width`.
pub(super) fn role_lines(block: &Block, role: &RunRole, width: u16) -> Vec<Line<'static>> {
    match role {
        RunRole::Alone | RunRole::Member { open: true } => block_lines(block, width),
        RunRole::Member { open: false } => Vec::new(),
        RunRole::Head { line, open, .. } => {
            let mut out = vec![run_line(line, *open, usize::from(width).max(4))];
            if *open {
                out.extend(block_lines(block, width));
            } else {
                out.push(Line::default());
            }
            out
        }
    }
}

/// The line of a run of finished calls of one tool, worded from their
/// own lines' heads: one head made by all of them is that head `×N`
/// (`run cargo test ×3`, `edit PLAN.md ×3`); calls of a file tool count
/// the files (`read 5 files: a.rs, b.rs, c.rs and 2 more`, `×N` when
/// some file was read twice); any other tool's are its verb `×N` and
/// the objects. The diffs' numbers are summed when every call has one.
fn tools_line(blocks: &[&Block]) -> RunLine {
    let mut heads: Vec<&tool_line::Head> = Vec::new();
    let mut name = "";
    let mut depth = 0;
    let (mut added, mut removed, mut diffs) = (0, 0, 0);
    for block in blocks {
        if let Block::Tool {
            tool,
            head,
            diff,
            depth: at,
            ..
        } = block
        {
            heads.push(head);
            name = tool;
            depth = *at;
            if let Some(diff) = diff {
                added += diff.added;
                removed += diff.removed;
                diffs += 1;
            }
        }
    }
    let count = heads.len();
    let verb = heads.first().map_or("", |head| head.verb.as_str());
    let mut objects: Vec<String> = Vec::new();
    for head in &heads {
        if !head.object.is_empty() && !objects.contains(&head.object) {
            objects.push(head.object.clone());
        }
    }
    let same = heads.windows(2).all(|pair| pair[0] == pair[1]);
    let text = if same {
        let head = heads.first().map(|head| head.text()).unwrap_or_default();
        format!("{head} ×{count}")
    } else if FILE_TOOLS.contains(&name) {
        let files = crate::text::count(objects.len(), "file");
        let times = if objects.len() < count {
            format!(" ×{count}")
        } else {
            String::new()
        };
        format!("{verb} {files}{times}: {}", names(&objects))
    } else if objects.is_empty() {
        format!("{verb} ×{count}")
    } else {
        format!("{verb} ×{count}: {}", names(&objects))
    };
    RunLine::Tools {
        depth,
        text,
        numbers: (diffs == count).then(|| format!("+{added} −{removed}")),
    }
}

/// `a, b, c and 2 more`.
fn names(all: &[String]) -> String {
    let shown: Vec<&str> = all.iter().take(MAX_RUN_NAMES).map(String::as_str).collect();
    let mut out = shown.join(", ");
    if all.len() > MAX_RUN_NAMES {
        out.push_str(&format!(" and {} more", all.len() - MAX_RUN_NAMES));
    }
    out
}

/// A run's one line at `w` columns: folded (`▸`, with the latest
/// message's first words when they fit) or opened (`▾`).
fn run_line(line: &RunLine, open: bool, w: usize) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    match line {
        RunLine::Messages {
            depth,
            tag,
            count,
            senders,
            latest,
        } => {
            let style = Style::default().fg(Color::Magenta);
            let bold = style.add_modifier(Modifier::BOLD);
            let indent = "  ".repeat(*depth);
            let lead = match tag {
                Some(tag) => format!("{indent}✉ {} ", sanitize(tag)),
                None => format!("{indent}✉ "),
            };
            let head = format!(
                "{lead}{} from {}",
                crate::text::count(*count, "message"),
                sanitize(&names(senders))
            );
            let head = truncate_to_width(&head, w.saturating_sub(2));
            if open {
                return Line::from(vec![Span::styled(head, bold), Span::styled(" ▾", dim)]);
            }
            let room = w.saturating_sub(display_width(&head) + 4);
            let mut spans = vec![Span::styled(head, bold)];
            if room >= MIN_LATEST && !latest.is_empty() {
                spans.push(Span::styled(
                    format!(": {}", truncate_to_width(&sanitize(latest), room)),
                    style,
                ));
            }
            spans.push(Span::styled(" ▸", dim));
            Line::from(spans)
        }
        RunLine::Tools {
            depth,
            text,
            numbers,
        } => {
            // As a finished call's own line: the marker and the head in
            // its colour, the numbers plain, the fold marker dim.
            let lead = format!("{}{} ", "  ".repeat(*depth), tool_line::Mark::Done.glyph());
            let numbers = numbers.as_ref().map(|n| format!("  {n}"));
            let fixed = display_width(&lead) + numbers.as_ref().map_or(0, |n| display_width(n)) + 2;
            let text = truncate_to_width(&sanitize(text), w.saturating_sub(fixed).max(1));
            let mut spans = vec![Span::styled(
                format!("{lead}{text}"),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )];
            if let Some(numbers) = numbers {
                spans.push(Span::raw(numbers));
            }
            spans.push(Span::styled(if open { " ▾" } else { " ▸" }, dim));
            Line::from(spans)
        }
    }
}
