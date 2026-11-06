//! The user's external editor: `/memory` (an instruction file) and Ctrl+G /
//! Ctrl+X Ctrl+E (the draft) open `$VISUAL`, else `$EDITOR`, else `vi`.
//!
//! The UI loop leaves TUI mode around a run (see `runtime.rs`); this module
//! holds the parts that need no terminal: which editor, the launch itself
//! (the editor runs as the user, unconfined, with the TUI's own environment)
//! and the draft's temporary file.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The editor to run: a program and the arguments that come before the
/// file (`code --wait` is the program `code` and the argument `--wait`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorCommand {
    /// The program, as the variable names it (found on `PATH` when bare).
    pub program: PathBuf,
    /// The arguments before the file.
    pub args: Vec<String>,
}

/// The editor from `visual` (`$VISUAL`), else `editor` (`$EDITOR`), else
/// `vi`. A variable counts when it is set and not empty; its value is split
/// on whitespace.
///
/// # Errors
///
/// The chosen variable holds only whitespace: it names no program.
pub fn editor_command(
    visual: Option<&str>,
    editor: Option<&str>,
) -> Result<EditorCommand, String> {
    let chosen = [("$VISUAL", visual), ("$EDITOR", editor)]
        .into_iter()
        .find_map(|(name, value)| value.filter(|v| !v.is_empty()).map(|v| (name, v)));
    let Some((name, value)) = chosen else {
        return Ok(EditorCommand {
            program: PathBuf::from("vi"),
            args: Vec::new(),
        });
    };
    let mut words = value.split_whitespace();
    let Some(program) = words.next() else {
        return Err(format!("{name} names no program"));
    };
    Ok(EditorCommand {
        program: PathBuf::from(program),
        args: words.map(str::to_string).collect(),
    })
}

/// [`editor_command`] from `$VISUAL` and `$EDITOR`.
///
/// # Errors
///
/// As [`editor_command`].
pub fn editor_from_env() -> Result<EditorCommand, String> {
    let visual = std::env::var("VISUAL").ok();
    let editor = std::env::var("EDITOR").ok();
    editor_command(visual.as_deref(), editor.as_deref())
}

/// Run `command` on `path` with the terminal (inherited stdio) and wait for
/// it. Blocking.
///
/// # Errors
///
/// A description when the editor cannot start or exits unsuccessfully.
pub fn run_editor(command: &EditorCommand, path: &Path) -> Result<(), String> {
    let program = command.program.display();
    match Command::new(&command.program)
        .args(&command.args)
        .arg(path)
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("{program} exited with {status}")),
        Err(err) => Err(format!("could not start {program}: {err}")),
    }
}

/// Write the draft to a fresh private temporary file in `dir` (created if
/// missing). The file is removed when the handle drops.
///
/// # Errors
///
/// Any I/O error creating or writing the file.
pub fn write_draft(dir: &Path, text: &str) -> io::Result<tempfile::NamedTempFile> {
    std::fs::create_dir_all(dir)?;
    let mut file = tempfile::Builder::new()
        .prefix("draft-")
        .suffix(".md")
        .tempfile_in(dir)?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    Ok(file)
}

/// The edited draft: the file's text with one trailing newline (the one
/// editors add) removed.
///
/// # Errors
///
/// Any I/O error reading the file, or text that is not UTF-8.
pub fn read_draft(path: &Path) -> io::Result<String> {
    let mut text = std::fs::read_to_string(path)?;
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    Ok(text)
}

/// Write `text` to a temporary file in `scratch`, let `run` edit it and read
/// it back; the file is removed afterwards.
///
/// # Errors
///
/// A description when the file cannot be written or read, or `run` fails.
pub fn edit_draft(
    text: &str,
    scratch: &Path,
    run: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<String, String> {
    let file = write_draft(scratch, text)
        .map_err(|err| format!("could not write the draft to {}: {err}", scratch.display()))?;
    run(file.path())?;
    read_draft(file.path()).map_err(|err| format!("could not read the edited draft: {err}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn cmd(program: &str, args: &[&str]) -> EditorCommand {
        EditorCommand {
            program: PathBuf::from(program),
            args: args.iter().map(|a| (*a).to_string()).collect(),
        }
    }

    #[test]
    fn the_editor_is_visual_then_editor_then_vi() {
        assert_eq!(
            editor_command(Some("code --wait"), Some("nano")),
            Ok(cmd("code", &["--wait"]))
        );
        assert_eq!(
            editor_command(Some(" emacsclient  -t "), None),
            Ok(cmd("emacsclient", &["-t"]))
        );
        assert_eq!(editor_command(Some(""), Some("nano")), Ok(cmd("nano", &[])));
        assert_eq!(editor_command(None, Some("hx")), Ok(cmd("hx", &[])));
        assert_eq!(editor_command(None, Some("")), Ok(cmd("vi", &[])));
        assert_eq!(editor_command(None, None), Ok(cmd("vi", &[])));
    }

    #[test]
    fn a_blank_editor_variable_is_refused() {
        assert_eq!(
            editor_command(Some("   "), Some("nano")),
            Err("$VISUAL names no program".to_string())
        );
        assert_eq!(
            editor_command(None, Some("\t")),
            Err("$EDITOR names no program".to_string())
        );
    }

    #[test]
    fn a_draft_round_trips_through_its_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("tui");
        let mut seen = None;
        let result = edit_draft("first line\nsecond", &scratch, |path| {
            assert!(path.starts_with(&scratch));
            seen = Some(std::fs::read_to_string(path).unwrap());
            std::fs::write(path, "edited\ntext\n").unwrap();
            Ok(())
        });
        assert_eq!(seen.as_deref(), Some("first line\nsecond"));
        assert_eq!(result, Ok("edited\ntext".to_string()));
        // The temporary file is gone.
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
        // Only one trailing newline goes (and a CR before it).
        let result = edit_draft("", &scratch, |path| {
            std::fs::write(path, "a\n\n").unwrap();
            Ok(())
        });
        assert_eq!(result, Ok("a\n".to_string()));
        let result = edit_draft("", &scratch, |path| {
            std::fs::write(path, "b\r\n").unwrap();
            Ok(())
        });
        assert_eq!(result, Ok("b".to_string()));
    }

    #[test]
    fn a_failed_editor_reports_and_keeps_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let result = edit_draft("keep", dir.path(), |_| Err("vi exited with 1".into()));
        assert_eq!(result, Err("vi exited with 1".to_string()));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        let missing = run_editor(&cmd("/nonexistent/editor-binary", &[]), dir.path());
        assert!(missing.is_err_and(|e| e.starts_with("could not start")));
    }

    #[cfg(unix)]
    #[test]
    fn a_real_editor_command_edits_the_draft() {
        let dir = tempfile::tempdir().unwrap();
        // `sh -c 'printf edited > "$0"'` stands in for an editor: the file
        // is the argument after the leading ones.
        let editor = cmd("sh", &["-c", "printf 'edited\\n' > \"$0\""]);
        assert_eq!(
            edit_draft("old", dir.path(), |path| run_editor(&editor, path)),
            Ok("edited".to_string())
        );
        let failing = cmd("false", &[]);
        assert!(
            edit_draft("old", dir.path(), |path| run_editor(&failing, path))
                .is_err_and(|e| e.contains("exited"))
        );
    }
}
