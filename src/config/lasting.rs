//! The lasting permissions the user's own layers give: their allow and
//! deny rules, their `[auto] allow` statements and their `[confinement]`
//! directories ([`Lasting::of`]). The one computation of them: the start
//! takes it ([`super::load_layered_with`]), and a running daemon takes it
//! again whenever the user's or the workspace's `tui.toml` changes, so a
//! command and a file change leave the engine holding what the files say
//! (`crate::approval::ApprovalEngine::replace_lasting`).
//!
//! The project's `.rkat/tui.toml` is not one of these layers: what it may
//! contribute (deny rules) is taken at start only.

use crate::approval::hosts::{HostOrigin, TrustedHost};
use crate::approval::rules::{Rule, RuleKind, RuleOrigin, RulesError};
use crate::approval::statements::{MAX_STATEMENTS, StatementOrigin, StatementRefusal, normalize};
use crate::bootstrap::confinement::ConfinementSection;

use super::file::RulesFile;

/// What the user's own layers allow and deny for every session.
#[derive(Debug, Default)]
pub struct Lasting {
    /// The rules, in layer order, each tagged with its layer.
    pub rules: Vec<Rule>,
    /// The statements, checked, none twice, at most [`MAX_STATEMENTS`].
    pub statements: Vec<(StatementOrigin, String)>,
    /// The `[confinement]` directories, in layer order, not yet validated
    /// (`crate::bootstrap::confinement` checks them).
    pub confinement: ConfinementSection,
    /// The `[auto] trusted_hosts`, checked
    /// ([`crate::approval::hosts::HostPattern::parse`]), none twice, at
    /// most [`crate::approval::hosts::MAX_TRUSTED_HOSTS`], each with every
    /// layer that names it.
    pub trusted_hosts: Vec<crate::approval::hosts::TrustedHost>,
    /// The entries not taken, with why.
    pub skipped: Vec<Skipped>,
}

/// An entry of a layer not taken.
#[derive(Debug)]
pub enum Skipped {
    /// A rule that does not parse (a start refuses the file).
    Rule {
        /// Its layer.
        origin: RuleOrigin,
        /// Why.
        error: RulesError,
    },
    /// A statement refused ([`normalize`], or one too many).
    Statement {
        /// Its layer.
        origin: RuleOrigin,
        /// The statement as written, cut at 80 characters.
        text: String,
        /// Why.
        refusal: StatementRefusal,
    },
    /// A trusted host refused (no host name, too broad, one too many).
    TrustedHost {
        /// Its layer.
        origin: RuleOrigin,
        /// The entry as written, cut at 80 characters.
        text: String,
        /// Why.
        why: String,
    },
}

impl Skipped {
    /// The layer of the entry.
    #[must_use]
    pub fn origin(&self) -> RuleOrigin {
        match self {
            Self::Rule { origin, .. }
            | Self::Statement { origin, .. }
            | Self::TrustedHost { origin, .. } => *origin,
        }
    }

    /// What was skipped, where and why, in words.
    #[must_use]
    pub fn describe(&self) -> String {
        let file = match self.origin() {
            RuleOrigin::Global => "your tui.toml",
            RuleOrigin::Realm => "this workspace's tui.toml",
            _ => "the command line",
        };
        match self {
            Self::Rule { error, .. } => format!(
                "a rule of {file} ({error}; the next start refuses the file until it is fixed)"
            ),
            Self::Statement { text, refusal, .. } => {
                format!("the statement {text:?} of {file} ({refusal})")
            }
            Self::TrustedHost { text, why, .. } => {
                format!("the trusted host {text:?} of {file} ({why})")
            }
        }
    }
}

impl Lasting {
    /// The lasting permissions of `layers` (each a user layer: the user's
    /// own file [`RuleOrigin::Global`], the workspace's
    /// [`RuleOrigin::Realm`], the command line's [`RuleOrigin::Cli`]), in
    /// that order: rules accumulate, statements accumulate checked (a
    /// duplicate is dropped quietly, a refused one or one past
    /// [`MAX_STATEMENTS`] skipped), directories accumulate.
    #[must_use]
    pub fn of(layers: &[(RuleOrigin, &RulesFile)]) -> Self {
        let mut lasting = Self::default();
        for (origin, file) in layers {
            lasting.add_layer(*origin, file);
        }
        lasting
    }

    fn add_layer(&mut self, origin: RuleOrigin, file: &RulesFile) {
        let entries = file
            .allow
            .iter()
            .map(|entry| (RuleKind::Allow, entry))
            .chain(file.deny.iter().map(|entry| (RuleKind::Deny, entry)));
        for (kind, entry) in entries {
            match Rule::from_entry(kind, entry, origin) {
                Ok(rule) => self.rules.push(rule),
                Err(error) => self.skipped.push(Skipped::Rule { origin, error }),
            }
        }
        if let Some(section) = &file.confinement {
            self.confinement.extend_from(section);
        }
        let Some(auto) = &file.auto else {
            return;
        };
        let host_origin = match origin {
            RuleOrigin::Global => HostOrigin::User,
            RuleOrigin::Realm => HostOrigin::Workspace,
            _ => HostOrigin::CommandLine,
        };
        for raw in &auto.trusted_hosts {
            let parsed = crate::approval::hosts::HostPattern::parse(raw);
            if let Ok(pattern) = &parsed
                && let Some(known) = self
                    .trusted_hosts
                    .iter_mut()
                    .find(|h| h.pattern == pattern.as_str())
            {
                if !known.origins.contains(&host_origin) {
                    known.origins.push(host_origin);
                }
                continue;
            }
            let taken = parsed.and_then(|pattern| {
                if self.trusted_hosts.len() >= crate::approval::hosts::MAX_TRUSTED_HOSTS {
                    Err(format!(
                        "more than {} trusted hosts",
                        crate::approval::hosts::MAX_TRUSTED_HOSTS
                    ))
                } else {
                    Ok(pattern)
                }
            });
            match taken {
                Ok(pattern) => self.trusted_hosts.push(TrustedHost {
                    pattern: pattern.as_str().to_string(),
                    origins: vec![host_origin],
                }),
                Err(why) => self.skipped.push(Skipped::TrustedHost {
                    origin,
                    text: raw.chars().take(80).collect(),
                    why,
                }),
            }
        }
        let statement_origin = if origin == RuleOrigin::Realm {
            StatementOrigin::Always
        } else {
            StatementOrigin::Config
        };
        for raw in &auto.allow {
            let taken = normalize(raw).and_then(|text| {
                if self
                    .statements
                    .iter()
                    .any(|(_, known)| known.eq_ignore_ascii_case(&text))
                {
                    Err(StatementRefusal::Duplicate)
                } else if self.statements.len() >= MAX_STATEMENTS {
                    Err(StatementRefusal::TooMany)
                } else {
                    Ok(text)
                }
            });
            match taken {
                Ok(text) => self.statements.push((statement_origin, text)),
                Err(StatementRefusal::Duplicate) => {}
                Err(refusal) => self.skipped.push(Skipped::Statement {
                    origin,
                    text: raw.chars().take(80).collect(),
                    refusal,
                }),
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn file(text: &str) -> RulesFile {
        toml::from_str(text).unwrap()
    }

    /// Rules, statements and directories accumulate over the layers; a
    /// rule that does not parse and a statement refused are skipped and
    /// named with their layer, the entries beside them taken; a statement
    /// repeated in a later layer is the earlier one's.
    #[test]
    fn the_layers_give_their_permissions_and_name_what_they_skip() {
        let user = file(
            "[auto]\nallow = [\"pushing to the project's own remote\", \"anything\"]\n[[allow]]\ntool = \"shell\"\narg = \"make lint\"\n[[deny]]\ntool = \"[\"\n[confinement]\nextra_write = [\"/data/out\"]\n",
        );
        let realm = file(
            "[auto]\nallow = [\"Pushing to the project's own remote\", \"network requests to api.example.com\"]\n[[deny]]\ntool = \"shell\"\narg = \"git push*\"\n[confinement]\nextra_read = [\"/data/in\"]\n",
        );
        let lasting = Lasting::of(&[(RuleOrigin::Global, &user), (RuleOrigin::Realm, &realm)]);
        let rules: Vec<(RuleKind, RuleOrigin)> = lasting
            .rules
            .iter()
            .map(|rule| (rule.kind, rule.origin))
            .collect();
        assert_eq!(
            rules,
            vec![
                (RuleKind::Allow, RuleOrigin::Global),
                (RuleKind::Deny, RuleOrigin::Realm)
            ]
        );
        assert_eq!(
            lasting.statements,
            vec![
                (
                    StatementOrigin::Config,
                    "pushing to the project's own remote".to_string()
                ),
                (
                    StatementOrigin::Always,
                    "network requests to api.example.com".to_string()
                ),
            ]
        );
        assert_eq!(
            lasting.confinement.extra_write,
            vec![std::path::PathBuf::from("/data/out")]
        );
        assert_eq!(
            lasting.confinement.extra_read,
            vec![std::path::PathBuf::from("/data/in")]
        );
        assert_eq!(lasting.skipped.len(), 2, "{:?}", lasting.skipped);
        assert!(
            lasting
                .skipped
                .iter()
                .all(|skipped| skipped.origin() == RuleOrigin::Global)
        );
        let words: Vec<String> = lasting.skipped.iter().map(Skipped::describe).collect();
        assert!(
            words[0].starts_with("a rule of your tui.toml (invalid pattern"),
            "{words:?}"
        );
        assert!(
            words[1].starts_with(
                "the statement \"anything\" of your tui.toml (the statement allows anything"
            ),
            "{words:?}"
        );
    }

    /// Trusted hosts accumulate over the user's layers, normalized and
    /// none twice; one that is no host is skipped, named with its layer.
    #[test]
    fn trusted_hosts_accumulate_checked() {
        let user = file("[auto]\ntrusted_hosts = [\"GitLab.Example.com\", \"*.com\"]\n");
        let realm = file("[auto]\ntrusted_hosts = [\"gitlab.example.com\", \"*.corp.example\"]\n");
        let lasting = Lasting::of(&[(RuleOrigin::Global, &user), (RuleOrigin::Realm, &realm)]);
        assert_eq!(
            lasting.trusted_hosts,
            vec![
                TrustedHost {
                    pattern: "gitlab.example.com".to_string(),
                    origins: vec![HostOrigin::User, HostOrigin::Workspace],
                },
                TrustedHost {
                    pattern: "*.corp.example".to_string(),
                    origins: vec![HostOrigin::Workspace],
                },
            ]
        );
        let words: Vec<String> = lasting.skipped.iter().map(Skipped::describe).collect();
        assert_eq!(words.len(), 1, "{words:?}");
        assert!(
            words[0].starts_with("the trusted host \"*.com\" of your tui.toml"),
            "{words:?}"
        );
    }

    /// At most [`MAX_STATEMENTS`] are taken; the rest are skipped.
    #[test]
    fn statements_past_the_maximum_are_skipped() {
        let many: Vec<String> = (0..=MAX_STATEMENTS)
            .map(|n| format!("\"network requests to host{n}.example.com\""))
            .collect();
        let user = file(&format!("[auto]\nallow = [{}]\n", many.join(", ")));
        let lasting = Lasting::of(&[(RuleOrigin::Global, &user)]);
        assert_eq!(lasting.statements.len(), MAX_STATEMENTS);
        assert!(matches!(
            lasting.skipped.as_slice(),
            [Skipped::Statement {
                refusal: StatementRefusal::TooMany,
                ..
            }]
        ));
    }
}
