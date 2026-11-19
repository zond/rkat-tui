//! The `/permissions` dialog: one [`ListPicker`] with two sections, Tab
//! and Shift+Tab between them. **Blocked**: the calls the gate recently
//! blocked and the approvals once that wait for the agent, to approve
//! once, for this session or always, or to allow their kind of effect.
//! **In force**: the approval rules, grants and statements with where
//! each comes from, to remove, and `n` (a statement) and `r` (a rule) to
//! add one. The rows are built here from the daemon's lists; the runner
//! asks for them and does what the keys choose.

use ratatui::crossterm::event::KeyCode;

use super::picker::{Field, FieldFor, ListPicker, Page, PickAct, PickKey, PickRow, PickerKind};
use crate::approval::desk::RuleRow;
use crate::approval::grants::{BlockedCall, GrantRow, GrantScope};
use crate::approval::statements::StatementRow;

/// A section of the `/permissions` dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// The calls recently blocked and the approvals waiting to be used.
    Blocked,
    /// The rules, grants and statements in force.
    InForce,
}

impl Section {
    /// The section's index in the dialog.
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::Blocked => 0,
            Self::InForce => 1,
        }
    }
}

/// The footer of the Blocked section, before its Tab and Esc.
const BLOCKED_FOOTER: &str = "Enter approve once · s grant this session · a grant always here · A always in every workspace · e allow this kind of effect";

/// What the Blocked footer adds when a row shows only part of its call.
const WHOLE_CALL_FOOTER: &str = " · Ctrl+O the whole call";

/// What the Blocked footer adds when a call waits for the agent.
const WITHDRAW_FOOTER: &str = " · w withdraw a waiting approval";

/// The footer of the In force section, before its Tab and Esc.
const IN_FORCE_FOOTER: &str = "↑/↓ move · d remove · n new statement · r new rule";

/// What a key other than `w` says on a call approved once.
const WAITING_REFUSAL: &str = "approved once already: it waits for the agent to run it again; w withdraws the approval, to approve it otherwise";

/// What a key of the other section says here.
const IN_FORCE_KEY: &str = "that key acts in In force: Tab switches to it";

/// What a key of the other section says here.
const BLOCKED_KEY: &str = "that key acts on a blocked call: Tab switches to Blocked";

/// The `/permissions` dialog over `calls` (the Blocked section) and the
/// rules, grants and statements in force, showing `open`: when `None`,
/// Blocked when it has a row, else In force.
#[must_use]
pub fn permissions_dialog(
    calls: &[BlockedCall],
    now: u64,
    rules: &[RuleRow],
    grants: &[GrantRow],
    statements: &[StatementRow],
    open: Option<Section>,
) -> ListPicker {
    let open = open.unwrap_or(if calls.is_empty() {
        Section::InForce
    } else {
        Section::Blocked
    });
    ListPicker::sectioned(
        PickerKind::Permissions,
        vec![
            ("Blocked", blocked_page(calls, now)),
            ("In force", in_force_page(rules, grants, statements)),
        ],
        open.index(),
    )
}

/// The Blocked section: the calls the gate recently blocked and the
/// calls approved once that wait for the agent to run them again, newest
/// first. On a blocked call Enter or `o` approves it once, `s` for this
/// session, `a` always for this workspace, `A` always in every workspace
/// (the user's own `tui.toml`; a call no grant fits says so): each exact,
/// the call itself or its directory or literal command. `e` opens a
/// one-line field with the kind of effect the classifier objected to
/// (empty when it named none), to edit and allow for this session or
/// always: a statement the classifier judges. A call approved once is
/// marked so; `w` withdraws its approval (it is a blocked call again),
/// and every other key says so.
fn blocked_page(calls: &[BlockedCall], now: u64) -> Page {
    let rows = calls
        .iter()
        .rev()
        .map(|call| {
            let who = call
                .helper
                .as_ref()
                .map_or_else(String::new, |helper| format!(" (helper {helper})"));
            let rule = call
                .rule
                .as_ref()
                .map_or_else(String::new, |rule| format!(" [{rule}]"));
            let ago = crate::text::span(now.saturating_sub(call.at), 1);
            if call.approved_once {
                return waiting_row(call, &who, &rule, &ago);
            }
            let once = PickAct::Approve(call.id, GrantScope::Once);
            let approve_once = |code| match call.call_bytes {
                // Nobody could read all of it: the daemon refuses too.
                Some(bytes) => PickKey::refused(
                    code,
                    crate::approval::blocks::too_long_to_approve(call.id, bytes),
                ),
                None => PickKey::now(code, once.clone()),
            };
            let mut keys = vec![
                approve_once(KeyCode::Enter),
                approve_once(KeyCode::Char('o')),
                PickKey::refused(
                    KeyCode::Char('w'),
                    "this call is not approved once; there is nothing to withdraw",
                ),
            ];
            match &call.grant {
                Some(grant) => {
                    let what = grant.describe();
                    keys.push(PickKey::confirmed(
                        KeyCode::Char('s'),
                        PickAct::Approve(call.id, GrantScope::Session),
                        format!("Grant {what} for this session?"),
                    ));
                    keys.push(PickKey::confirmed(
                        KeyCode::Char('a'),
                        PickAct::Approve(call.id, GrantScope::Always),
                        format!("Grant {what} always (written to the realm's tui.toml)?"),
                    ));
                    keys.push(PickKey::confirmed(
                        KeyCode::Char('A'),
                        PickAct::Approve(call.id, GrantScope::AlwaysUser),
                        format!(
                            "Grant {what} always, in every workspace (written to your own tui.toml)?"
                        ),
                    ));
                }
                None => {
                    for code in ['s', 'a', 'A'] {
                        keys.push(PickKey::refused(
                            KeyCode::Char(code),
                            "no grant fits this call; Enter approves it once",
                        ));
                    }
                }
            }
            keys.push(PickKey::field(
                KeyCode::Char('e'),
                Field {
                    what: FieldFor::Effect {
                        id: call.id,
                        scope: GrantScope::Session,
                    },
                    text: call.effect.clone().unwrap_or_default(),
                },
            ));
            let grant = call.grant.as_ref().map_or_else(String::new, |grant| {
                format!(" · s/a exact: {}", grant.describe())
            });
            let effect = call.effect.as_ref().map_or_else(
                || " · e judged: a kind of effect you type".to_string(),
                |effect| format!(" · e judged: \"{effect}\""),
            );
            let row = PickRow::new(
                format!(
                    "#{} {}{who}: {}{}{rule} · {ago} ago",
                    call.id,
                    call.tool,
                    call.summary,
                    more_marker(call)
                ),
                keys,
            )
            .noted(format!("{}{grant}{effect}", call.reason))
            .keyed(call.id.to_string());
            with_whole_call(row, call, vec![KeyCode::Enter, KeyCode::Char('o')])
        })
        .collect();
    let withdraw = if calls.iter().any(|call| call.approved_once) {
        WITHDRAW_FOOTER
    } else {
        ""
    };
    let whole = if calls.iter().any(row_cuts) {
        WHOLE_CALL_FOOTER
    } else {
        ""
    };
    Page::new(
        rows,
        "nothing was blocked recently",
        format!("{BLOCKED_FOOTER}{whole}{withdraw} · Tab In force · Esc close"),
        ['d', 'n', 'r']
            .into_iter()
            .map(|code| PickKey::refused(KeyCode::Char(code), IN_FORCE_KEY))
            .collect(),
    )
}

/// The Blocked row of `call`, approved once and waiting for the agent:
/// `w` withdraws the approval; the keys that approve say it is approved
/// already.
fn waiting_row(call: &BlockedCall, who: &str, rule: &str, ago: &str) -> PickRow {
    let mut keys = vec![PickKey::now(KeyCode::Char('w'), PickAct::Withdraw(call.id))];
    keys.extend(
        [
            KeyCode::Enter,
            KeyCode::Char('o'),
            KeyCode::Char('s'),
            KeyCode::Char('a'),
            KeyCode::Char('A'),
            KeyCode::Char('e'),
        ]
        .into_iter()
        .map(|code| PickKey::refused(code, WAITING_REFUSAL)),
    );
    let row = PickRow::new(
        format!(
            "#{} {}{who}: {}{}{rule} · approved once · waiting for the agent to run it again",
            call.id,
            call.tool,
            call.summary,
            more_marker(call)
        ),
        keys,
    )
    .noted(format!(
        "blocked {ago} ago: {} · w withdraws the approval (the call is blocked again)",
        call.reason
    ))
    .keyed(call.id.to_string());
    with_whole_call(row, call, Vec::new())
}

/// Whether `call`'s row shows less than the whole call an approval once
/// approves.
fn row_cuts(call: &BlockedCall) -> bool {
    !call.call.is_empty() && call.call.trim() != call.summary.trim()
}

/// What `call`'s row says after its summary when it shows less than the
/// whole call.
fn more_marker(call: &BlockedCall) -> &'static str {
    if row_cuts(call) {
        " … (Ctrl+O: the whole call)"
    } else {
        ""
    }
}

/// `row` with `call`'s whole call behind Ctrl+O when the row shows less
/// of it; `view_first` act only once it was shown (an approval once
/// approves what nobody read otherwise). A call cut for its size says so
/// at its end.
fn with_whole_call(row: PickRow, call: &BlockedCall, view_first: Vec<KeyCode>) -> PickRow {
    if !row_cuts(call) {
        return row;
    }
    let detail = match call.call_bytes {
        Some(bytes) => format!(
            "{}\n… (cut: the call is {bytes} bytes, {} shown)",
            call.call,
            call.call.len()
        ),
        None => call.call.clone(),
    };
    row.detailed(detail, view_first)
}

/// The In force section: a row per rule (number, allow or deny, tool,
/// pattern or literal, layer), per grant and per statement, each with
/// where it comes from; `d` removes one after a confirmation that says
/// how long the removal lasts (for good for the realm's own rules, until
/// the daemon restarts for those written in another layer, as
/// `/permissions rm` does), or says where to drop it. `n` opens the
/// field of a new statement, `r` the form of a new rule.
fn in_force_page(rules: &[RuleRow], grants: &[GrantRow], statements: &[StatementRow]) -> Page {
    use crate::approval::grants::GrantOrigin;
    use crate::approval::rules::RuleOrigin;
    use crate::approval::statements::StatementOrigin;
    let grant_rows = grants.iter().map(|grant| {
        let what = grant.target.describe();
        let key = if grant.removable {
            PickKey::confirmed(
                KeyCode::Char('d'),
                PickAct::RemoveGrant(grant.id),
                format!("Remove the session grant of {what}?"),
            )
        } else {
            PickKey::refused(
                KeyCode::Char('d'),
                match grant.origin {
                    GrantOrigin::Default => "the sandbox grants it by default",
                    GrantOrigin::Flag => "it comes from the daemon's command line",
                    GrantOrigin::Always => "it is in the realm's tui.toml ([confinement] extra_write): edit that file to drop it",
                    GrantOrigin::Config | GrantOrigin::Session => "it is in a tui.toml ([confinement]): edit that file to drop it",
                },
            )
        };
        PickRow::new(
            format!("grant {what} · {}", grant.origin.word()),
            vec![key],
        )
        .keyed(format!("grant {}", grant.id))
    });
    let statement_rows = statements.iter().map(|statement| {
        let key = match statement.origin {
            StatementOrigin::Config => PickKey::refused(
                KeyCode::Char('d'),
                "it is in your own tui.toml ([auto] allow): edit that file to drop it",
            ),
            StatementOrigin::Session => PickKey::confirmed(
                KeyCode::Char('d'),
                PickAct::RemoveStatement(statement.id),
                format!("Stop allowing \"{}\" for this session?", statement.text),
            ),
            StatementOrigin::Always => PickKey::confirmed(
                KeyCode::Char('d'),
                PickAct::RemoveStatement(statement.id),
                format!(
                    "Stop allowing \"{}\"? It leaves the realm's tui.toml.",
                    statement.text
                ),
            ),
        };
        PickRow::new(
            format!(
                "allow kind \"{}\" · {}",
                statement.text,
                statement.origin.word()
            ),
            vec![key],
        )
        .noted("a statement: judged by the classifier, read narrowly")
        .keyed(format!("statement {}", statement.id))
    });
    let rows = rules
        .iter()
        .map(|rule| {
            let kind = if rule.deny { "deny" } else { "allow" };
            let what = match (&rule.arg, rule.literal) {
                (None, _) => format!("{kind} {} (every call)", rule.tool),
                (Some(arg), true) => format!("{kind} {} literal `{arg}`", rule.tool),
                (Some(arg), false) => format!("{kind} {} pattern `{arg}`", rule.tool),
            };
            let lasts = match rule.origin {
                RuleOrigin::Realm | RuleOrigin::Session => {
                    "It leaves the workspace's rules for good".to_string()
                }
                RuleOrigin::Global => "It is written in your own tui.toml: it is removed from that file, for every workspace".to_string(),
                RuleOrigin::Project => "It is written in the project's .rkat/tui.toml, so it is removed until the daemon restarts (edit that file to drop it for good)".to_string(),
                RuleOrigin::Cli => "It comes from the daemon's command line, so it is removed until the daemon restarts".to_string(),
            };
            let n = usize::try_from(rule.n).unwrap_or(usize::MAX);
            PickRow::new(
                format!("{}. {what} · {}", rule.n, rule.origin.as_str()),
                vec![PickKey::confirmed(
                    KeyCode::Char('d'),
                    PickAct::RemoveRule(n),
                    format!("Remove rule {}? {lasts}.", rule.n),
                )],
            )
            // Rules are numbered again after a removal: what the rule
            // says is what it is.
            .keyed(format!("rule {what} · {}", rule.origin.as_str()))
        })
        .chain(grant_rows)
        .chain(statement_rows)
        .collect();
    let mut keys = vec![
        PickKey::field(
            KeyCode::Char('n'),
            Field {
                what: FieldFor::Statement {
                    scope: GrantScope::Session,
                },
                text: String::new(),
            },
        ),
        PickKey::field(
            KeyCode::Char('r'),
            Field {
                what: FieldFor::Rule {
                    deny: false,
                    user: false,
                    literal: false,
                },
                text: String::new(),
            },
        ),
    ];
    keys.extend(
        ['o', 's', 'a', 'A', 'e', 'w']
            .into_iter()
            .map(|code| PickKey::refused(KeyCode::Char(code), BLOCKED_KEY)),
    );
    Page::new(
        rows,
        "no approval rules, grants or statements yet (n adds a statement, r a rule)",
        format!("{IN_FORCE_FOOTER} · Tab Blocked · Esc close"),
        keys,
    )
}
