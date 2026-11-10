#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::approval::mode::PermissionMode;
use crate::approval::rules::{ArgPattern, Rule, RuleKind, RuleOrigin, RuleSet, RulesError};
use crate::bootstrap::confinement::ConfinementSection;
use crate::bootstrap::trust::Untrusted;
use crate::thinking::ThinkingSetting;
use crate::ui::checklist::ChecklistView;
use crate::ui::compaction::CompactMode;
use crate::ui::transcript::HelperView;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn allow(tool: &str, arg: Option<&str>) -> Rule {
    Rule::new(RuleKind::Allow, tool, arg, RuleOrigin::Realm).unwrap()
}

#[test]
fn rules_file_round_trip() {
    let file = RulesFile {
        daemon: Some(DaemonSection {
            principal: Some("alice".into()),
        }),
        attach: Some(true),
        auto: Some(AutoSection {
            enabled: Some(true),
            classifier_model: Some("claude-sonnet-4-6".into()),
            trust_project_builds: Some(false),
            ..AutoSection::default()
        }),
        allow: vec![
            RuleEntry {
                tool: "shell".into(),
                arg: Some("cargo nextest run*".into()),
                literal: None,
            },
            RuleEntry {
                tool: "shell".into(),
                arg: None,
                literal: Some("make lint [*]".into()),
            },
        ],
        deny: vec![RuleEntry {
            tool: "mcp__prod__*".into(),
            arg: None,
            literal: None,
        }],
        endpoints: BTreeMap::from([(
            "litellm".to_string(),
            EndpointSection {
                dialect: Some("openai".into()),
                base_url: Some("http://localhost:4000".into()),
                api_key_env: Some("LITELLM_KEY".into()),
                api_key: None,
                priority: Some(2),
            },
        )]),
        pricing: PricingTable {
            fetch: None,
            models: BTreeMap::from([(
                "gpt-5.4".to_string(),
                PricingSection {
                    input: Some(1.25),
                    output: Some(10.0),
                    cached_input: None,
                    cache_write: None,
                },
            )]),
        },
        thinking: BTreeMap::from([
            ("litellm:claude-x".to_string(), ThinkingSetting::Adaptive),
            (
                "claude-sonnet-4-5".to_string(),
                ThinkingSetting::Enabled {
                    budget_tokens: 8000,
                },
            ),
        ]),
        confinement: Some(ConfinementSection {
            extra_read: vec![PathBuf::from("/data")],
            extra_write: vec![PathBuf::from("/scratch")],
        }),
        helpers: Some(HelpersSection { max_depth: Some(2) }),
        shell: Some(ShellSection {
            user_timeout_secs: Some(300),
        }),
        ui: Some(UiSection {
            helpers: Some(HelperView::Expanded),
            bell: Some(false),
            checklist: Some(ChecklistView::Shown),
        }),
    };
    let text = toml::to_string_pretty(&file).unwrap();
    let back: RulesFile = toml::from_str(&text).unwrap();
    assert_eq!(back, file);
    let parsed: RulesFile = toml::from_str(
        "[auto]\nenabled = true\n\n[[allow]]\ntool = \"shell\"\narg = \"cargo nextest run*\"\n",
    )
    .unwrap();
    assert_eq!(parsed.allow.len(), 1);
    assert!(toml::from_str::<RulesFile>("[[allow]]\ntool=\"x\"\nbogus=1\n").is_err());
    let rules = file.rules(RuleOrigin::Realm).unwrap();
    assert_eq!(
        rules[1].arg,
        Some(ArgPattern::Literal("make lint [*]".into()))
    );
    assert_eq!(rules[1].to_entry(), file.allow[1]);
    let both: RulesFile =
        toml::from_str("[[allow]]\ntool=\"x\"\narg=\"a\"\nliteral=\"b\"\n").unwrap();
    assert!(matches!(
        both.rules(RuleOrigin::Realm),
        Err(RulesError::ArgAndLiteral { .. })
    ));
}

#[test]
fn layered_loading() {
    let tmp = tempfile::tempdir().unwrap();
    let realm = tmp.path().join("realm");
    let home = tmp.path().join("home");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&realm).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        home.join(RULES_FILE_NAME),
        "[auto]\ntrust_project_builds = false\n[[deny]]\ntool = \"shell\"\narg = \"git push*\"\n",
    )
    .unwrap();
    std::fs::write(
        realm.join(RULES_FILE_NAME),
        "[auto]\nclassifier_model = \"gpt-5.2\"\n[[allow]]\ntool = \"shell\"\narg = \"make lint\"\n",
    )
    .unwrap();
    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ntrust_project_builds = true\nenabled = false\n[[allow]]\ntool = \"shell\"\n[[deny]]\ntool = \"agent_spawn\"\n",
    )
    .unwrap();
    let (rules, settings, notices) = load_layered(
        &realm,
        Some(&home.join(RULES_FILE_NAME)),
        &project,
        RealmTrust::Owned,
    )
    .unwrap();
    assert_eq!(rules.allow().len(), 1);
    assert_eq!(rules.allow().next().unwrap().1.origin, RuleOrigin::Realm);
    assert_eq!(rules.deny().len(), 2);
    assert!(!settings.trust_project_builds);
    assert_eq!(settings.classifier_model.as_deref(), Some("gpt-5.2"));
    assert_eq!(notices.len(), 2);
    assert!(
        notices
            .iter()
            .any(|n| matches!(n, Notice::ProjectAllowIgnored { count: 1, .. }))
    );
    // The project's `enabled = false` turns the classifier off.
    let layered = load_layered_with(
        &realm,
        Some(&home.join(RULES_FILE_NAME)),
        &project,
        RealmTrust::Owned,
        None,
    )
    .unwrap();
    assert_eq!(layered.mode, PermissionMode::Manual);
}

#[test]
fn global_rules_come_from_the_file_path() {
    let tmp = tempfile::tempdir().unwrap();
    let global = tmp.path().join("cfg").join("custom.toml");
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(
        &global,
        "[[allow]]\ntool = \"shell\"\narg = \"make test\"\n[[deny]]\ntool = \"shell\"\narg = \"rm *\"\n",
    )
    .unwrap();
    let (rules, _, notices) = load_layered(
        &tmp.path().join("realm"),
        Some(&global),
        tmp.path(),
        RealmTrust::Owned,
    )
    .unwrap();
    assert_eq!(rules.allow().len(), 1);
    assert_eq!(rules.allow().next().unwrap().1.origin, RuleOrigin::Global);
    assert_eq!(rules.deny().len(), 1);
    assert_eq!(rules.deny().next().unwrap().1.origin, RuleOrigin::Global);
    assert!(notices.is_empty());
    // A missing global file is not an error.
    let (rules, _, _) = load_layered(
        &tmp.path().join("realm"),
        Some(&tmp.path().join("missing.toml")),
        tmp.path(),
        RealmTrust::Owned,
    )
    .unwrap();
    assert_eq!(rules, RuleSet::default());
}

#[test]
fn legacy_global_rules_notice_only_when_old_file_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let new_path = tmp.path().join(".config/rkat-tui/tui.toml");
    assert_eq!(
        legacy_global_rules_notice(Some(tmp.path()), Some(&new_path)),
        None
    );
    let old = tmp.path().join(".rkat").join(RULES_FILE_NAME);
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, "").unwrap();
    let notice = legacy_global_rules_notice(Some(tmp.path()), Some(&new_path)).unwrap();
    assert_eq!(
        notice,
        Notice::LegacyGlobalRulesIgnored {
            path: old.clone(),
            new_path: new_path.clone()
        }
    );
    assert_eq!(
        notice.to_string(),
        format!(
            "{} is no longer read; move it to {}",
            old.display(),
            new_path.display()
        )
    );
    assert_eq!(legacy_global_rules_notice(None, Some(&new_path)), None);
    assert_eq!(legacy_global_rules_notice(Some(tmp.path()), None), None);
}

#[test]
fn missing_files_give_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let (rules, settings, notices) = load_layered(
        tmp.path(),
        None,
        &tmp.path().join("nope"),
        RealmTrust::Owned,
    )
    .unwrap();
    assert_eq!(rules, RuleSet::default());
    assert_eq!(settings, AutoSettings::default());
    assert!(notices.is_empty());
}

#[test]
fn invalid_file_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join(RULES_FILE_NAME),
        "[[allow]]\ntool = \"[\"\n",
    )
    .unwrap();
    assert!(matches!(
        load_layered(tmp.path(), None, tmp.path(), RealmTrust::Owned),
        Err(RulesError::Pattern { .. })
    ));
    std::fs::write(tmp.path().join(RULES_FILE_NAME), "not toml [").unwrap();
    assert!(matches!(
        load_layered(tmp.path(), None, tmp.path(), RealmTrust::Owned),
        Err(RulesError::Parse { .. })
    ));
}

#[test]
fn save_and_persist_preserve_auto_section() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("deep/realm").join(RULES_FILE_NAME);
    save_realm(
        &path,
        &RulesFile {
            auto: Some(AutoSection {
                enabled: Some(false),
                ..AutoSection::default()
            }),
            ..RulesFile::default()
        },
    )
    .unwrap();
    let mut set = RuleSet::default();
    set.push(allow("shell", Some("make lint")));
    set.push(Rule::new(RuleKind::Deny, "shell", Some("rm *"), RuleOrigin::Session).unwrap());
    set.push(Rule::new(RuleKind::Deny, "shell", Some("x"), RuleOrigin::Project).unwrap());
    persist_realm_rules(&path, &set).unwrap();
    let back = RulesFile::load(&path).unwrap();
    assert_eq!(back.auto.unwrap().enabled, Some(false));
    assert_eq!(back.allow.len(), 1);
    assert_eq!(
        back.deny.len(),
        1,
        "project rules are not copied into the realm file"
    );
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn compaction_settings_default_layer_and_clamp() {
    let defaults = AutoSettings::default();
    assert_eq!(defaults.compact, CompactMode::Native);
    assert_eq!(
        (defaults.compact_warn_percent, defaults.compact_auto_percent),
        (75, 85)
    );
    let tmp = tempfile::tempdir().unwrap();
    let global = tmp.path().join("global.toml");
    let realm = tmp.path().join("realm");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&realm).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let (_, settings, notices) =
        load_layered(&realm, Some(&global), &project, RealmTrust::Owned).unwrap();
    assert_eq!(settings, defaults);
    assert!(notices.is_empty());

    std::fs::write(
        &global,
        "[auto]\ncompact = \"auto\"\ncompact_warn_percent = 60\ncompact_auto_percent = 70\n",
    )
    .unwrap();
    std::fs::write(
        realm.join(RULES_FILE_NAME),
        "[auto]\ncompact_auto_percent = 90\n",
    )
    .unwrap();
    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ncompact = \"warn\"\n",
    )
    .unwrap();
    let (_, settings, notices) =
        load_layered(&realm, Some(&global), &project, RealmTrust::Owned).unwrap();
    assert!(notices.is_empty(), "{notices:?}");
    assert_eq!(
        settings.compact,
        CompactMode::Warn,
        "the project layer may set it"
    );
    assert_eq!(
        (settings.compact_warn_percent, settings.compact_auto_percent),
        (60, 90)
    );
    let policy = settings.compaction_policy(None);
    assert_eq!(policy.mode, CompactMode::Warn);
    assert_eq!((policy.warn_percent, policy.auto_percent), (60, 90));
    assert_eq!(
        settings.compaction_policy(Some(CompactMode::Auto)).mode,
        CompactMode::Auto,
        "--auto-compact overrides the file"
    );

    // auto below warn is clamped up, with a notice.
    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ncompact_warn_percent = 95\ncompact_auto_percent = 80\n",
    )
    .unwrap();
    let (_, settings, notices) =
        load_layered(&realm, Some(&global), &project, RealmTrust::Owned).unwrap();
    assert_eq!(
        (settings.compact_warn_percent, settings.compact_auto_percent),
        (95, 95)
    );
    assert_eq!(
        notices,
        vec![Notice::CompactPercentClamped {
            configured: (95, 80),
            applied: (95, 95),
        }]
    );

    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ncompact = \"always\"\n",
    )
    .unwrap();
    assert!(load_layered(&realm, Some(&global), &project, RealmTrust::Owned).is_err());
    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ncompact_warn_percent = 300\n",
    )
    .unwrap();
    assert!(
        load_layered(&realm, Some(&global), &project, RealmTrust::Owned).is_err(),
        "u8 overflow is a parse error"
    );
}

#[test]
fn trust_defaults_off_and_project_can_only_lower_it() {
    assert!(!AutoSettings::default().trust_project_builds);
    let tmp = tempfile::tempdir().unwrap();
    let realm = tmp.path().join("realm");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&realm).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ntrust_project_builds = true\n",
    )
    .unwrap();
    let (_, settings, notices) = load_layered(&realm, None, &project, RealmTrust::Owned).unwrap();
    assert!(!settings.trust_project_builds);
    assert!(matches!(notices[..], [Notice::ProjectTrustIgnored { .. }]));

    std::fs::write(
        realm.join(RULES_FILE_NAME),
        "[auto]\ntrust_project_builds = true\n",
    )
    .unwrap();
    std::fs::write(
        project.join(RULES_FILE_NAME),
        "[auto]\ntrust_project_builds = false\n",
    )
    .unwrap();
    let (_, settings, _) = load_layered(&realm, None, &project, RealmTrust::Owned).unwrap();
    assert!(!settings.trust_project_builds, "project lowers trust");
    std::fs::remove_file(project.join(RULES_FILE_NAME)).unwrap();
    let (_, settings, _) = load_layered(&realm, None, &project, RealmTrust::Owned).unwrap();
    assert!(
        settings.trust_project_builds,
        "the user's realm may raise it"
    );
}

/// A4 of the security review: a project file cannot pick the classifier's
/// model; the user's layers keep theirs and the default stays the default.
#[test]
fn a_project_file_cannot_pick_the_classifier_model() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "");
    write(&project, "[auto]\nclassifier_model = \"tiny-model\"\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.settings.classifier_model, None);
    assert!(
        matches!(&layered.notices[..], [Notice::ProjectClassifierModelIgnored { model, .. }] if model == "tiny-model"),
        "{:?}",
        layered.notices
    );
    assert!(layered.notices[0].to_string().contains("classifier_model"));
    write(&realm, "[auto]\nclassifier_model = \"realm-model\"\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(
        layered.settings.classifier_model.as_deref(),
        Some("realm-model")
    );
}

/// E3 of the security review: `[pricing] fetch = false` keeps the
/// community price list from being fetched; a project file may turn the
/// fetch off, never back on.
#[test]
fn pricing_fetch_is_a_user_setting_the_project_can_only_turn_off() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert!(layered.pricing_fetch, "fetched by default");
    write(
        &realm,
        "[pricing]\nfetch = false\n[pricing.\"m\"]\ninput = 1.0\n",
    );
    write(&project, "[pricing]\nfetch = true\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert!(!layered.pricing_fetch, "the project cannot turn it back on");
    assert_eq!(layered.pricing["m"].input, Some(1.0));
    assert!(
        !layered
            .notices
            .iter()
            .any(|n| matches!(n, Notice::PricingIgnored { .. })),
        "{:?}",
        layered.notices
    );
    write(&realm, "");
    write(&project, "[pricing]\nfetch = false\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert!(!layered.pricing_fetch, "the project may turn it off");
}

#[test]
fn untrusted_realm_contributes_only_deny_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let realm = tmp.path().join("realm");
    std::fs::create_dir_all(&realm).unwrap();
    std::fs::write(
        realm.join(RULES_FILE_NAME),
        "[auto]\ntrust_project_builds = true\nenabled = true\n[[allow]]\ntool = \"shell\"\narg = \"make lint\"\n[[deny]]\ntool = \"shell\"\narg = \"rm *\"\n",
    )
    .unwrap();
    let (rules, settings, notices) = load_layered(
        &realm,
        None,
        &tmp.path().join("p"),
        RealmTrust::DenyOnly(Untrusted::NotRecorded),
    )
    .unwrap();
    assert_eq!(rules.allow().len(), 0);
    assert_eq!(rules.deny().len(), 1);
    assert!(!settings.trust_project_builds);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices.iter().any(|n| matches!(
        n,
        Notice::UntrustedRealmDenyOnly {
            allow: 1,
            auto: true,
            ..
        }
    )));
    assert!(notices[0].to_string().contains("/permissions trust"));
}

fn write(dir: &Path, text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(RULES_FILE_NAME), text).unwrap();
}

fn cli(text: &str) -> RulesFile {
    toml::from_str(text).unwrap()
}

#[test]
fn classifier_tools_default_on_and_project_can_only_turn_them_off() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    assert!(AutoSettings::default().classifier_tools);
    // The project may turn the classifier's file reads off, even over
    // the command line...
    write(&realm, "");
    write(&project, "[auto]\nclassifier_tools = false\n");
    let overlay = cli("[auto]\nclassifier_tools = true\n");
    let layered =
        load_layered_with(&realm, None, &project, RealmTrust::Owned, Some(&overlay)).unwrap();
    assert!(!layered.settings.classifier_tools);
    // ...but never back on once a user layer turned them off.
    write(&realm, "[auto]\nclassifier_tools = false\n");
    write(&project, "[auto]\nclassifier_tools = true\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert!(!layered.settings.classifier_tools);
    // The command line may turn them off.
    write(&realm, "");
    let overlay = cli("[auto]\nclassifier_tools = false\n");
    let layered =
        load_layered_with(&realm, None, &project, RealmTrust::Owned, Some(&overlay)).unwrap();
    assert!(!layered.settings.classifier_tools);
}

#[test]
fn helper_depth_defaults_to_one_layers_and_the_project_only_lowers_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.helpers.max_depth, DEFAULT_HELPER_MAX_DEPTH);
    assert_eq!(DEFAULT_HELPER_MAX_DEPTH, 1);
    // The realm sets it, the command line beats the realm.
    write(&realm, "[helpers]\nmax_depth = 3\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.helpers.max_depth, 3);
    let overlay = cli("[helpers]\nmax_depth = 2\n");
    let layered =
        load_layered_with(&realm, None, &project, RealmTrust::Owned, Some(&overlay)).unwrap();
    assert_eq!(layered.helpers.max_depth, 2);
    // The project may lower it, never raise it.
    write(&project, "[helpers]\nmax_depth = 1\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.helpers.max_depth, 1);
    write(&realm, "");
    write(&project, "[helpers]\nmax_depth = 5\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.helpers.max_depth, 1);
}

#[test]
fn the_user_shell_timeout_layers_and_the_project_only_lowers_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.shell.user_timeout_secs, 120);
    write(&realm, "[shell]\nuser_timeout_secs = 600\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.shell.user_timeout_secs, 600);
    write(&project, "[shell]\nuser_timeout_secs = 30\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.shell.user_timeout_secs, 30);
    write(&project, "[shell]\nuser_timeout_secs = 6000\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.shell.user_timeout_secs, 600);
    write(&realm, "[shell]\nuser_timeout_secs = 0\n");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.shell.user_timeout_secs, 1);
}

#[test]
fn ui_bell_defaults_to_on_and_layers() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert!(layered.ui.bell);
    write(&realm, "[ui]\nbell = false\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert!(!layered.ui.bell);
    let overlay = cli("[ui]\nbell = true\n");
    let layered =
        load_layered_with(&realm, None, &project, RealmTrust::Owned, Some(&overlay)).unwrap();
    assert!(layered.ui.bell);
}

#[test]
fn ui_helpers_defaults_to_folded_and_layers() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.ui.helpers, HelperView::Folded);
    write(&realm, "[ui]\nhelpers = \"expanded\"\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.ui.helpers, HelperView::Expanded);
    // The project's preference applies unless the command line set one.
    write(&project, "[ui]\nhelpers = \"folded\"\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.ui.helpers, HelperView::Folded);
    let overlay = cli("[ui]\nhelpers = \"expanded\"\n");
    let layered =
        load_layered_with(&realm, None, &project, RealmTrust::Owned, Some(&overlay)).unwrap();
    assert_eq!(layered.ui.helpers, HelperView::Expanded);
}

#[test]
fn ui_checklist_defaults_to_hidden_layers_and_persists() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(&realm, "[auto]\nmode = \"plan\"\n[ui]\nbell = false\n");
    write(&project, "");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.ui.checklist, ChecklistView::Hidden);
    let realm_file = realm.join(RULES_FILE_NAME);
    persist_realm_checklist(&realm_file, ChecklistView::Shown).unwrap();
    let text = std::fs::read_to_string(&realm_file).unwrap();
    assert!(text.contains("checklist = \"shown\""), "{text}");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.ui.checklist, ChecklistView::Shown);
    // The rest of the file stays.
    assert!(!layered.ui.bell);
    assert_eq!(layered.mode, PermissionMode::Plan);
    // The project's preference applies unless the command line set one.
    write(&project, "[ui]\nchecklist = \"hidden\"\n");
    let layered = load_layered_with(&realm, None, &project, RealmTrust::Owned, None).unwrap();
    assert_eq!(layered.ui.checklist, ChecklistView::Hidden);
    let overlay = cli("[ui]\nchecklist = \"shown\"\n");
    let layered =
        load_layered_with(&realm, None, &project, RealmTrust::Owned, Some(&overlay)).unwrap();
    assert_eq!(layered.ui.checklist, ChecklistView::Shown);
    write(&project, "[ui]\nchecklist = \"open\"\n");
    assert!(load_layered_with(&realm, None, &project, RealmTrust::Owned, None).is_err());
}

/// The realm's `[auto] mode` (what `/mode` records) sets the start
/// mode; a project cannot pick one, only move auto mode to manual with
/// `enabled = false`; the command line beats both.
#[test]
fn the_permission_mode_layers() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    let mode = |realm_text: &str, project_text: &str, overlay: Option<&str>| {
        write(&realm, realm_text);
        write(&project, project_text);
        let overlay = overlay.map(cli);
        load_layered_with(&realm, None, &project, RealmTrust::Owned, overlay.as_ref())
            .unwrap()
            .mode
    };
    assert_eq!(mode("", "", None), PermissionMode::Auto);
    assert_eq!(
        mode("[auto]\nmode = \"plan\"\n", "", None),
        PermissionMode::Plan
    );
    assert_eq!(
        mode(
            "[auto]\nenabled = true\nmode = \"accept-edits\"\n",
            "",
            None
        ),
        PermissionMode::AcceptEdits,
        "mode wins over enabled"
    );
    assert_eq!(
        mode("[auto]\nenabled = false\n", "", None),
        PermissionMode::Manual
    );
    assert_eq!(
        mode(
            "",
            "[auto]\nmode = \"accept-edits\"\nenabled = true\n",
            None
        ),
        PermissionMode::Auto,
        "a project picks no mode"
    );
    assert_eq!(
        mode("", "[auto]\nenabled = false\n", None),
        PermissionMode::Manual
    );
    assert_eq!(
        mode(
            "[auto]\nmode = \"plan\"\n",
            "[auto]\nenabled = false\n",
            None
        ),
        PermissionMode::Plan,
        "only auto mode moves to manual"
    );
    assert_eq!(
        mode(
            "[auto]\nmode = \"plan\"\n",
            "",
            Some("[auto]\nenabled = false\n")
        ),
        PermissionMode::Manual,
        "--no-auto beats the realm"
    );
    assert_eq!(
        mode(
            "",
            "[auto]\nenabled = false\n",
            Some("[auto]\nmode = \"auto\"\n")
        ),
        PermissionMode::Auto,
        "the command line beats the project"
    );
}

#[test]
fn cli_layer_beats_realm_but_project_deny_and_lowering_still_apply() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    write(
        &realm,
        "[auto]\nenabled = true\ncompact = \"warn\"\nclassifier_model = \"realm-model\"\n[[allow]]\ntool = \"read_file\"\n",
    );
    write(
        &project,
        "[auto]\ncompact = \"warn\"\ncompact_warn_percent = 50\ntrust_project_builds = false\n[[deny]]\ntool = \"shell\"\narg = \"git push*\"\n",
    );
    let overlay = cli(
        "[auto]\nenabled = false\ncompact = \"auto\"\ntrust_project_builds = true\n[[allow]]\ntool = \"shell\"\narg = \"cargo nextest run*\"\n[[deny]]\ntool = \"shell\"\narg = \"git push*\"\n",
    );
    let layered =
        load_layered_with(&realm, None, &project, RealmTrust::Owned, Some(&overlay)).unwrap();
    let s = &layered.settings;
    assert_eq!(layered.mode, PermissionMode::Manual, "cli beats realm");
    assert_eq!(s.compact, CompactMode::Auto, "cli beats project");
    assert_eq!(s.classifier_model.as_deref(), Some("realm-model"));
    assert_eq!(
        s.compact_warn_percent, 50,
        "project fills what cli left unset"
    );
    assert!(!s.trust_project_builds, "project may lower trust");
    let allow: Vec<(RuleOrigin, &str)> = layered
        .rules
        .allow()
        .map(|(_, r)| (r.origin, r.tool.as_str()))
        .collect();
    assert_eq!(
        allow,
        vec![(RuleOrigin::Realm, "read_file"), (RuleOrigin::Cli, "shell")]
    );
    // The project's deny duplicates the cli's: one rule, the cli's.
    assert_eq!(layered.rules.deny().len(), 1);
    assert!(
        layered
            .rules
            .matches_deny("shell", &["git push origin".to_string()])
            .is_some()
    );
    // Without the cli deny, the project's still applies.
    let layered = load_layered_with(
        &realm,
        None,
        &project,
        RealmTrust::Owned,
        Some(&cli("[[allow]]\ntool = \"shell\"\narg = \"git push*\"\n")),
    )
    .unwrap();
    assert_eq!(
        layered.rules.deny().next().unwrap().1.origin,
        RuleOrigin::Project
    );
    assert!(
        layered
            .rules
            .matches_deny("shell", &["git push".to_string()])
            .is_some()
    );
}

#[test]
fn pricing_layers_merge_by_field_and_only_user_layers_count() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    let global = tmp.path().join("global").join(RULES_FILE_NAME);
    write(
        global.parent().unwrap(),
        "[pricing.\"claude-opus-4-6\"]\ninput = 5\noutput = 25\ncached_input = 0.5\n",
    );
    write(
        &realm,
        "[pricing.\"claude-opus-4-6\"]\noutput = 30\n[pricing.bad]\ninput = -1\noutput = 1\n",
    );
    write(&project, "[pricing.\"gpt-5.4\"]\ninput = 0\noutput = 0\n");
    let overlay = cli("[pricing.\"claude-opus-4-6\"]\ninput = 15\n");
    let layered = load_layered_with(
        &realm,
        Some(&global),
        &project,
        RealmTrust::Owned,
        Some(&overlay),
    )
    .unwrap();
    assert_eq!(
        layered.pricing.get("claude-opus-4-6"),
        Some(&PricingSection {
            input: Some(15.0),
            output: Some(30.0),
            cached_input: Some(0.5),
            cache_write: None,
        })
    );
    assert!(
        !layered.pricing.contains_key("gpt-5.4"),
        "project prices ignored"
    );
    assert!(
        !layered.pricing.contains_key("bad"),
        "negative price dropped"
    );
    let texts: Vec<String> = layered.notices.iter().map(ToString::to_string).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("ignored 1 [pricing] entry")),
        "{texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("`input` must be")),
        "{texts:?}"
    );
    // An untrusted realm file's prices are ignored too.
    let layered = load_layered_with(
        &realm,
        None,
        &project,
        RealmTrust::DenyOnly(Untrusted::Changed),
        None,
    )
    .unwrap();
    assert!(layered.pricing.is_empty());
}

#[test]
fn thinking_layers_replace_by_key_and_only_user_layers_count() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    let global = tmp.path().join("global").join(RULES_FILE_NAME);
    write(
        global.parent().unwrap(),
        "[thinking]\n\"a\" = \"adaptive\"\n\"b\" = { enabled = 4096 }\n",
    );
    write(&realm, "[thinking]\n\"b\" = \"default\"\n");
    write(&project, "[thinking]\n\"c\" = \"adaptive\"\n");
    let overlay = cli("[thinking]\n\"a\" = { enabled = 2048 }\n");
    let layered = load_layered_with(
        &realm,
        Some(&global),
        &project,
        RealmTrust::Owned,
        Some(&overlay),
    )
    .unwrap();
    assert_eq!(
        layered.thinking,
        BTreeMap::from([
            (
                "a".to_string(),
                ThinkingSetting::Enabled {
                    budget_tokens: 2048
                }
            ),
            ("b".to_string(), ThinkingSetting::Default),
        ]),
        "the project's setting is ignored"
    );
    let texts: Vec<String> = layered.notices.iter().map(ToString::to_string).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("ignored 1 [thinking] setting(s)")),
        "{texts:?}"
    );
    // An untrusted realm file's settings are ignored too, and counted.
    let layered = load_layered_with(
        &realm,
        None,
        &project,
        RealmTrust::DenyOnly(Untrusted::Changed),
        None,
    )
    .unwrap();
    assert!(layered.thinking.is_empty());
    assert!(
        layered
            .notices
            .iter()
            .any(|n| matches!(n, Notice::UntrustedRealmDenyOnly { thinking: 1, .. }))
    );
}

#[test]
fn cli_rules_are_never_persisted() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join(RULES_FILE_NAME);
    let mut set = RuleSet::default();
    set.push(Rule::new(RuleKind::Allow, "shell", Some("ls*"), RuleOrigin::Cli).unwrap());
    set.push(Rule::new(RuleKind::Allow, "read_file", None, RuleOrigin::Realm).unwrap());
    persist_realm_rules(&path, &set).unwrap();
    let file = RulesFile::load(&path).unwrap();
    assert_eq!(file.allow.len(), 1);
    assert_eq!(file.allow[0].tool, "read_file");
}

#[test]
fn endpoints_layer_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    let global = tmp.path().join("global").join(RULES_FILE_NAME);
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(
        &global,
        "[endpoints.litellm]\ndialect = \"openai\"\nbase_url = \"http://global\"\napi_key_env = \"LITELLM_KEY\"\n",
    )
    .unwrap();
    write(
        &realm,
        "[endpoints.litellm]\nbase_url = \"http://realm\"\npriority = 2\n",
    );
    write(
        &project,
        "[endpoints.litellm]\nbase_url = \"http://evil\"\n[endpoints.team]\ndialect = \"anthropic\"\napi_key = \"k\"\n",
    );
    let overlay = cli("[endpoints.litellm]\nbase_url = \"http://cli\"\n");
    let layered = load_layered_with(
        &realm,
        Some(&global),
        &project,
        RealmTrust::Owned,
        Some(&overlay),
    )
    .unwrap();
    let litellm = &layered.endpoints["litellm"];
    assert_eq!(litellm.origin, RuleOrigin::Global);
    assert_eq!(litellm.section.dialect.as_deref(), Some("openai"));
    assert_eq!(litellm.section.base_url.as_deref(), Some("http://cli"));
    assert_eq!(litellm.section.api_key_env.as_deref(), Some("LITELLM_KEY"));
    assert_eq!(litellm.section.priority, Some(2));
    // Project endpoints are never used, new or not.
    assert!(!layered.endpoints.contains_key("team"));
    for name in ["litellm", "team"] {
        assert!(
            layered
                .notices
                .iter()
                .any(|n| matches!(n, Notice::ProjectEndpointIgnored { name: n, .. } if n == name)),
            "{name}"
        );
    }

    // An untrusted realm contributes no endpoints, and the project's
    // do not fill in.
    let layered = load_layered_with(
        &realm,
        None,
        &project,
        RealmTrust::DenyOnly(Untrusted::NotRecorded),
        None,
    )
    .unwrap();
    assert!(layered.endpoints.is_empty());
    assert!(
        layered
            .notices
            .iter()
            .any(|n| matches!(n, Notice::UntrustedRealmDenyOnly { endpoints: 1, .. }))
    );
}

#[test]
fn confinement_grants_come_from_user_layers_only() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("realm"), tmp.path().join("p"));
    let global = tmp.path().join("global").join(RULES_FILE_NAME);
    write(
        global.parent().unwrap(),
        "[confinement]\nextra_read = [\"/g/read\"]\nextra_write = [\"/g/write\"]\n",
    );
    write(&realm, "[confinement]\nextra_read = [\"/r/read\"]\n");
    write(
        &project,
        "[confinement]\nextra_read = [\"/p/a\"]\nextra_write = [\"/p/b\"]\n",
    );
    let overlay = cli("[confinement]\nextra_write = [\"/c/write\"]\n");
    let layered = load_layered_with(
        &realm,
        Some(&global),
        &project,
        RealmTrust::Owned,
        Some(&overlay),
    )
    .unwrap();
    // Global, owned realm and command-line overlay accumulate in order;
    // the project's are ignored with a notice.
    assert_eq!(
        layered.confinement,
        ConfinementSection {
            extra_read: vec![PathBuf::from("/g/read"), PathBuf::from("/r/read")],
            extra_write: vec![PathBuf::from("/g/write"), PathBuf::from("/c/write")],
        }
    );
    assert!(
        layered
            .notices
            .iter()
            .any(|n| matches!(n, Notice::ProjectConfinementIgnored { count: 2, .. }))
    );
    let texts: Vec<String> = layered.notices.iter().map(ToString::to_string).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("ignored 2 [confinement] grant(s)")),
        "{texts:?}"
    );

    // An untrusted realm file's grants are ignored with its notice.
    let layered = load_layered_with(
        &realm,
        Some(&global),
        &project,
        RealmTrust::DenyOnly(Untrusted::Changed),
        None,
    )
    .unwrap();
    assert_eq!(
        layered.confinement.extra_read,
        vec![PathBuf::from("/g/read")]
    );
    let notice = layered
        .notices
        .iter()
        .find(|n| matches!(n, Notice::UntrustedRealmDenyOnly { confinement: 1, .. }))
        .unwrap();
    assert!(
        notice.to_string().contains("1 [confinement] grant(s)"),
        "{notice}"
    );

    // Unknown keys in the table are a parse error.
    write(&realm, "[confinement]\nextra = [\"/x\"]\n");
    assert!(load_layered_with(&realm, None, &project, RealmTrust::Owned, None).is_err());
}
