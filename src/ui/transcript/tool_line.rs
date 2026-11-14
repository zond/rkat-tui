//! A tool call as one plain line: what the transcript, a restored
//! conversation, a helper's view, the approval dialog and `/export` show
//! for it, never its arguments as JSON (those are in its expansion).
//!
//! The line has five parts, always in this order: the state marker
//! (`○ ◐ ✓ ✗ ⊘`), a plain verb, the object, the outcome in numbers and the
//! time (`✓ read src/main.rs  lines 1–40 · 1.2s`). Each tool's verb, object
//! and outcome come from one row of [`RULES`]; a tool without a row shows
//! `<tool>: <its first string argument>`. A success says nothing of being
//! one; a failure adds its reason. An outcome is only what the call's own
//! data says for certain (the result of one of this application's tools,
//! the engine's shell status line, a structured result): a number that is
//! not there is left out, never guessed.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::ui::markdown::{display_width, truncate_to_width};

/// How a line's object is shortened when the line does not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// In the middle (a path: its start and its file name stay).
    Middle,
    /// At its end (a command, a text).
    End,
}

/// A call's verb and object (`run`, `cargo build`), and what follows the
/// object uncut (`(+2 lines)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// The plain verb (`read`, `run`, `search`).
    pub verb: String,
    /// What it acts on; shortened first when the line is too long.
    pub object: String,
    /// How the object is shortened.
    pub cut: Cut,
    /// Kept after the object (`(+2 lines)`).
    pub more: Option<String>,
}

impl Head {
    fn new(verb: impl Into<String>, object: impl Into<String>, cut: Cut) -> Self {
        Self {
            verb: verb.into(),
            object: object.into(),
            cut,
            more: None,
        }
    }

    /// The head as one unshortened text (`run cargo build (+2 lines)`).
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = self.verb.clone();
        if !self.object.is_empty() {
            out.push(' ');
            out.push_str(&self.object);
        }
        if let Some(more) = &self.more {
            out.push(' ');
            out.push_str(more);
        }
        out
    }

    /// The head in at most `width` columns: the object shortened (`…`),
    /// the verb and what follows the object kept.
    #[must_use]
    pub fn fit(&self, width: usize) -> String {
        let full = self.text();
        if display_width(&full) <= width {
            return full;
        }
        let more = self.more.as_ref().map_or(0, |m| display_width(m) + 1);
        let fixed = display_width(&self.verb) + 1 + more;
        let room = width.saturating_sub(fixed).max(1);
        let object = match self.cut {
            Cut::End => truncate_to_width(&self.object, room),
            Cut::Middle => cut_middle(&self.object, room),
        };
        let shortened = Self {
            object,
            ..self.clone()
        };
        shortened.text()
    }
}

/// `text` in at most `width` columns, cut in the middle with `…` (more
/// of its end kept than of its start: a path's file name says most).
fn cut_middle(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let room = width - 1;
    let head_w = room / 3;
    let tail_w = room - head_w;
    let mut head = String::new();
    for c in text.chars() {
        if display_width(&head) + display_width(c.encode_utf8(&mut [0; 4])) > head_w {
            break;
        }
        head.push(c);
    }
    let mut tail: Vec<char> = Vec::new();
    for c in text.chars().rev() {
        let w: usize = tail
            .iter()
            .map(|c| display_width(c.encode_utf8(&mut [0; 4])))
            .sum();
        if w + display_width(c.encode_utf8(&mut [0; 4])) > tail_w {
            break;
        }
        tail.push(c);
    }
    tail.reverse();
    format!("{head}…{}", tail.into_iter().collect::<String>())
}

/// Where paths are shown from: relative to the project root, `~` for the
/// home directory, else absolute.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathView {
    /// The project root.
    pub root: Option<PathBuf>,
    /// The user's home directory.
    pub home: Option<PathBuf>,
}

impl PathView {
    /// `raw` as a line shows it.
    #[must_use]
    pub fn show(&self, raw: &str) -> String {
        let path = Path::new(raw);
        if !path.is_absolute() {
            return raw.to_string();
        }
        if let Some(rest) = self
            .root
            .as_deref()
            .and_then(|root| path.strip_prefix(root).ok())
        {
            let rest = rest.display().to_string();
            return if rest.is_empty() { ".".into() } else { rest };
        }
        if let Some(rest) = self
            .home
            .as_deref()
            .and_then(|home| path.strip_prefix(home).ok())
        {
            return format!("~/{}", rest.display());
        }
        raw.to_string()
    }
}

/// What a finished call came to: the outcome text, and whether it failed
/// (a tool's error, a command's nonzero exit or time-out).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The numbers (`lines 1–40`, `exit 2`); `None`: nothing known.
    pub text: Option<String>,
    /// Whether it failed although the tool did not report an error.
    pub failed: bool,
}

impl Outcome {
    fn said(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            failed: false,
        }
    }

    fn failure(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            failed: true,
        }
    }
}

/// One tool's line: its verb and object from its arguments, its outcome
/// from its arguments and its whole result text.
struct Rule {
    tools: &'static [&'static str],
    head: fn(&str, &Value, &PathView) -> Head,
    outcome: fn(&str, &Value, &str) -> Outcome,
}

/// The rows: a new tool gets its line by adding one.
const RULES: &[Rule] = &[
    Rule {
        tools: &["edit_file"],
        head: |_, args, paths| path_head("edit", args, paths),
        outcome: no_outcome,
    },
    Rule {
        tools: &["write_file"],
        head: |_, args, paths| path_head("write", args, paths),
        outcome: |_, _, result| write_outcome(result),
    },
    Rule {
        tools: &["read_file"],
        head: |_, args, paths| path_head("read", args, paths),
        outcome: |_, _, result| read_outcome(result),
    },
    Rule {
        tools: &["shell"],
        head: |_, args, _| shell_head(args),
        outcome: |_, _, result| shell_outcome(result),
    },
    Rule {
        tools: &["grep"],
        head: |_, args, paths| {
            let mut object = format!("\"{}\"", str_arg(args, "pattern"));
            let scope = args
                .get("glob")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    args.get("path")
                        .and_then(Value::as_str)
                        .map(|p| paths.show(p))
                });
            if let Some(scope) = scope {
                object.push_str(" in ");
                object.push_str(&scope);
            }
            Head::new("search", object, Cut::End)
        },
        outcome: |_, args, result| grep_outcome(args, result),
    },
    Rule {
        tools: &["glob"],
        head: |_, args, paths| {
            let mut object = str_arg(args, "pattern").to_string();
            if let Some(path) = args.get("path").and_then(Value::as_str) {
                object.push_str(" in ");
                object.push_str(&paths.show(path));
            }
            Head::new("find", object, Cut::End)
        },
        outcome: |_, _, result| glob_outcome(result),
    },
    Rule {
        tools: &["delegate"],
        head: |_, args, _| member_task_head("delegate to", args),
        outcome: no_outcome,
    },
    Rule {
        tools: &["spawn_member", "mob_spawn_member"],
        head: |_, args, _| member_task_head("spawn", args),
        outcome: no_outcome,
    },
    Rule {
        tools: &["spawn_many_members"],
        head: |_, args, _| {
            let names: Vec<&str> = args
                .as_object()
                .into_iter()
                .flat_map(|map| map.values())
                .filter_map(Value::as_array)
                .flatten()
                .filter_map(|spec| spec.get("member_id").and_then(Value::as_str))
                .collect();
            Head::new("spawn", names.join(", "), Cut::End)
        },
        outcome: no_outcome,
    },
    Rule {
        tools: &["fork_off"],
        head: |_, args, _| member_task_head("fork", args),
        outcome: no_outcome,
    },
    Rule {
        tools: &["send_message"],
        head: |_, args, _| {
            let to = args
                .get("display_name")
                .and_then(Value::as_str)
                .or_else(|| args.get("peer_id").and_then(Value::as_str))
                .unwrap_or("?");
            let body = one_line(str_arg(args, "body"));
            Head::new("message →", format!("{to}: {body}"), Cut::End)
        },
        outcome: no_outcome,
    },
    Rule {
        tools: &["task_create"],
        head: |_, args, _| Head::new("task +", str_arg(args, "subject"), Cut::End),
        outcome: no_outcome,
    },
    Rule {
        tools: &["task_update"],
        head: |_, args, _| {
            let done = args.get("status").and_then(Value::as_str) == Some("completed");
            let what = args
                .get("subject")
                .and_then(Value::as_str)
                .or_else(|| args.get("id").and_then(Value::as_str))
                .unwrap_or_default();
            Head::new(if done { "task ✓" } else { "task" }, what, Cut::End)
        },
        outcome: no_outcome,
    },
    Rule {
        tools: &["task_get"],
        head: |_, args, _| Head::new("task", str_arg(args, "id"), Cut::End),
        outcome: no_outcome,
    },
    Rule {
        tools: &["task_list"],
        head: |_, _, _| Head::new("tasks", "", Cut::End),
        outcome: |_, _, result| {
            json_count(result).map_or_else(Outcome::default, |n| Outcome::said(n.to_string()))
        },
    },
    Rule {
        tools: &["note_save", "note_read", "note_forget"],
        head: |_, args, _| Head::new("note", str_arg(args, "name"), Cut::End),
        outcome: |tool, _, _| {
            Outcome::said(match tool {
                "note_save" => "saved",
                "note_read" => "read",
                _ => "removed",
            })
        },
    },
    Rule {
        tools: &["note_list"],
        head: |_, _, _| Head::new("notes", "", Cut::End),
        outcome: no_outcome,
    },
    Rule {
        tools: &["skill_read"],
        head: |_, args, _| match args.get("name").and_then(Value::as_str) {
            Some(name) => Head::new("skill", name, Cut::End),
            None => Head::new("skills", "", Cut::End),
        },
        outcome: no_outcome,
    },
    Rule {
        tools: &["tool_search"],
        head: |_, args, _| {
            let query = args
                .get("query")
                .or_else(|| args.get("family"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            Head::new("tools", format!("\"{query}\""), Cut::End)
        },
        outcome: no_outcome,
    },
    Rule {
        tools: &["watch_paths"],
        head: |_, args, _| {
            let patterns: Vec<&str> = args
                .get("patterns")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            Head::new("watch", patterns.join(" "), Cut::End)
        },
        outcome: no_outcome,
    },
    Rule {
        tools: &["unwatch"],
        head: |_, args, _| Head::new("unwatch", first_string(args).unwrap_or_default(), Cut::End),
        outcome: no_outcome,
    },
    Rule {
        tools: &[
            "meerkat_schedule_create",
            "meerkat_schedule_update",
            "meerkat_schedule_get",
            "meerkat_schedule_delete",
            "meerkat_schedule_pause",
            "meerkat_schedule_resume",
            "meerkat_schedule_occurrences",
        ],
        head: |tool, args, _| {
            let what = args
                .get("name")
                .or_else(|| args.get("schedule_id"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let verb = match tool.trim_start_matches("meerkat_schedule_") {
                "create" => "schedule",
                "delete" => "unschedule",
                other => return Head::new(format!("schedule {other}"), what, Cut::End),
            };
            Head::new(verb, what, Cut::End)
        },
        outcome: no_outcome,
    },
    Rule {
        tools: &["meerkat_schedule_list"],
        head: |_, _, _| Head::new("schedules", "", Cut::End),
        outcome: no_outcome,
    },
    Rule {
        tools: &["member_status", "mob_check_member"],
        head: |_, args, _| Head::new("status", member(args), Cut::End),
        outcome: no_outcome,
    },
    Rule {
        tools: &["mob_list_members", "mob_list", "peers"],
        head: |tool, args, _| {
            let verb = match tool {
                "peers" => "peers",
                "mob_list" => "mobs",
                _ => "members",
            };
            Head::new(verb, str_arg(args, "mob_id"), Cut::End)
        },
        outcome: no_outcome,
    },
    Rule {
        tools: &["retire_member", "mob_retire_member"],
        head: |_, args, _| Head::new("retire", member(args), Cut::End),
        outcome: no_outcome,
    },
];

fn no_outcome(_: &str, _: &Value, _: &str) -> Outcome {
    Outcome::default()
}

fn str_arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn member(args: &Value) -> String {
    args.get("member_id")
        .or_else(|| args.get("member"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// `text` on one line: its whitespace runs as single spaces.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first string argument's value (a JSON object's own order).
fn first_string(args: &Value) -> Option<String> {
    args.as_object()?
        .values()
        .find_map(Value::as_str)
        .map(one_line)
}

fn path_head(verb: &str, args: &Value, paths: &PathView) -> Head {
    let raw = args
        .get("path")
        .or_else(|| args.get("file_path"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    Head::new(verb, paths.show(raw), Cut::Middle)
}

fn member_task_head(verb: &str, args: &Value) -> Head {
    let name = member(args);
    let task = args
        .get("task")
        .or_else(|| args.get("initial_message"))
        .and_then(Value::as_str)
        .map(one_line)
        .unwrap_or_default();
    let object = if task.is_empty() {
        name
    } else {
        format!("{name}: {task}")
    };
    Head::new(verb, object, Cut::End)
}

fn shell_head(args: &Value) -> Head {
    let command = str_arg(args, "command").trim();
    let mut lines = command.lines();
    let first = lines.next().unwrap_or_default().trim().to_string();
    let rest = lines.count();
    let mut head = Head::new("run", first, Cut::End);
    if rest > 0 {
        head.more = Some(format!(
            "(+{} {})",
            rest,
            if rest == 1 { "line" } else { "lines" }
        ));
    }
    head
}

/// `write_file`'s answer for a new file, `created <path> (N lines)`
/// (`src/tools/write.rs`): `new, N lines`. A replaced file's numbers are
/// its diff's.
fn write_outcome(result: &str) -> Outcome {
    let first = result.lines().next().unwrap_or_default();
    if !first.starts_with("created ") {
        return Outcome::default();
    }
    let lines = first
        .rsplit_once(" (")
        .and_then(|(_, rest)| rest.strip_suffix(" lines)"))
        .and_then(|n| n.parse::<usize>().ok());
    match lines {
        Some(n) => Outcome::said(format!("new, {}", crate::text::count(n, "line"))),
        None => Outcome::said("new"),
    }
}

/// `read_file`'s numbered lines (`{number:>6}\t{line}`, `src/tools/read.rs`):
/// `N lines` from the start, else `lines A–B`, `(truncated)` when the file
/// was cut.
fn read_outcome(result: &str) -> Outcome {
    if result.starts_with("(empty file)") {
        return Outcome::said("empty");
    }
    let numbers: Vec<usize> = result
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter_map(|(n, _)| n.trim_start().parse().ok())
        .collect();
    let (Some(first), Some(last)) = (numbers.first(), numbers.last()) else {
        return Outcome::default();
    };
    let more = result.contains("\n… [showing lines ");
    let mut text = if *first == 1 && !more {
        crate::text::count(numbers.len(), "line")
    } else {
        format!("lines {first}–{last}")
    };
    if result.contains("\n… [file truncated") {
        text.push_str(" (truncated)");
    }
    Outcome::said(text)
}

/// The engine's shell status line (meerkat-tools `ShellOutput::render_for_model`,
/// its first line): `exit code N (Xs)`, `timed out after Xs; …`, or
/// `terminated by a signal, …`. Exit 0 says nothing.
fn shell_outcome(result: &str) -> Outcome {
    let first = result.lines().next().unwrap_or_default();
    if let Some(rest) = first.strip_prefix("exit code ") {
        let code = rest.split_whitespace().next().unwrap_or_default();
        return match code.parse::<i32>() {
            Ok(0) | Err(_) => Outcome::default(),
            Ok(code) => Outcome::failure(format!("exit {code}")),
        };
    }
    if let Some(rest) = first.strip_prefix("timed out after ") {
        let secs = rest
            .split(['s', ';'])
            .next()
            .and_then(|s| s.parse::<f64>().ok());
        return Outcome::failure(match secs {
            Some(secs) => format!("timed out after {}", human_secs(secs)),
            None => "timed out".to_string(),
        });
    }
    if first.starts_with("terminated by a signal") {
        return Outcome::failure("killed by a signal");
    }
    Outcome::default()
}

/// The lines of a search's answer before its notes (`… [`, `[partial:`).
fn answer_lines(result: &str) -> impl Iterator<Item = &str> {
    result
        .lines()
        .take_while(|line| !line.starts_with("… [") && !line.starts_with("[partial:"))
        .filter(|line| !line.is_empty())
}

/// Whether a search stopped early (`src/tools/scan.rs` `partial_note`).
fn stopped(result: &str) -> bool {
    result.contains("[partial: the search stopped") || result.contains("The search stopped before")
}

fn with_stop(text: String, result: &str) -> Outcome {
    if stopped(result) {
        Outcome::said(format!("{text} (stopped early)"))
    } else {
        Outcome::said(text)
    }
}

/// `grep`'s answer (`src/tools/grep.rs`): file paths (the default), per
/// file counts (`count`), or matching lines (`content`; counted only
/// without context lines, which look alike).
fn grep_outcome(args: &Value, result: &str) -> Outcome {
    if result.starts_with("No matches for") {
        return with_stop("no matches".to_string(), result);
    }
    let lines: Vec<&str> = answer_lines(result).collect();
    let mode = args
        .get("output_mode")
        .and_then(Value::as_str)
        .unwrap_or("files_with_matches");
    let text = match mode {
        "files_with_matches" => crate::text::count(lines.len(), "file"),
        "count" => {
            let counts: Option<Vec<usize>> = lines
                .iter()
                .map(|line| line.rsplit_once(':').and_then(|(_, n)| n.parse().ok()))
                .collect();
            let Some(counts) = counts else {
                return Outcome::default();
            };
            format!(
                "{} in {}",
                matches(counts.iter().sum::<usize>()),
                crate::text::count(counts.len(), "file")
            )
        }
        "content" if args.get("context").and_then(Value::as_u64).unwrap_or(0) == 0 => {
            let files: std::collections::HashSet<&str> = lines
                .iter()
                .filter_map(|line| line.split_once(':').map(|(path, _)| path))
                .collect();
            format!(
                "{} in {}",
                matches(lines.len()),
                crate::text::count(files.len(), "file")
            )
        }
        _ => return Outcome::default(),
    };
    with_stop(text, result)
}

/// `1 match`, `3 matches`.
fn matches(n: usize) -> String {
    if n == 1 {
        "1 match".to_string()
    } else {
        format!("{n} matches")
    }
}

/// `glob`'s answer (`src/tools/glob.rs`): one path a line, and the total
/// when it was cut (`showing N of T matches`).
fn glob_outcome(result: &str) -> Outcome {
    if result.starts_with("No files matched") {
        return with_stop("no files".to_string(), result);
    }
    let shown = answer_lines(result).count();
    let total = result
        .split("of ")
        .nth(1)
        .filter(|_| result.contains("[results truncated: showing "))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(shown);
    with_stop(crate::text::count(total, "file"), result)
}

/// The number of entries of a structured result: a JSON array, or an
/// object's one array field.
fn json_count(result: &str) -> Option<usize> {
    let value: Value = serde_json::from_str(result.trim()).ok()?;
    match value {
        Value::Array(items) => Some(items.len()),
        Value::Object(map) => {
            let mut arrays = map.values().filter_map(Value::as_array);
            let first = arrays.next()?;
            arrays.next().is_none().then_some(first.len())
        }
        _ => None,
    }
}

/// The tool's own name, without a helper's tag (`[scout · m] glob`).
fn base(name: &str) -> &str {
    name.rsplit(' ').next().unwrap_or(name)
}

/// A call's head, from its row, else `<tool>: <first string argument>`.
/// A helper's call keeps its tag in front (`[scout] read a.rs`).
#[must_use]
pub fn head(name: &str, args: &Value, paths: &PathView) -> Head {
    let tool = base(name);
    let tag = name
        .strip_suffix(tool)
        .map(str::trim_end)
        .unwrap_or_default();
    let mut head = RULES
        .iter()
        .find(|rule| rule.tools.contains(&tool))
        .map_or_else(
            || {
                Head::new(
                    format!("{tool}:"),
                    first_string(args).unwrap_or_default(),
                    Cut::End,
                )
            },
            |rule| (rule.head)(tool, args, paths),
        );
    if head.verb.ends_with(':') && head.object.is_empty() {
        head.verb.pop();
    }
    if !tag.is_empty() {
        head.verb = format!("{tag} {}", head.verb);
    }
    head
}

/// A finished call's outcome from its whole result text.
#[must_use]
pub fn outcome(name: &str, args: &Value, result: &str) -> Outcome {
    let tool = base(name);
    RULES
        .iter()
        .find(|rule| rule.tools.contains(&tool))
        .map_or_else(Outcome::default, |rule| (rule.outcome)(tool, args, result))
}

/// A denial in a few words, from the gate's message
/// (`crate::approval::DenyReason`): `blocked by the classifier (R1)`,
/// `denied by you`, `denied by rule <id>`, `plan mode`; else the
/// message's first words.
#[must_use]
pub fn denial(reason: &str) -> String {
    use crate::approval::{CLASSIFIER_SOURCE, REVIEW_INCOMPLETE};
    if reason.contains(CLASSIFIER_SOURCE) {
        let rule = reason
            .split('[')
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .filter(|rule| !rule.is_empty() && rule.len() <= 16 && !rule.contains(' '));
        return match rule {
            Some(rule) => format!("blocked by the classifier ({rule})"),
            None => "blocked by the classifier".to_string(),
        };
    }
    if let Some(rest) = reason.split("denied by the user's deny rule ").nth(1) {
        let id = rest.split_whitespace().next().unwrap_or_default();
        return format!("denied by rule {id}");
    }
    let table: [(&str, &str); 9] = [
        ("denied by the user", "denied by you"),
        ("the user denied this action", "denied by you"),
        ("the user stopped this turn", "stopped by you"),
        ("plan mode", "plan mode"),
        (REVIEW_INCOMPLETE, "review not completed"),
        ("safety classifier is unavailable", "classifier unavailable"),
        (
            "did not answer the approval request in time",
            "no answer in time",
        ),
        ("fixed policy", "fixed policy"),
        ("no client", "no one to ask"),
    ];
    if let Some((_, short)) = table.iter().find(|(needle, _)| reason.contains(needle)) {
        return (*short).to_string();
    }
    let reason = reason
        .trim_start_matches("Tool call denied")
        .trim_start_matches([':', ' ']);
    one_line(reason.lines().next().unwrap_or_default())
}

/// A failed call's reason: the first line of its result.
#[must_use]
pub fn failure_reason(result: &str) -> Option<String> {
    result
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(one_line)
}

/// `secs` in human units: `1.2s`, `48s`, `3m 2s`, `1h 5m`.
#[must_use]
pub fn human_secs(secs: f64) -> String {
    if secs < 10.0 {
        let shown = format!("{secs:.1}");
        return format!("{}s", shown.strip_suffix(".0").unwrap_or(&shown));
    }
    let whole = secs.round() as u64;
    match whole {
        0..60 => format!("{whole}s"),
        60..3_600 => {
            let (m, s) = (whole / 60, whole % 60);
            if s == 0 {
                format!("{m}m")
            } else {
                format!("{m}m {s}s")
            }
        }
        _ => format!("{}h {}m", whole / 3_600, whole % 3_600 / 60),
    }
}

/// A call's time as its line shows it: nothing under a second.
#[must_use]
pub fn time(ms: u64) -> Option<String> {
    (ms >= 1_000).then(|| human_secs(ms as f64 / 1_000.0))
}

/// How a line's state reads (its marker and colour).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// `○`: asked for, not started.
    Requested,
    /// `◐`: running.
    Running,
    /// `✓`: done.
    Done,
    /// `✗`: failed.
    Failed,
    /// `⊘`: denied.
    Denied,
}

impl Mark {
    /// The marker glyph.
    #[must_use]
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Requested => "○",
            Self::Running => "◐",
            Self::Done => "✓",
            Self::Failed => "✗",
            Self::Denied => "⊘",
        }
    }
}

/// A tool block's line, before it is fitted to a width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parts {
    /// Its state.
    pub mark: Mark,
    /// Verb and object.
    pub head: Head,
    /// The outcome (or the failure's or denial's reason).
    pub outcome: Option<String>,
    /// The time (`1.2s`), or the running clock.
    pub time: Option<String>,
}

impl Parts {
    /// The parts of a tool block's line: its state, `head`, `outcome`, the
    /// clock of a running call (`ticked` seconds) and the numbers of its
    /// `diff` (an edit's or a replaced file's `+A −R`).
    #[must_use]
    pub fn of(
        head: &Head,
        state: &super::ToolState,
        outcome: &Outcome,
        ticked: u64,
        diff: Option<&super::DiffView>,
    ) -> Self {
        use super::ToolState;
        // A replaced file's or an edit's numbers are its diff's; a new
        // file's are its own answer's.
        let numbers = match (&outcome.text, diff) {
            (Some(text), _) if text.starts_with("new") => Some(text.clone()),
            (_, Some(d)) => Some(format!("+{} −{}", d.added, d.removed)),
            (text, None) => text.clone(),
        };
        let (mark, outcome, time) = match state {
            ToolState::Requested => (Mark::Requested, None, None),
            ToolState::Running { .. } => (
                Mark::Running,
                None,
                (ticked >= 1).then(|| human_secs(ticked as f64)),
            ),
            ToolState::Done {
                is_error,
                duration_ms,
            } => (
                if *is_error || outcome.failed {
                    Mark::Failed
                } else {
                    Mark::Done
                },
                numbers,
                time(*duration_ms),
            ),
            ToolState::Denied { reason } => (Mark::Denied, Some(denial(reason)), None),
        };
        Self {
            mark,
            head: head.clone(),
            outcome,
            time,
        }
    }

    /// What follows the head: `  outcome · time`.
    #[must_use]
    pub fn tail(&self) -> String {
        match (&self.outcome, &self.time) {
            (Some(outcome), Some(time)) => format!("  {outcome} · {time}"),
            (Some(outcome), None) => format!("  {outcome}"),
            (None, Some(time)) => format!("  {time}"),
            (None, None) => String::new(),
        }
    }

    /// The whole line, unshortened (`/export`, the dialog).
    #[must_use]
    pub fn text(&self) -> String {
        format!("{} {}{}", self.mark.glyph(), self.head.text(), self.tail())
    }
}

/// A call's arguments as its expansion shows them: a shell command's
/// whole text as written, else the arguments pretty-printed; `None` for
/// none.
#[must_use]
pub fn expanded_args(name: &str, args: &Value) -> Option<String> {
    if base(name) == "shell"
        && let Some(command) = args.get("command").and_then(Value::as_str)
    {
        return Some(command.trim_end().to_string());
    }
    let empty = args.as_object().is_some_and(serde_json::Map::is_empty) || args.is_null();
    (!empty).then(|| serde_json::to_string_pretty(args).unwrap_or_default())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(name: &str, args: Value) -> String {
        head(name, &args, &PathView::default()).text()
    }

    #[test]
    fn each_tool_has_its_verb_and_object() {
        assert_eq!(
            line("read_file", json!({"path": "src/a.rs"})),
            "read src/a.rs"
        );
        assert_eq!(
            line("edit_file", json!({"path": "a.rs", "old_string": "x"})),
            "edit a.rs"
        );
        assert_eq!(line("write_file", json!({"path": "b.rs"})), "write b.rs");
        assert_eq!(
            line("shell", json!({"command": "cargo build"})),
            "run cargo build"
        );
        assert_eq!(
            line(
                "shell",
                json!({"command": "set -e\ncargo build\ncargo test"})
            ),
            "run set -e (+2 lines)"
        );
        assert_eq!(
            line("grep", json!({"pattern": "fn main", "glob": "*.rs"})),
            "search \"fn main\" in *.rs"
        );
        assert_eq!(
            line("glob", json!({"pattern": "**/*.toml"})),
            "find **/*.toml"
        );
        assert_eq!(
            line(
                "delegate",
                json!({"member_id": "scout", "task": "find\n the bug"})
            ),
            "delegate to scout: find the bug"
        );
        assert_eq!(
            line("fork_off", json!({"member_id": "kid", "task": "try it"})),
            "fork kid: try it"
        );
        assert_eq!(
            line(
                "spawn_many_members",
                json!({"specs": [{"member_id": "a"}, {"member_id": "b"}]})
            ),
            "spawn a, b"
        );
        assert_eq!(
            line(
                "send_message",
                json!({"peer_id": "p1", "display_name": "main-ab12", "body": "hi\nthere"})
            ),
            "message → main-ab12: hi there"
        );
        assert_eq!(
            line("task_create", json!({"subject": "Fix it"})),
            "task + Fix it"
        );
        assert_eq!(
            line("task_update", json!({"id": "t1", "status": "completed"})),
            "task ✓ t1"
        );
        assert_eq!(line("note_save", json!({"name": "style"})), "note style");
        assert_eq!(line("skill_read", json!({})), "skills");
        assert_eq!(
            line("tool_search", json!({"query": "mob"})),
            "tools \"mob\""
        );
        assert_eq!(
            line("watch_paths", json!({"patterns": ["src/**", "*.md"]})),
            "watch src/** *.md"
        );
        assert_eq!(
            line("meerkat_schedule_create", json!({"name": "nightly"})),
            "schedule nightly"
        );
        assert_eq!(
            line("member_status", json!({"member_id": "scout"})),
            "status scout"
        );
        // No row: the tool's name and its first string argument.
        assert_eq!(
            line("mcp_fetch", json!({"url": "https://example.com", "n": 3})),
            "mcp_fetch: https://example.com"
        );
        assert_eq!(line("mcp_ping", json!({})), "mcp_ping");
        // A helper's call keeps its tag.
        assert_eq!(
            line("[scout · m] read_file", json!({"path": "a"})),
            "[scout · m] read a"
        );
    }

    #[test]
    fn paths_are_relative_to_the_project_or_home() {
        let paths = PathView {
            root: Some("/w/proj".into()),
            home: Some("/home/u".into()),
        };
        assert_eq!(paths.show("/w/proj/src/a.rs"), "src/a.rs");
        assert_eq!(paths.show("/w/proj"), ".");
        assert_eq!(paths.show("/home/u/notes.md"), "~/notes.md");
        assert_eq!(paths.show("/etc/hosts"), "/etc/hosts");
        assert_eq!(paths.show("src/b.rs"), "src/b.rs");
    }

    #[test]
    fn the_object_is_shortened_and_the_rest_kept() {
        let path = Head::new(
            "read",
            "src/very/deep/directory/tree/file_name.rs",
            Cut::Middle,
        );
        let fit = path.fit(24);
        assert!(display_width(&fit) <= 24, "{fit}");
        assert!(fit.starts_with("read src/"), "{fit}");
        assert!(fit.ends_with("file_name.rs"), "{fit}");
        assert!(fit.contains('…'), "{fit}");
        let mut command = Head::new("run", "cargo test --all-features --workspace", Cut::End);
        command.more = Some("(+2 lines)".into());
        let fit = command.fit(24);
        assert!(display_width(&fit) <= 24, "{fit}");
        assert!(fit.starts_with("run cargo"), "{fit}");
        assert!(fit.ends_with("… (+2 lines)"), "{fit}");
    }

    #[test]
    fn outcomes_come_from_the_tools_own_answers() {
        let o = |name: &str, args: Value, result: &str| outcome(name, &args, result).text;
        assert_eq!(
            o("read_file", json!({}), "     1\ta\n     2\tb\n"),
            Some("2 lines".into())
        );
        assert_eq!(
            o(
                "read_file",
                json!({}),
                "    10\ta\n    11\tb\n… [showing lines 10-11 of 40; use offset=12 to continue]\n"
            ),
            Some("lines 10–11".into())
        );
        assert_eq!(
            o("write_file", json!({}), "created src/x.rs (12 lines)"),
            Some("new, 12 lines".into())
        );
        assert_eq!(o("write_file", json!({}), "updated src/x.rs (+1 -1)"), None);
        assert_eq!(o("shell", json!({}), "exit code 0 (0.1s)\nok"), None);
        let failed = outcome("shell", &json!({}), "exit code 101 (3.0s)\nerror");
        assert_eq!(failed.text.as_deref(), Some("exit 101"));
        assert!(failed.failed);
        assert_eq!(
            o(
                "shell",
                json!({}),
                "timed out after 600.0s; the process was terminated"
            ),
            Some("timed out after 10m".into())
        );
        assert_eq!(
            o("grep", json!({"pattern": "x"}), "a.rs\nb.rs\n"),
            Some("2 files".into())
        );
        assert_eq!(
            o(
                "grep",
                json!({"pattern": "x", "output_mode": "count"}),
                "a.rs:3\nb.rs:1\n"
            ),
            Some("4 matches in 2 files".into())
        );
        assert_eq!(
            o(
                "grep",
                json!({"pattern": "x", "output_mode": "content"}),
                "a.rs:3:x\na.rs:9:x\n"
            ),
            Some("2 matches in 1 file".into())
        );
        assert_eq!(
            o(
                "grep",
                json!({"pattern": "x", "output_mode": "content", "context": 2}),
                "a.rs:3:x\n"
            ),
            None,
            "context lines look like matches: no number"
        );
        assert_eq!(
            o("grep", json!({"pattern": "x"}), "No matches for `x`\n"),
            Some("no matches".into())
        );
        assert_eq!(
            o(
                "glob",
                json!({}),
                "a\nb\n[partial: the search stopped after 10.0 s and 5 entries examined, because x.]\n"
            ),
            Some("2 files (stopped early)".into())
        );
        assert_eq!(
            o(
                "glob",
                json!({}),
                "a\nb\n… [results truncated: showing 2 of 340 matches; use a narrower pattern or path]\n"
            ),
            Some("340 files".into())
        );
        assert_eq!(
            o("task_list", json!({}), "[{\"id\":1},{\"id\":2}]"),
            Some("2".into())
        );
        assert_eq!(o("note_forget", json!({}), "ok"), Some("removed".into()));
    }

    #[test]
    fn denials_and_times_in_few_words() {
        assert_eq!(
            denial(
                "Tool call denied (auto_mode): Blocked by auto-mode safety policy: the safety classifier blocked it: force push [R1]. Do not"
            ),
            "blocked by the classifier (R1)"
        );
        assert_eq!(
            denial(
                "Blocked by auto-mode safety policy: denied by the user's deny rule u3 (`shell rm`); only"
            ),
            "denied by rule u3"
        );
        assert_eq!(
            denial("Blocked: the user denied this action in the approval dialog"),
            "denied by you"
        );
        assert_eq!(denial(crate::approval::PLAN_MODE_DENIAL), "plan mode");
        assert_eq!(time(999), None);
        assert_eq!(time(1_200).as_deref(), Some("1.2s"));
        assert_eq!(time(48_000).as_deref(), Some("48s"));
        assert_eq!(time(182_000).as_deref(), Some("3m 2s"));
    }
}
