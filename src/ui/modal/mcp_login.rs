//! The `/mcp login` dialog's state and keys: first, for a server of the
//! project's `mcp.toml`, the question whether to log into its URL; then the
//! authorize URL (a link, and plain text to copy) with a field for the
//! address the browser shows after the login, when its redirect cannot
//! reach this machine.
//!
//! What the field holds is the login's code: it goes to the client runner
//! alone ([`crate::ui::Effect::McpLoginPasted`]), never to the input
//! editor, its history, the drafts or the transcript. `Debug` shows
//! neither it nor the authorize URL.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{ModalKey, ctrl_c_answer, is_held, push_line_text};

/// The most characters the paste field takes.
const MAX_FIELD: usize = 8 * 1024;

/// A secret text (the authorize URL, a pasted address): `Debug` redacts it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// What the login dialog shows once the daemon admitted the attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpLoginShown {
    pub server: String,
    /// The address to open in a browser.
    pub authorize_url: Secret,
    /// Where the browser is sent back.
    pub redirect_uri: String,
    /// The listener could not be bound (a port in use): only the paste
    /// works, and this says why.
    pub note: Option<String>,
}

/// The `/mcp login` dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpLoginDialog {
    /// A project server: log into `url`? (`y`/`n`).
    Confirm { server: String, url: String },
    /// The attempt runs: the URL, the field, and why the last paste was
    /// refused.
    Waiting {
        shown: McpLoginShown,
        field: Secret,
        refused: Option<String>,
    },
}

/// What a key in the dialog asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpLoginAct {
    /// `y` on the question: start the login, confirmed.
    Confirmed(String),
    /// Enter on the field: this address.
    Pasted(Secret),
    /// Ctrl+Y: copy the authorize URL.
    Copy(Secret),
}

impl McpLoginDialog {
    /// Type (paste) `text` into the field.
    pub fn type_text(&mut self, text: &str) {
        if let Self::Waiting { field, refused, .. } = self {
            push_line_text(&mut field.0, text.trim(), MAX_FIELD);
            *refused = None;
        }
    }

    /// The refusal of the last paste, shown under the field.
    pub fn refuse(&mut self, why: String) {
        if let Self::Waiting { refused, .. } = self {
            *refused = Some(why);
        }
    }

    /// A key: the question takes `y` and `n`/Esc; the field takes text,
    /// Backspace, Ctrl+U (clear), Enter (the address), Ctrl+Y (copy the
    /// URL); Esc and Ctrl-C close it (the attempt is cancelled).
    pub fn key(&mut self, key: &KeyEvent) -> ModalKey<McpLoginAct> {
        if let Some(answer) = ctrl_c_answer(key) {
            return answer;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match self {
            Self::Confirm { server, .. } => {
                if is_held(key) || ctrl {
                    return ModalKey::Stay;
                }
                match key.code {
                    KeyCode::Char('y' | 'Y') => {
                        ModalKey::Choose(McpLoginAct::Confirmed(server.clone()))
                    }
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => ModalKey::Close,
                    _ => ModalKey::Stay,
                }
            }
            Self::Waiting {
                shown,
                field,
                refused,
            } => match key.code {
                KeyCode::Esc if !is_held(key) => ModalKey::Close,
                KeyCode::Enter if !is_held(key) => {
                    if field.0.trim().is_empty() {
                        ModalKey::Stay
                    } else {
                        ModalKey::Choose(McpLoginAct::Pasted(field.clone()))
                    }
                }
                KeyCode::Char('y') if ctrl && !is_held(key) => {
                    ModalKey::Choose(McpLoginAct::Copy(shown.authorize_url.clone()))
                }
                KeyCode::Char('u') if ctrl => {
                    field.0.clear();
                    *refused = None;
                    ModalKey::Stay
                }
                KeyCode::Backspace => {
                    field.0.pop();
                    ModalKey::Stay
                }
                KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                    push_line_text(&mut field.0, &c.to_string(), MAX_FIELD);
                    *refused = None;
                    ModalKey::Stay
                }
                _ => ModalKey::Stay,
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn waiting() -> McpLoginDialog {
        McpLoginDialog::Waiting {
            shown: McpLoginShown {
                server: "docs".into(),
                authorize_url: Secret("https://as.example/authorize?state=s".into()),
                redirect_uri: "http://127.0.0.1:1/callback".into(),
                note: None,
            },
            field: Secret(String::new()),
            refused: None,
        }
    }

    /// The field takes typed and pasted text, Enter hands it over, Ctrl+Y
    /// copies the URL, Esc closes; nothing of either shows in `Debug`.
    #[test]
    fn the_field_takes_the_address_and_hides_it_and_the_url_from_debug() {
        let mut dialog = waiting();
        assert_eq!(dialog.key(&key(KeyCode::Enter)), ModalKey::Stay);
        dialog.type_text("  http://127.0.0.1:1/callback?code=c");
        assert_eq!(dialog.key(&key(KeyCode::Char('x'))), ModalKey::Stay);
        assert_eq!(dialog.key(&key(KeyCode::Backspace)), ModalKey::Stay);
        dialog.refuse("not it".into());
        let ModalKey::Choose(McpLoginAct::Pasted(Secret(text))) = dialog.key(&key(KeyCode::Enter))
        else {
            panic!("no paste");
        };
        assert_eq!(text, "http://127.0.0.1:1/callback?code=c");
        let shown = format!("{dialog:?}");
        assert!(
            !shown.contains("code=c") && !shown.contains("state=s"),
            "{shown}"
        );
        let copy = dialog.key(&KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert!(matches!(copy, ModalKey::Choose(McpLoginAct::Copy(_))));
        assert_eq!(dialog.key(&key(KeyCode::Esc)), ModalKey::Close);
    }

    /// The question answers `y` with the server, `n` and Esc close it.
    #[test]
    fn the_question_takes_y_or_n() {
        let mut dialog = McpLoginDialog::Confirm {
            server: "docs".into(),
            url: "https://docs.example/mcp".into(),
        };
        assert_eq!(dialog.key(&key(KeyCode::Enter)), ModalKey::Stay);
        assert_eq!(
            dialog.key(&key(KeyCode::Char('y'))),
            ModalKey::Choose(McpLoginAct::Confirmed("docs".into()))
        );
        assert_eq!(dialog.key(&key(KeyCode::Char('n'))), ModalKey::Close);
    }
}
