#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::config::settings::PrepareSetting;
use crate::config::{Layered, RULES_FILE_NAME, RulesFile, load_layered_with};
use std::path::Path;

/// Two distinct values a layer may hold for `setting` (neither needs the
/// clamps, so the layering's own normalization stays out of the way).
fn samples(setting: &Setting) -> [Value; 2] {
    let int = |n: i64| Value::Integer(n);
    let text = |s: &str| Value::String(s.to_string());
    match setting.key {
        "auto.mode" => [text("plan"), text("default")],
        "auto.classifier_model" => [text("claude-haiku-5-5"), text("proxy:gpt-5.5")],
        "auto.compact" => [text("auto"), text("warn")],
        "auto.compact_warn_percent" => [int(60), int(70)],
        "auto.compact_auto_percent" => [int(90), int(95)],
        "auto.compact_prepare" => [text("update PLAN.md"), text("")],
        "auto.compact_prepare_at" => [int(60_000), text("65%")],
        "helpers.max_depth" => [int(2), int(0)],
        "helpers.model" => [text("claude-haiku-5-5"), text("proxy:gpt-5.5")],
        "helpers.allow_models" | "helpers.deny_models" => [
            Value::Array(vec![text("claude-haiku-*")]),
            Value::Array(vec![text("gpt-5.5"), text("claude-*")]),
        ],
        "shell.user_timeout_secs" => [int(30), int(300)],
        "search.budget_secs" => [int(5), int(20)],
        "search.max_entries" => [int(1_000), int(900_000)],
        "ui.helpers" => [text("expanded"), text("folded")],
        "ui.checklist" => [text("shown"), text("hidden")],
        "daemon.principal" => [text("alice"), text("bob")],
        _ => match setting.kind {
            Kind::Bool => [Value::Boolean(true), Value::Boolean(false)],
            other => panic!("no samples for {} ({other:?})", setting.key),
        },
    }
}

/// A table holding `value` at the dotted `key`.
fn table_with(key: &str, value: Option<&Value>) -> Table {
    let mut root = Table::new();
    let Some(value) = value else {
        return root;
    };
    let parts: Vec<&str> = key.split('.').collect();
    let mut table = &mut root;
    for part in &parts[..parts.len() - 1] {
        table = table
            .entry((*part).to_string())
            .or_insert_with(|| Value::Table(Table::new()))
            .as_table_mut()
            .unwrap();
    }
    table.insert(parts[parts.len() - 1].to_string(), value.clone());
    root
}

/// What the layering gives for `key`, read from the typed view.
fn typed_value(key: &str, layered: &Layered) -> Option<Value> {
    let settings = &layered.settings;
    let int = |n: u64| Some(Value::Integer(i64::try_from(n).unwrap()));
    let to = |value: Value| Some(value);
    let ser = |v: Value| v;
    match key {
        "auto.mode" => to(ser(Value::try_from(layered.mode).unwrap())),
        "auto.classifier_model" => settings.classifier_model.clone().map(Value::String),
        "auto.trust_project_builds" => settings.trust_project_builds.map(Value::Boolean),
        "auto.classifier_tools" => to(Value::Boolean(settings.classifier_tools)),
        "auto.classifier_probe" => to(Value::Boolean(settings.classifier_probe)),
        "auto.compact" => to(Value::try_from(settings.compact).unwrap()),
        "auto.compact_warn_percent" => int(u64::from(settings.compact_warn_percent)),
        "auto.compact_auto_percent" => int(u64::from(settings.compact_auto_percent)),
        // The instruction's typed view trims and reads an empty one as
        // off; the samples are trimmed.
        "auto.compact_prepare" => {
            let instruction = PrepareSetting::of(&layered.layers).instruction;
            (instruction.source != Level::Default)
                .then(|| Value::String(instruction.value.unwrap_or_default()))
        }
        "auto.compact_prepare_at" => to(threshold_value(
            PrepareSetting::of(&layered.layers).at.value,
        )),
        "helpers.max_depth" => int(u64::from(layered.helpers.max_depth)),
        "helpers.model" | "helpers.allow_models" | "helpers.deny_models" => {
            use crate::config::helper_models::{HelperModels, ModelRule};
            let models = HelperModels::of(&layered.layers).unwrap();
            let list = |patterns: &[crate::config::helper_models::ModelPattern]| {
                Some(Value::Array(
                    patterns
                        .iter()
                        .map(|p| Value::String(p.as_str().to_string()))
                        .collect(),
                ))
            };
            match (key, &models.rule) {
                ("helpers.model", _) => models.default.map(Value::String),
                ("helpers.allow_models", ModelRule::Allow(patterns))
                | ("helpers.deny_models", ModelRule::Deny(patterns)) => list(patterns),
                _ => None,
            }
        }
        "shell.user_timeout_secs" => int(layered.shell.user_timeout_secs),
        "search.budget_secs" => int(layered.search.budget.as_secs()),
        "search.max_entries" => int(layered.search.max_entries),
        "ui.helpers" => to(Value::try_from(layered.ui.helpers).unwrap()),
        "ui.bell" => to(Value::Boolean(layered.ui.bell)),
        "ui.checklist" => to(Value::try_from(layered.ui.checklist).unwrap()),
        "pricing.fetch" => to(Value::Boolean(layered.pricing_fetch)),
        "daemon.principal" => layered.daemon_principal.clone().map(Value::String),
        other => panic!("no typed view of {other}"),
    }
}

/// The precedence the layering has always had, written out on its own:
/// the user's layers in order, then the project's as its rule says.
fn oracle(
    setting: &Setting,
    [user, realm, cli, project]: [Option<&Value>; 4],
) -> (Option<Value>, Level) {
    let mut value = setting.default.value();
    let mut source = Level::Default;
    for (level, layer) in [
        (Level::User, user),
        (Level::Workspace, realm),
        (Level::CommandLine, cli),
    ] {
        if let Some(layer) = layer {
            value = Some(layer.clone());
            source = level;
        }
    }
    let Some(project) = project else {
        return (value, source);
    };
    let takes = match setting.project {
        ProjectRule::Takes => cli.is_none(),
        ProjectRule::Never => false,
        ProjectRule::OnlyLower => {
            project.as_integer().unwrap() <= value.as_ref().unwrap().as_integer().unwrap()
        }
        ProjectRule::OnlyFalse => *project == Value::Boolean(false),
        // Of a project's mode only `"default"` is read, as the manual mode.
        ProjectRule::OnlyManual => {
            cli.is_none()
                && project.as_str() == Some("default")
                && value.as_ref().and_then(Value::as_str) == Some("auto")
        }
    };
    if takes {
        value = Some(project.clone());
        source = Level::Project;
    }
    (value, source)
}

fn write(dir: &Path, table: &Table) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(RULES_FILE_NAME), toml::to_string(table).unwrap()).unwrap();
}

/// Every setting, from every combination of layers (each unset or one of
/// two values): the layering's typed view and [`Setting::resolve`] give
/// what the precedence and the project rule say. Written against the
/// layering as it was before the registry, and kept as it moved onto it.
#[test]
fn every_setting_layers_as_it_always_did() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, realm, project) = (
        tmp.path().join("home"),
        tmp.path().join("realm"),
        tmp.path().join("project"),
    );
    let mut checked = 0;
    for setting in SETTINGS {
        let samples = samples(setting);
        let choices: Vec<Option<&Value>> = vec![None, Some(&samples[0]), Some(&samples[1])];
        let project_samples = [
            Value::String("default".into()),
            Value::String("plan".into()),
        ];
        let project_choices: Vec<Option<&Value>> = if setting.project == ProjectRule::OnlyManual {
            vec![None, Some(&project_samples[0]), Some(&project_samples[1])]
        } else {
            choices.clone()
        };
        for combo in 0..81_usize {
            let pick = |digit: u32| combo / 3_usize.pow(digit) % 3;
            let layers = [
                choices[pick(0)],
                choices[pick(1)],
                choices[pick(2)],
                project_choices[pick(3)],
            ];
            let tables = Layers {
                user: table_with(setting.key, layers[0]),
                workspace: table_with(setting.key, layers[1]),
                cli: table_with(setting.key, layers[2]),
                project: table_with(setting.key, layers[3]),
            };
            write(&home, &tables.user);
            write(&realm, &tables.workspace);
            write(&project, &tables.project);
            let cli: RulesFile = Value::Table(tables.cli.clone()).try_into().unwrap();
            let layered = load_layered_with(
                &realm,
                Some(&home.join(RULES_FILE_NAME)),
                &project,
                Some(&cli),
            )
            .unwrap();
            let (want, source) = oracle(setting, layers);
            let what = format!("{} from {layers:?}", setting.key);
            assert_eq!(typed_value(setting.key, &layered), want, "{what}");
            let resolved = setting.resolve(&tables);
            assert_eq!(resolved.value, want, "resolve: {what}");
            assert_eq!(resolved.source, source, "resolve's source: {what}");
            let prepare = PrepareSetting::of(&layered.layers);
            match setting.key {
                "auto.compact_prepare" => {
                    assert_eq!(prepare.instruction.source, source, "{what}");
                }
                "auto.compact_prepare_at" => {
                    assert_eq!(prepare.at.source, source, "{what}");
                }
                _ => {}
            }
            // What removing the workspace's value leaves.
            let (beneath, from) = oracle(setting, [layers[0], None, layers[2], layers[3]]);
            let without = setting.resolve_without(&tables, Some(Level::Workspace));
            assert_eq!(
                (without.value, without.source),
                (beneath, from),
                "without: {what}"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, SETTINGS.len() * 81);
}

/// The registry's defaults read as the typed views (`Typed::defaults`,
/// which every load starts from; the views have no `Default` of their
/// own), and they are what the layering gives when no file sets anything.
/// The two types that keep a `Default` agree with them: the mode (a UI's
/// before any file is read) and the search limits (built from the
/// constants the registry's entries name).
#[test]
fn the_registry_s_defaults_read_as_the_typed_views() {
    let defaults = crate::config::Typed::defaults().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let layered =
        load_layered_with(&tmp.path().join("r"), None, &tmp.path().join("p"), None).unwrap();
    assert_eq!(layered.settings, defaults.settings);
    assert_eq!(layered.mode, defaults.mode);
    assert_eq!(layered.helpers, defaults.helpers);
    assert_eq!(layered.shell, defaults.shell);
    assert_eq!(layered.search, defaults.search);
    assert_eq!(layered.ui, defaults.ui);
    assert_eq!(layered.pricing_fetch, defaults.pricing_fetch);
    assert_eq!(layered.daemon_principal, defaults.daemon_principal);
    assert_eq!(
        defaults.mode,
        crate::approval::mode::PermissionMode::default()
    );
    assert_eq!(defaults.search, crate::tools::SearchLimits::default());
    // A few as the registry's table states them.
    assert!(defaults.ui.bell && defaults.settings.classifier_tools && defaults.pricing_fetch);
    assert_eq!(
        defaults.helpers.max_depth,
        crate::config::DEFAULT_HELPER_MAX_DEPTH
    );
    assert_eq!(defaults.daemon_principal, None);
}

/// `auto.enabled`, the mode's spelling before 0.0.7, is gone: a file that
/// still has it in any layer is a load error naming it (ignoring
/// `enabled = false` would start in auto mode).
#[test]
fn a_file_with_auto_enabled_does_not_load() {
    let tmp = tempfile::tempdir().unwrap();
    let (realm, project) = (tmp.path().join("r"), tmp.path().join("p"));
    for (dir, other) in [(&realm, &project), (&project, &realm)] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::create_dir_all(other).unwrap();
        std::fs::write(dir.join(RULES_FILE_NAME), "[auto]\nenabled = false\n").unwrap();
        let _ = std::fs::remove_file(other.join(RULES_FILE_NAME));
        let err = load_layered_with(&realm, None, &project, None).unwrap_err();
        assert!(err.to_string().contains("unknown field `enabled`"), "{err}");
    }
}

#[test]
fn keys_are_unique_and_every_default_is_a_value_the_setting_takes() {
    let mut keys: Vec<&str> = SETTINGS.iter().map(|setting| setting.key).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), SETTINGS.len());
    for setting in SETTINGS {
        // The default is a value the setting takes.
        if let Some(value) = setting.default.value() {
            setting.check(&value).unwrap();
        }
        for sample in samples(setting) {
            setting.check(&sample).unwrap();
        }
    }
}

#[test]
fn values_are_parsed_checked_and_refused_with_what_the_key_takes() {
    let find = |key| Setting::find(key).unwrap();
    assert_eq!(find("ui.bell").parse("off"), Ok(Value::Boolean(false)));
    // A pattern list: names separated by commas, or a TOML list; a bad
    // entry is named.
    let list = Value::Array(vec![
        Value::String("claude-haiku-*".into()),
        Value::String("gpt-5.5".into()),
    ]);
    assert_eq!(
        find("helpers.allow_models").parse("claude-haiku-*, gpt-5.5"),
        Ok(list.clone())
    );
    assert_eq!(
        find("helpers.deny_models").parse(r#"["claude-haiku-*", "gpt-5.5"]"#),
        Ok(list)
    );
    let refused = find("helpers.allow_models").parse("a*b").unwrap_err();
    assert!(
        refused.starts_with("helpers.allow_models") && refused.contains("`a*b`"),
        "{refused}"
    );
    assert_eq!(
        find("auto.compact_prepare_at").parse("65%"),
        Ok(Value::String("65%".into()))
    );
    assert_eq!(
        find("auto.compact_prepare_at").parse("300k"),
        Ok(Value::Integer(300_000))
    );
    assert_eq!(
        find("auto.compact_prepare").parse("\"update PLAN.md\""),
        Ok(Value::String("update PLAN.md".into()))
    );
    for (key, text, needle) in [
        ("auto.compact_warn_percent", "300", "from 1 to 100"),
        ("auto.compact_warn_percent", "0", "from 1 to 100"),
        ("auto.compact", "sometimes", "`native`, `warn`, `auto`"),
        (
            "auto.mode",
            "yolo",
            "`default`, `accept-edits`, `plan`, `auto`",
        ),
        ("ui.bell", "maybe", "`true` or `false`"),
        ("auto.compact_prepare_at", "lots", "no threshold"),
        ("auto.classifier_model", "two words", "[endpoint:]model"),
        ("shell.user_timeout_secs", "0", "from 1"),
    ] {
        let err = find(key).parse(text).unwrap_err();
        assert!(
            err.contains(key) && err.contains(needle),
            "{key}={text}: {err}"
        );
    }
}

/// The manual's settings table is the registry's: regenerate it with
/// `manual_table()` when this fails.
#[test]
fn the_manual_s_settings_table_is_the_registry_s() {
    let manual = include_str!("../../docs/manual.md");
    let begin = "<!-- settings-table:begin -->\n";
    let end = "<!-- settings-table:end -->";
    let start = manual.find(begin).expect("the begin marker") + begin.len();
    let stop = manual.find(end).expect("the end marker");
    let table = manual_table();
    assert!(
        manual[start..stop] == table,
        "docs/manual.md's settings table drifted from the registry; it should read:\n{table}"
    );
}
