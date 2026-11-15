//! The engine's lasting and session permissions as the user changes them:
//! rules added and removed, blocked calls approved, grants and statements
//! made and taken back, and the lasting permissions held again from the
//! settings files ([`ApprovalEngine::replace_lasting`]). The decision path
//! and the session state stay in [`super`]; this is another `impl` block
//! of the same engine. Each change of the engine's that a settings file
//! also takes goes in with the file's write as one ([`PermissionFiles`]).

use super::*;

/// Who holds the settings files' permission sections beside the engine
/// (the daemon's settings, `crate::daemon::settings::Settings`). The
/// engine's own change of a rule, grant or statement and its write to a
/// file go in as one under it ([`Self::writing`]), so a sync of the files,
/// and a hold of what they give, comes before both or after both: never
/// between, where it would find the engine and the files apart and undo
/// the change (then redo it, telling every client twice).
pub trait PermissionFiles: Send + Sync {
    /// Hold the files still (no sync, no hold of what they give) until the
    /// guard drops; dropping it holds the files' permission sections as
    /// written.
    fn writing(&self) -> Box<dyn Writing + '_>;
}

/// [`PermissionFiles::writing`]'s guard: its drop ends the write.
pub trait Writing {}

impl ApprovalEngine {
    /// Install who holds the settings files' sections ([`PermissionFiles`]);
    /// `false` when one was installed already.
    pub fn set_permission_files(&self, files: Arc<dyn PermissionFiles>) -> bool {
        self.permission_files.set(files).is_ok()
    }

    /// Call `hook` between a rule's push into the engine and its file's
    /// write (tests make a sync of the files arrive there); `false` when one
    /// was set already.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_between_steps(&self, hook: Arc<dyn Fn() + Send + Sync>) -> bool {
        self.between_steps.set(hook).is_ok()
    }

    /// The files held still for one change ([`PermissionFiles::writing`]),
    /// when someone holds them beside the engine.
    fn writing(&self) -> Option<Box<dyn Writing + '_>> {
        self.permission_files.get().map(|files| files.writing())
    }

    /// Add a rule, written to the file of `level` (the user's own
    /// `tui.toml` for [`Level::User`], else the realm's) when the engine
    /// has its files; it is that layer's rule from then on.
    ///
    /// # Errors
    ///
    /// Returns [`RulesError`] when the file was not written (the rule stays
    /// active).
    pub fn add_rule(&self, rule: Rule, level: Level) -> Result<(), RulesError> {
        self.audit_event("rule_added", describe_rule(&rule));
        let rule = self.of_level(rule, level);
        let _writing = self.writing();
        add_rule_as_one(
            &self.rules,
            self.files.as_ref(),
            self.between_steps.get(),
            rule,
            level,
        )
    }

    /// [`Self::add_rule`] off the async executor (the escalation's "allow
    /// and add a rule").
    pub(super) async fn add_rule_blocking(
        &self,
        rule: Rule,
        level: Level,
    ) -> Result<(), PersistError> {
        self.audit_event("rule_added", describe_rule(&rule));
        let rule = self.of_level(rule, level);
        let rules = Arc::clone(&self.rules);
        let files = self.files.clone();
        let held = self.permission_files.get().cloned();
        let between = self.between_steps.get().cloned();
        tokio::task::spawn_blocking(move || {
            let _writing = held.as_ref().map(|held| held.writing());
            add_rule_as_one(&rules, files.as_ref(), between.as_ref(), rule, level)
        })
        .await??;
        Ok(())
    }

    /// `rule` as a rule of `level`'s layer (when the engine has its files).
    fn of_level(&self, mut rule: Rule, level: Level) -> Rule {
        if self.files.is_some() {
            rule.origin = match level {
                Level::User => RuleOrigin::Global,
                _ => RuleOrigin::Realm,
            };
        }
        rule
    }

    /// Remove a rule: one of the realm or of the user's own `tui.toml`
    /// leaves its file too; one of the command line or the project's file
    /// is removed for this process only.
    ///
    /// # Errors
    ///
    /// Returns [`RulesError`] when the file was not written.
    pub fn remove_rule(&self, id: RuleId) -> Result<Option<Rule>, RulesError> {
        let _writing = self.writing();
        let removed = self.rules.write().remove(id);
        if let Some(rule) = &removed {
            self.audit_event("rule_removed", describe_rule(rule));
            let files = self.files.as_ref();
            match rule.origin {
                RuleOrigin::Realm | RuleOrigin::Session => {
                    write_rule(files, rule, Level::Workspace, false)?;
                }
                RuleOrigin::Global => write_rule(files, rule, Level::User, false)?,
                RuleOrigin::Cli | RuleOrigin::Project => {}
            }
        }
        Ok(removed)
    }

    /// The user approves blocked call `id` of `session_id` for `scope`: once
    /// (the next identical call passes), for this session (its grant joins
    /// the session's, live) or always (its grant is written through the
    /// realm's trusted writer as well). Returns what the user and the agent
    /// are told.
    ///
    /// # Errors
    ///
    /// Why nothing was granted (no such call, no grant fits, the realm file
    /// could not be written), in words for the user.
    pub fn approve(
        &self,
        session_id: &SessionId,
        id: u64,
        scope: GrantScope,
    ) -> Result<Approved, String> {
        let blocked = self.blocks.take(session_id, id, scope)?;
        self.stats
            .add(Some(session_id), |stats| stats.approvals += 1);
        let what = format!("`{} {}`", blocked.call.tool, blocked.call.summary);
        let target = match scope {
            GrantScope::Once => {
                self.blocks.approve_once(session_id, blocked.key);
                self.audit_event("approved_once", what.clone());
                let approved = Approved {
                    notice: format!("approved once: {what} runs when the agent tries it again"),
                    note: format!(
                        "The user approved your blocked call {what} once: run it again exactly as before (any other call is reviewed as usual)."
                    ),
                };
                self.emit(ApprovalEvent::Granted {
                    session_id: session_id.clone(),
                    note: approved.note.clone(),
                });
                return Ok(approved);
            }
            GrantScope::Session | GrantScope::Always | GrantScope::AlwaysUser => blocked.call.grant,
        };
        let Some(target) = target else {
            return Err(format!("no grant fits blocked call #{id}"));
        };
        self.grant(session_id, target, scope)
    }

    /// Make `target` a grant of `session_id` for `scope` (session or
    /// always), live; an "always" grant is written to the realm too.
    ///
    /// # Errors
    ///
    /// Why it was not made, for the user.
    pub fn grant(
        &self,
        session_id: &SessionId,
        target: GrantTarget,
        scope: GrantScope,
    ) -> Result<Approved, String> {
        if let GrantTarget::Dir { path } = &target
            && let Some(held) = self.settings_dir_in(path)
        {
            return Err(format!(
                "`{}` cannot be granted: it reaches {}, which holds your rkat-tui settings (a grant there would let the agent change them)",
                path.display(),
                held.display()
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        match (scope, &target) {
            (GrantScope::Once, _) => return Err("a grant is for this session or always".into()),
            (GrantScope::Session, _) => {
                self.grants.add_session(session_id, id, target.clone());
            }
            (GrantScope::Always | GrantScope::AlwaysUser, GrantTarget::Dir { path }) => {
                let dir = self.grants.always_dir(path, self.root.as_path())?;
                let _writing = self.writing();
                if let Some(files) = &self.files {
                    files
                        .of(scope.level())
                        .add_to_list("confinement.extra_write", &dir.display().to_string())
                        .map_err(|err| format!("the grant was not saved: {err}"))?;
                }
                self.grants.add_always(id, dir);
            }
            (GrantScope::Always | GrantScope::AlwaysUser, GrantTarget::Builds) => {
                return Err("builds after a dependency change are allowed for a session at a time; allow this kind of effect always with /approve's e instead".into());
            }
            (GrantScope::Always | GrantScope::AlwaysUser, GrantTarget::Rule { tool, subject }) => {
                let rule = Rule::literal(RuleKind::Allow, tool, subject, RuleOrigin::Realm)
                    .map_err(|err| format!("no rule fits: {err}"))?;
                self.add_rule(rule, scope.level())
                    .map_err(|err| format!("the rule is active but was not saved: {err}"))?;
            }
        }
        let what = target.describe();
        self.audit_event("grant_added", format!("{} {what}", scope.words()));
        // A grant may change what a cached denial would be; allows stay.
        let lasting = match (scope, &target) {
            (GrantScope::Always | GrantScope::AlwaysUser, GrantTarget::Dir { .. }) => {
                format!(" ({ALWAYS_DIR_LIMIT})")
            }
            _ => String::new(),
        };
        let approved = Approved {
            notice: format!("granted {}: {what}{lasting}", scope.words()),
            note: format!(
                "The user granted {what} {}: such calls are no longer blocked for that reason, so you need not avoid them.",
                scope.words()
            ),
        };
        self.emit(ApprovalEvent::Granted {
            session_id: session_id.clone(),
            note: approved.note.clone(),
        });
        Ok(approved)
    }

    /// The directory of a settings file (the user's own `tui.toml`, the
    /// realm's) that `dir` holds or lies in, when one does: no grant may
    /// let the agent write the files the daemon reads with the user's
    /// authority.
    fn settings_dir_in(&self, dir: &std::path::Path) -> Option<PathBuf> {
        let files = self.files.as_ref()?;
        let canonical = |path: &std::path::Path| crate::bootstrap::scope::canonical_prefix(path);
        let dir = canonical(dir);
        [files.user.path(), files.workspace.path()]
            .into_iter()
            .filter_map(std::path::Path::parent)
            .map(canonical)
            .find(|held| held.starts_with(&dir) || dir.starts_with(held))
    }

    /// Remove session grant `id` of `session_id`.
    ///
    /// # Errors
    ///
    /// When the session has no such grant.
    pub fn remove_grant(&self, session_id: &SessionId, id: u64) -> Result<String, String> {
        let target = self
            .grants
            .remove_session(session_id, id)
            .ok_or_else(|| format!("no session grant #{id}"))?;
        let what = target.describe();
        self.audit_event("grant_removed", format!("for this session {what}"));
        Ok(format!("removed the session grant of {what}"))
    }

    /// The user allows the kind of action `raw` describes, for `scope`
    /// (this session, its helpers and forks; or always, written to the
    /// realm's `tui.toml` too). The classifier reads it from the next
    /// review on; the agent is told in one system note.
    ///
    /// # Errors
    ///
    /// Why it was not added, for the user: refused by
    /// [`super::super::statements::normalize`], [`MAX_STATEMENTS`] active, the same
    /// one active, "once" asked, or the realm file not written.
    pub fn allow_statement(
        &self,
        session_id: &SessionId,
        raw: &str,
        scope: GrantScope,
    ) -> Result<Approved, String> {
        let text = super::super::statements::normalize(raw).map_err(|why| why.to_string())?;
        let active = self.statements_for(session_id);
        if active.iter().any(|known| known.eq_ignore_ascii_case(&text)) {
            return Err(StatementRefusal::Duplicate.to_string());
        }
        if self.statements.rows(Some(session_id)).len() >= MAX_STATEMENTS {
            return Err(StatementRefusal::TooMany.to_string());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        match scope {
            GrantScope::Once => {
                return Err(
                    "a kind of action is allowed for this session or always, not once".into(),
                );
            }
            GrantScope::Session => self.statements.add_session(session_id, id, text.clone()),
            GrantScope::Always | GrantScope::AlwaysUser => {
                let _writing = self.writing();
                if let Some(files) = &self.files {
                    files
                        .of(scope.level())
                        .add_to_list("auto.allow", &text)
                        .map_err(|err| format!("the statement was not saved: {err}"))?;
                }
                self.statements.add_lasting(StatementRow {
                    id,
                    text: text.clone(),
                    origin: if scope == GrantScope::AlwaysUser {
                        StatementOrigin::Config
                    } else {
                        StatementOrigin::Always
                    },
                });
            }
        }
        self.audit_event("statement_added", format!("{} {text:?}", scope.words()));
        let approved = Approved {
            notice: format!(
                "allowed {}: \"{text}\" (judged by the classifier, read narrowly)",
                scope.words()
            ),
            note: format!(
                "The user now allows: \"{text}\" ({}). The safety review reads it narrowly: actions of that kind are no longer blocked for that reason; other actions are reviewed as before.",
                scope.words()
            ),
        };
        self.emit(ApprovalEvent::Granted {
            session_id: session_id.clone(),
            note: approved.note.clone(),
        });
        Ok(approved)
    }

    /// The user allows the kind of action of blocked call `id` in their
    /// words `text` (its `EFFECT:` sentence, edited) for `scope`: the call
    /// leaves the `/approve` list and the statement is added
    /// ([`Self::allow_statement`]).
    ///
    /// # Errors
    ///
    /// No such call, or why the statement was not added.
    pub fn approve_effect(
        &self,
        session_id: &SessionId,
        id: u64,
        text: &str,
        scope: GrantScope,
    ) -> Result<Approved, String> {
        if !self.blocks.contains(session_id, id) {
            return Err(format!("no blocked call #{id} (`/approve` lists them)"));
        }
        let approved = self.allow_statement(session_id, text, scope)?;
        self.blocks.drop_call(session_id, id);
        self.stats
            .add(Some(session_id), |stats| stats.approvals += 1);
        Ok(approved)
    }

    /// Remove statement `id`: a session's, or one added always (it leaves
    /// the realm's `tui.toml` too). One from the user's own `tui.toml` is
    /// removed by editing that file. Cached allows are dropped: one may
    /// have rested on it.
    ///
    /// # Errors
    ///
    /// No such statement, one of the user's own files, or the realm file
    /// not written.
    pub fn remove_statement(&self, session_id: &SessionId, id: u64) -> Result<String, String> {
        let removed = self.statements.remove_session(session_id, id);
        let (text, origin) = if let Some(text) = removed {
            (text, StatementOrigin::Session)
        } else {
            let _writing = self.writing();
            let row = self
                .statements
                .lasting(id)
                .ok_or_else(|| format!("no statement #{id}"))?;
            if self.files.is_none() && row.origin == StatementOrigin::Config {
                return Err(format!(
                    "\"{}\" comes from your own tui.toml ([auto] allow): remove it there",
                    row.text
                ));
            }
            if let Some(files) = &self.files {
                let level = match row.origin {
                    StatementOrigin::Config => Level::User,
                    _ => Level::Workspace,
                };
                let removed = files
                    .of(level)
                    .remove_from_list("auto.allow", &row.text)
                    .map_err(|err| format!("the statement was not removed: {err}"))?;
                if !removed && row.origin == StatementOrigin::Config {
                    return Err(format!(
                        "\"{}\" comes from the daemon's command line (--set auto.allow): it ends when the daemon stops",
                        row.text
                    ));
                }
            }
            self.statements.remove_lasting(id);
            (row.text, row.origin)
        };
        for state in self.sessions.lock().values_mut() {
            state.allow_cache.clear();
        }
        self.audit_event("statement_removed", format!("{} {text:?}", origin.word()));
        Ok(format!("no longer allowed: \"{text}\""))
    }

    /// Hold the lasting permissions `lasting` (what the user's own layers
    /// give now, [`crate::config::lasting::Lasting::of`], as at start): the
    /// rules of the user's and the workspace's files, the statements and
    /// the named directories become theirs. The command line's rules, the
    /// project's (taken at start) and every session's grants and
    /// statements stay. An entry is refused as at start (a broad allow
    /// rule, a directory the start's plan would refuse) and, for a
    /// directory not held before, as a grant by command is (one that
    /// reaches the settings files, or [`super::super::grants::grant_dir_for`]
    /// refuses); a refused one is skipped and named. Each item added or
    /// removed is audited with `why` ("your tui.toml changed"); a removal
    /// drops the cached allows (one may have rested on it).
    pub fn replace_lasting(
        &self,
        lasting: &crate::config::lasting::Lasting,
        why: &str,
    ) -> LastingChange {
        let mut change = LastingChange {
            skipped: lasting.skipped.iter().map(|s| s.describe()).collect(),
            ..LastingChange::default()
        };
        // Rules: as at start, a broad allow rule is dropped.
        let rules: Vec<Rule> = lasting
            .rules
            .iter()
            .filter(|rule| {
                let broad = (rule.kind == RuleKind::Allow)
                    .then(|| rule.broad_reason())
                    .flatten();
                if let Some(reason) = broad {
                    change
                        .skipped
                        .push(format!("the rule allow {} ({reason})", describe_rule(rule)));
                }
                broad.is_none()
            })
            .cloned()
            .collect();
        let (added, removed) = self
            .rules
            .write()
            .replace(&[RuleOrigin::Global, RuleOrigin::Realm], rules);
        change.rules_added = added
            .iter()
            .map(super::super::desk::describe_rule)
            .collect();
        change.rules_removed = removed
            .iter()
            .map(super::super::desk::describe_rule)
            .collect();
        // Statements.
        let (added, removed) = self.statements.replace(&lasting.statements, || {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        });
        change.statements_added = added;
        change.statements_removed = removed;
        // Directories: checked as the start's plan checks them.
        let (write, read) = self.grants.flag_dirs();
        let extra = crate::bootstrap::confinement::ExtraGrants {
            read,
            write,
            file: lasting.confinement.clone(),
        };
        let (sandbox_write, sandbox_read) = self.grants.sandbox(self.root.as_path());
        let (named, refused) = crate::bootstrap::confinement::named_dirs(
            self.root.as_path(),
            &sandbox_write,
            &sandbox_read,
            &extra,
        );
        change
            .skipped
            .extend(refused.iter().map(|err| format!("the directory {err}")));
        let mut dirs = Vec::new();
        for grant in named {
            let new = !grant.from_cli && !self.grants.is_named(&grant.path, grant.writable);
            if new {
                if let Some(held) = self.settings_dir_in(&grant.path) {
                    change.skipped.push(format!(
                        "the directory {}: it reaches {}, which holds your rkat-tui settings",
                        grant.path.display(),
                        held.display()
                    ));
                    continue;
                }
                // An existing directory (the plan's checks): its own grant.
                if let Err(refusal) = super::super::grants::grant_dir_for(
                    &self.root,
                    &grant.path.display().to_string(),
                ) {
                    change
                        .skipped
                        .push(format!("the directory {}: {refusal}", grant.path.display()));
                    continue;
                }
            }
            let origin = if grant.from_cli {
                super::super::grants::GrantOrigin::Flag
            } else {
                super::super::grants::GrantOrigin::Config
            };
            dirs.push((grant.path, grant.writable, origin));
        }
        let (added, removed) = self.grants.replace_named(dirs);
        change.dirs_added = added;
        change.dirs_removed = removed;
        for (event, items) in [
            ("rule_added", &change.rules_added),
            ("rule_removed", &change.rules_removed),
            ("statement_added", &change.statements_added),
            ("statement_removed", &change.statements_removed),
        ] {
            for item in items {
                self.audit_event(event, format!("{item} ({why})"));
            }
        }
        for (event, dirs) in [
            ("grant_added", &change.dirs_added),
            ("grant_removed", &change.dirs_removed),
        ] {
            for dir in dirs {
                let access = if dir.write { "writes" } else { "reads" };
                self.audit_event(
                    event,
                    format!("{access} under `{}` ({why})", dir.path.display()),
                );
            }
        }
        if change.removed() {
            for state in self.sessions.lock().values_mut() {
                state.allow_cache.clear();
            }
        }
        change
    }
}

/// Push `rule` into `rules`, then write it to `level`'s file (the caller
/// holds the files still: [`PermissionFiles::writing`]); `between` runs
/// between the two steps.
fn add_rule_as_one(
    rules: &RwLock<RuleSet>,
    files: Option<&config::SettingsFiles>,
    between: Option<&Arc<dyn Fn() + Send + Sync>>,
    rule: Rule,
    level: Level,
) -> Result<(), RulesError> {
    rules.write().push(rule.clone());
    if let Some(between) = between {
        between();
    }
    write_rule(files, &rule, level, true)
}

/// Add `rule` to (or remove it from, `!add`) the file of `level`, when the
/// engine has its files.
fn write_rule(
    files: Option<&config::SettingsFiles>,
    rule: &Rule,
    level: Level,
    add: bool,
) -> Result<(), RulesError> {
    let Some(files) = files else {
        return Ok(());
    };
    let list = match rule.kind {
        RuleKind::Allow => "allow",
        RuleKind::Deny => "deny",
    };
    let file = files.of(level);
    let entry = rule.to_entry();
    if add {
        file.add_rule(list, &entry)?;
    } else {
        file.remove_rule(list, &entry)?;
    }
    Ok(())
}
