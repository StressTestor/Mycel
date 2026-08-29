use std::{
    io,
    path::{Path, PathBuf},
};

use toml::Value;

use crate::{
    cli::{DoctorArgs, DoctorTarget},
    production::{parse_config, ConfigSource},
    runtime::{AdapterOutput, RuntimeCompletion},
};

const CONFIG_FILE: &str = "config.toml";
const TUI_FILE: &str = "tui.toml";

#[derive(Clone, Copy)]
enum CheckKind {
    Config,
    Tui,
}

struct CheckSpec {
    kind: CheckKind,
    label: &'static str,
    path: PathBuf,
    explicit: bool,
}

struct CheckResult {
    label: &'static str,
    path: PathBuf,
    status: &'static str,
    message: Option<String>,
}

pub(crate) fn run_doctor(
    args: &DoctorArgs,
    home: &Path,
    cwd: &Path,
    source: &dyn ConfigSource,
) -> AdapterOutput {
    let specs = match &args.target {
        None => vec![
            CheckSpec {
                kind: CheckKind::Config,
                label: CONFIG_FILE,
                path: home.join(CONFIG_FILE),
                explicit: false,
            },
            CheckSpec {
                kind: CheckKind::Tui,
                label: TUI_FILE,
                path: home.join(TUI_FILE),
                explicit: false,
            },
        ],
        Some(DoctorTarget::Config { path }) => vec![target_spec(
            CheckKind::Config,
            CONFIG_FILE,
            path.as_deref(),
            home,
            cwd,
        )],
        Some(DoctorTarget::Tui { path }) => vec![target_spec(
            CheckKind::Tui,
            TUI_FILE,
            path.as_deref(),
            home,
            cwd,
        )],
    };
    let results: Vec<_> = specs
        .into_iter()
        .map(|spec| check_file(source, spec))
        .collect();
    let issue_count = results
        .iter()
        .filter(|result| result.status == "ERROR")
        .count();
    if issue_count == 0 {
        AdapterOutput {
            stdout: format_success(&results),
            stderr: String::new(),
            completion: RuntimeCompletion::success(),
        }
    } else {
        AdapterOutput {
            stdout: String::new(),
            stderr: format_failure(&results, issue_count),
            completion: RuntimeCompletion::failure(),
        }
    }
}

fn target_spec(
    kind: CheckKind,
    label: &'static str,
    path: Option<&Path>,
    home: &Path,
    cwd: &Path,
) -> CheckSpec {
    CheckSpec {
        kind,
        label,
        path: path.map_or_else(
            || home.join(label),
            |path| {
                let combined = if path.is_absolute() {
                    path.to_owned()
                } else {
                    cwd.join(path)
                };
                normalize_path(&combined)
            },
        ),
        explicit: path.is_some(),
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                } else if !normalized.has_root() {
                    normalized.push(component.as_os_str());
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn check_file(source: &dyn ConfigSource, spec: CheckSpec) -> CheckResult {
    let text = match source.read_to_string(&spec.path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return CheckResult {
                label: spec.label,
                path: spec.path,
                status: if spec.explicit { "ERROR" } else { "SKIP" },
                message: Some(if spec.explicit {
                    "File does not exist.".to_owned()
                } else {
                    "File does not exist; built-in defaults will apply.".to_owned()
                }),
            };
        }
        Err(error) => {
            return CheckResult {
                label: spec.label,
                path: spec.path,
                status: "ERROR",
                message: Some(format!("Could not read file: {error}")),
            };
        }
    };

    let validation = match spec.kind {
        CheckKind::Config => parse_config(&text).map(|_| ()),
        CheckKind::Tui => validate_tui_config(&text),
    };
    match validation {
        Ok(()) => CheckResult {
            label: spec.label,
            path: spec.path,
            status: "OK",
            message: None,
        },
        Err(message) => CheckResult {
            label: spec.label,
            message: Some(format!(
                "Invalid configuration in {}.\nValidation issues:\n  {message}",
                spec.path.display()
            )),
            path: spec.path,
            status: "ERROR",
        },
    }
}

fn validate_tui_config(source: &str) -> Result<(), String> {
    if source.trim().is_empty() {
        return Ok(());
    }
    let value: Value = toml::from_str(source).map_err(|error| format!("<root>: {error}"))?;
    let table = value
        .as_table()
        .ok_or_else(|| "<root>: expected a table".to_owned())?;
    let mut issues = Vec::new();
    expect_type(
        table.get("theme"),
        "theme",
        Value::is_str,
        "string",
        &mut issues,
    );
    expect_type(
        table.get("disable_paste_burst"),
        "disable_paste_burst",
        Value::is_bool,
        "boolean",
        &mut issues,
    );
    validate_editor(table.get("editor"), &mut issues);
    validate_rails(table.get("rails"), &mut issues);
    validate_startup(table.get("startup"), &mut issues);
    validate_notifications(table.get("notifications"), &mut issues);
    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues.join("\n  "))
    }
}

fn validate_editor(value: Option<&Value>, issues: &mut Vec<String>) {
    let Some(value) = value else { return };
    let Some(table) = value.as_table() else {
        issues.push(format!(
            "editor: expected table, found {}",
            value_type(value)
        ));
        return;
    };
    expect_type(
        table.get("command"),
        "editor.command",
        Value::is_str,
        "string",
        issues,
    );
}

fn validate_rails(value: Option<&Value>, issues: &mut Vec<String>) {
    let Some(value) = value else { return };
    let Some(table) = value.as_table() else {
        issues.push(format!(
            "rails: expected table, found {}",
            value_type(value)
        ));
        return;
    };
    expect_type(
        table.get("session_open"),
        "rails.session_open",
        Value::is_bool,
        "boolean",
        issues,
    );
    expect_type(
        table.get("inspector_open"),
        "rails.inspector_open",
        Value::is_bool,
        "boolean",
        issues,
    );
}

fn validate_startup(value: Option<&Value>, issues: &mut Vec<String>) {
    let Some(value) = value else { return };
    let Some(table) = value.as_table() else {
        issues.push(format!(
            "startup: expected table, found {}",
            value_type(value)
        ));
        return;
    };
    expect_type(
        table.get("flourish"),
        "startup.flourish",
        Value::is_bool,
        "boolean",
        issues,
    );
}

fn validate_notifications(value: Option<&Value>, issues: &mut Vec<String>) {
    let Some(value) = value else { return };
    let Some(table) = value.as_table() else {
        issues.push(format!(
            "notifications: expected table, found {}",
            value_type(value)
        ));
        return;
    };
    expect_type(
        table.get("enabled"),
        "notifications.enabled",
        Value::is_bool,
        "boolean",
        issues,
    );
    if let Some(condition) = table.get("notification_condition") {
        match condition.as_str() {
            Some("unfocused" | "always") => {}
            Some(other) => issues.push(format!(
                "notifications.notification_condition: expected unfocused or always, found {other:?}"
            )),
            None => issues.push(format!(
                "notifications.notification_condition: expected string, found {}",
                value_type(condition)
            )),
        }
    }
}

fn expect_type(
    value: Option<&Value>,
    path: &str,
    predicate: fn(&Value) -> bool,
    expected: &str,
    issues: &mut Vec<String>,
) {
    if let Some(value) = value {
        if !predicate(value) {
            issues.push(format!(
                "{path}: expected {expected}, found {}",
                value_type(value)
            ));
        }
    }
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Integer(_) => "integer",
        Value::Float(_) => "float",
        Value::Boolean(_) => "boolean",
        Value::Datetime(_) => "datetime",
        Value::Array(_) => "array",
        Value::Table(_) => "table",
    }
}

fn format_success(results: &[CheckResult]) -> String {
    format!(
        "Mycel doctor\n\n{}\n\nAll checked config files are valid.\n",
        format_results(results)
    )
}

fn format_failure(results: &[CheckResult], issue_count: usize) -> String {
    let noun = if issue_count == 1 { "issue" } else { "issues" };
    format!(
        "Mycel doctor found {issue_count} {noun}.\n\n{}\n",
        format_results(results)
    )
}

fn format_results(results: &[CheckResult]) -> String {
    let mut lines = Vec::new();
    for result in results {
        lines.push(format!(
            "{} {:<12} {}",
            result.status,
            result.label,
            result.path.display()
        ));
        if let Some(message) = &result.message {
            lines.extend(message.lines().map(|line| format!("  {line}")));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use tempfile::TempDir;

    use super::*;
    use crate::{production::FileConfigSource, runtime::RuntimeCompletion};

    fn args(target: Option<DoctorTarget>) -> DoctorArgs {
        DoctorArgs { target }
    }

    #[test]
    fn missing_defaults_have_stable_success_golden() {
        let temp = TempDir::new().expect("temp");
        let output = run_doctor(&args(None), temp.path(), temp.path(), &FileConfigSource);
        let expected = format!(
            "Mycel doctor\n\nSKIP config.toml  {}\n  File does not exist; built-in defaults will apply.\nSKIP tui.toml     {}\n  File does not exist; built-in defaults will apply.\n\nAll checked config files are valid.\n",
            temp.path().join("config.toml").display(),
            temp.path().join("tui.toml").display()
        );
        assert_eq!(output.stdout, expected);
        assert_eq!(output.stderr, "");
        assert_eq!(output.completion, RuntimeCompletion::success());
    }

    #[test]
    fn explicit_missing_file_is_an_error_golden() {
        let temp = TempDir::new().expect("temp");
        let output = run_doctor(
            &args(Some(DoctorTarget::Config {
                path: Some(PathBuf::from("./nested/../candidate.toml")),
            })),
            temp.path(),
            temp.path(),
            &FileConfigSource,
        );
        assert_eq!(output.stdout, "");
        assert_eq!(output.completion, RuntimeCompletion::failure());
        assert_eq!(
            output.stderr,
            format!(
                "Mycel doctor found 1 issue.\n\nERROR config.toml  {}\n  File does not exist.\n",
                temp.path().join("candidate.toml").display()
            )
        );
    }

    #[test]
    fn aggregates_semantic_config_and_tui_issues() {
        let temp = TempDir::new().expect("temp");
        fs::write(
            temp.path().join("config.toml"),
            "[providers.kimi]\ntype='kimi'\n[models.kimi]\nprovider='kimi'\nmodel='kimi'\nmax_context_size=0\n",
        )
        .expect("config");
        fs::write(
            temp.path().join("tui.toml"),
            "editor = 123\n[notifications]\nenabled = 'yes'\n",
        )
        .expect("tui");
        let output = run_doctor(&args(None), temp.path(), temp.path(), &FileConfigSource);
        assert_eq!(output.completion, RuntimeCompletion::failure());
        assert!(output.stderr.contains("Mycel doctor found 2 issues."));
        assert!(output.stderr.contains("max_context_size"));
        assert!(output.stderr.contains("editor: expected table"));
        assert!(output
            .stderr
            .contains("notifications.enabled: expected boolean"));
    }

    #[test]
    fn flags_non_boolean_rail_state() {
        let temp = TempDir::new().expect("temp");
        fs::write(
            temp.path().join("tui.toml"),
            "[rails]\nsession_open = 'yes'\ninspector_open = 1\n",
        )
        .expect("tui");
        let output = run_doctor(&args(None), temp.path(), temp.path(), &FileConfigSource);
        assert_eq!(output.completion, RuntimeCompletion::failure());
        assert!(output
            .stderr
            .contains("rails.session_open: expected boolean"));
        assert!(output
            .stderr
            .contains("rails.inspector_open: expected boolean"));
    }

    #[test]
    fn flags_non_boolean_startup_flourish() {
        let temp = TempDir::new().expect("temp");
        fs::write(
            temp.path().join("tui.toml"),
            "[startup]\nflourish = 'yes'\n",
        )
        .expect("tui");
        let output = run_doctor(&args(None), temp.path(), temp.path(), &FileConfigSource);
        assert_eq!(output.completion, RuntimeCompletion::failure());
        assert!(output.stderr.contains("startup.flourish: expected boolean"));
    }

    #[test]
    fn validates_valid_explicit_tui_and_ignores_unknown_keys() {
        let temp = Arc::new(TempDir::new().expect("temp"));
        fs::write(
            temp.path().join("custom.toml"),
            "theme='dark'\nunknown=true\n[editor]\ncommand='vim'\n[notifications]\nenabled=true\nnotification_condition='always'\n",
        )
        .expect("tui");
        let output = run_doctor(
            &args(Some(DoctorTarget::Tui {
                path: Some(temp.path().join("custom.toml")),
            })),
            temp.path(),
            temp.path(),
            &FileConfigSource,
        );
        assert_eq!(output.completion, RuntimeCompletion::success());
        assert!(output.stdout.contains("OK tui.toml"));
    }
}
