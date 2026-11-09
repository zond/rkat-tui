//! `/status`: one notice with what this process runs on.

use std::fmt;
use std::path::PathBuf;

use super::Runner;
use crate::approval::mode::PermissionMode;
use crate::approval::rules::RULES_FILE_NAME;
use crate::approval::trust::TrustStatus;
use crate::ui::NoticeLevel;

/// Everything `/status` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StatusReport {
    pub(super) version: &'static str,
    pub(super) model: String,
    pub(super) endpoint: Option<String>,
    pub(super) session: String,
    pub(super) workspace: PathBuf,
    pub(super) realm: PathBuf,
    pub(super) realm_id: String,
    pub(super) confinement: String,
    pub(super) auto: String,
    pub(super) realm_rules: (PathBuf, String),
    /// Each project input (`/trust`) and its trust in words.
    pub(super) project_inputs: Vec<(PathBuf, String)>,
    /// `None`: this build hosts no helpers.
    pub(super) helper_depth: Option<u8>,
}

/// A trust state in words.
pub(super) fn trust_words(status: &TrustStatus) -> String {
    match status {
        TrustStatus::Missing => "absent".to_string(),
        TrustStatus::Trusted => "trusted".to_string(),
        TrustStatus::Untrusted(why) => format!("not trusted: {why}"),
    }
}

impl fmt::Display for StatusReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "rkat-tui {}", self.version)?;
        match &self.endpoint {
            Some(endpoint) => writeln!(f, "model: {} on endpoint {endpoint}", self.model)?,
            None => writeln!(
                f,
                "model: {} (no endpoint of this process serves its binding)",
                self.model
            )?,
        }
        writeln!(f, "session: {}", self.session)?;
        writeln!(f, "workspace: {}", self.workspace.display())?;
        writeln!(f, "realm: {} (id {})", self.realm.display(), self.realm_id)?;
        writeln!(f, "shell confinement: {}", self.confinement)?;
        writeln!(f, "permission mode: {}", self.auto)?;
        writeln!(
            f,
            "realm {}: {}",
            self.realm_rules.0.display(),
            self.realm_rules.1
        )?;
        for (file, trust) in &self.project_inputs {
            writeln!(f, "project {}: {trust}", file.display())?;
        }
        match self.helper_depth {
            None => f.write_str("helpers: not built in"),
            Some(0) => f.write_str("helpers: depth limit 0, every helper is refused"),
            Some(1) => f.write_str(
                "helpers: depth limit on (max_depth 1: the session's helpers start none of their own)",
            ),
            Some(n) => write!(f, "helpers: depth limit on (max_depth {n})"),
        }
    }
}

impl Runner {
    /// [`crate::ui::Effect::StatusReport`].
    pub(super) fn status_report(&self) {
        let scope = &self.runtime.scope;
        let (mode, paused, (c, cm, t, tm)) = super::approval::mode_status(&self.approval.engine);
        let realm = scope.realm_root();
        let rules = realm.join(RULES_FILE_NAME);
        let rules_trust = trust_words(&self.runtime.approval.trust.lock().check(&rules));
        let inputs = &self.runtime.project_inputs;
        let project_inputs = inputs
            .files()
            .map(|file| {
                let mut words = trust_words(&inputs.status(file));
                if inputs.by_flag() && words.starts_with("not trusted") {
                    words.push_str(" (trusted for this run by --trust-project)");
                }
                (file.to_path_buf(), words)
            })
            .collect();
        let confinement = clap::ValueEnum::to_possible_value(&self.runtime.confinement.mode)
            .map_or_else(
                || format!("{:?}", self.runtime.confinement.mode),
                |v| v.get_name().to_string(),
            );
        let report = StatusReport {
            version: env!("CARGO_PKG_VERSION"),
            model: format!(
                "{} ({})",
                self.identity.model,
                self.identity.provider.as_str()
            ),
            endpoint: self.current_endpoint.as_ref().map(ToString::to_string),
            session: self.session.id().to_string(),
            workspace: scope.context_root.clone(),
            realm,
            realm_id: scope.realm_id.clone(),
            confinement,
            auto: match mode {
                PermissionMode::Auto => format!(
                    "auto{} (consecutive denials {c}/{cm}, total {t}/{tm})",
                    if paused { ", PAUSED" } else { "" }
                ),
                mode => mode.label().to_string(),
            },
            realm_rules: (rules, rules_trust),
            project_inputs,
            helper_depth: self
                .runtime
                .helpers
                .is_some()
                .then_some(self.runtime.helper_max_depth),
        };
        self.notice(NoticeLevel::Info, report.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::trust::Untrusted;

    fn report() -> StatusReport {
        StatusReport {
            version: "0.9.10",
            model: "claude-sonnet-4-6 (anthropic)".into(),
            endpoint: Some("anthropic".into()),
            session: "0193-abcd".into(),
            workspace: PathBuf::from("/w"),
            realm: PathBuf::from("/w/.rkat/tui"),
            realm_id: "tui".into(),
            confinement: "workspace".into(),
            auto: "auto (consecutive denials 0/3, total 0/20)".into(),
            realm_rules: (PathBuf::from("/w/.rkat/tui/tui.toml"), "absent".into()),
            project_inputs: vec![
                (
                    PathBuf::from("/w/.rkat/mcp.toml"),
                    trust_words(&TrustStatus::Untrusted(Untrusted::Changed)),
                ),
                (PathBuf::from("/w/.rkat/config.toml"), "absent".into()),
            ],
            helper_depth: Some(1),
        }
    }

    #[test]
    fn the_status_report_names_everything_on_its_own_line() {
        let text = report().to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "rkat-tui 0.9.10");
        assert_eq!(
            lines[1],
            "model: claude-sonnet-4-6 (anthropic) on endpoint anthropic"
        );
        assert_eq!(lines[2], "session: 0193-abcd");
        assert_eq!(lines[3], "workspace: /w");
        assert_eq!(lines[4], "realm: /w/.rkat/tui (id tui)");
        assert_eq!(lines[5], "shell confinement: workspace");
        assert!(lines[6].starts_with("permission mode: auto"));
        assert_eq!(lines[7], "realm /w/.rkat/tui/tui.toml: absent");
        assert!(lines[8].starts_with("project /w/.rkat/mcp.toml: not trusted: it changed"));
        assert_eq!(lines[9], "project /w/.rkat/config.toml: absent");
        assert!(lines[10].starts_with("helpers: depth limit on (max_depth 1"));
        let mut off = report();
        off.helper_depth = Some(0);
        off.endpoint = None;
        let text = off.to_string();
        assert!(text.contains("no endpoint of this process serves its binding"));
        assert!(text.ends_with("helpers: depth limit 0, every helper is refused"));
        off.helper_depth = None;
        assert!(off.to_string().ends_with("helpers: not built in"));
        assert_eq!(trust_words(&TrustStatus::Trusted), "trusted");
    }
}
