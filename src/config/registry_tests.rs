#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::config::{RULES_FILE_NAME, RulesFile, load_layered_with};
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
        "auto.trust_project_builds" => settings
            .trust_project_builds_given
            .then_some(Value::Boolean(settings.trust_project_builds)),
        "auto.classifier_tools" => to(Value::Boolean(settings.classifier_tools)),
        "auto.classifier_probe" => to(Value::Boolean(settings.classifier_probe)),
        "auto.compact" => to(Value::try_from(settings.compact).unwrap()),
        "auto.compact_warn_percent" => int(u64::from(settings.compact_warn_percent)),
        "auto.compact_auto_percent" => int(u64::from(settings.compact_auto_percent)),
        "auto.compact_prepare" => settings.compact_prepare.clone().map(Value::String),
        "auto.compact_prepare_at" => to(threshold_value(settings.compact_prepare_at)),
        "helpers.max_depth" => int(u64::from(layered.helpers.max_depth)),
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
        // The project's sample is its `enabled`, as the manual mode.
        ProjectRule::OnlyManual => {
            cli.is_none()
                && *project == Value::Boolean(false)
                && value.as_ref().and_then(Value::as_str) == Some("auto")
        }
    };
    if takes {
        value = Some(match setting.project {
            ProjectRule::OnlyManual => Value::String("default".to_string()),
            _ => project.clone(),
        });
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
        let project_samples = [Value::Boolean(false), Value::Boolean(true)];
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
            let project_key = match setting.project {
                ProjectRule::OnlyManual => "auto.enabled",
                _ => setting.key,
            };
            let tables = Layers {
                user: table_with(setting.key, layers[0]),
                workspace: table_with(setting.key, layers[1]),
                cli: table_with(setting.key, layers[2]),
                project: table_with(project_key, layers[3]),
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
            match setting.key {
                "auto.compact_prepare" => {
                    assert_eq!(layered.settings.compact_prepare_source, source, "{what}");
                }
                "auto.compact_prepare_at" => {
                    assert_eq!(layered.settings.compact_prepare_at_source, source, "{what}");
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

/// The mode's older spelling: `enabled` in a user layer is the mode, and a
/// `mode` beside it in the same file wins.
#[test]
fn the_mode_reads_its_older_spelling() {
    let setting = Setting::find("auto.mode").unwrap();
    let layers = |text: &str| Layers {
        workspace: text.parse().unwrap(),
        ..Layers::default()
    };
    let mode = |text: &str| setting.resolve(&layers(text)).value.unwrap();
    assert_eq!(mode("[auto]\nenabled = false\n").as_str(), Some("default"));
    assert_eq!(mode("[auto]\nenabled = true\n").as_str(), Some("auto"));
    assert_eq!(
        mode("[auto]\nenabled = false\nmode = \"plan\"\n").as_str(),
        Some("plan")
    );
}

#[test]
fn keys_are_unique_and_every_flag_writes_a_key_the_setting_reads() {
    let mut keys: Vec<&str> = SETTINGS.iter().map(|setting| setting.key).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), SETTINGS.len());
    for setting in SETTINGS {
        for flag in setting.flags {
            assert!(
                flag.key == setting.key || setting.alias.is_some_and(|alias| alias.key == flag.key),
                "{} writes {}",
                flag.flag,
                flag.key
            );
        }
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
