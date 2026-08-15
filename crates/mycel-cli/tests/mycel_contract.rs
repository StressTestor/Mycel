use std::{collections::HashMap, fs};

use assert_cmd::Command as ProcessCommand;
use clap::Parser;
use mycel_agent_runtime::SessionIndex;
use mycel_cli::cli::{
    validate, validate_provider_command, CatalogCommand, Cli, Command, DoctorTarget, OutputFormat,
    PermissionMode, ProviderAuthTarget, ProviderCommand, SessionSelection, ValidatedMode,
};
use predicates::prelude::*;

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).expect("CLI should parse")
}

fn validation_error(args: &[&str]) -> String {
    validate(parse(args), &HashMap::new())
        .expect_err("options should conflict")
        .to_string()
}

#[test]
fn root_flags_and_hidden_aliases_map_to_the_same_contract() {
    let cli = parse(&[
        "mycel",
        "-r",
        "session-1",
        "-C",
        "--yes",
        "--model",
        "model-a",
        "--skills-dir",
        "skills-a",
        "--skills-dir",
        "skills-b",
        "--add-dir",
        "workspace-a",
    ]);
    assert_eq!(cli.session.as_deref(), Some("session-1"));
    assert!(cli.continue_session);
    assert!(cli.yolo);
    assert_eq!(cli.model.as_deref(), Some("model-a"));
    assert_eq!(cli.skills_dirs.len(), 2);
    assert_eq!(cli.add_dirs.len(), 1);

    let alias = parse(&["mycel", "--resume=session-2", "--auto-approve"]);
    assert_eq!(alias.session.as_deref(), Some("session-2"));
    assert!(alias.yolo);
}

#[test]
fn bare_session_selects_the_interactive_picker() {
    let validated = validate(parse(&["mycel", "--session", "--plan"]), &HashMap::new())
        .expect("valid interactive options");
    let ValidatedMode::Interactive(request) = validated.mode else {
        panic!("expected interactive mode");
    };
    assert_eq!(request.session, SessionSelection::Pick);
    assert_eq!(request.permission, PermissionMode::Manual);
    assert!(request.plan);
}

#[test]
fn prompt_conflicts_match_the_stable_cli_contract() {
    assert_eq!(
        validation_error(&["mycel", "--prompt", "hello", "--yolo"]),
        "Cannot combine --prompt with --yolo."
    );
    assert_eq!(
        validation_error(&["mycel", "--prompt", "hello", "--auto"]),
        "Cannot combine --prompt with --auto."
    );
    assert_eq!(
        validation_error(&["mycel", "--prompt", "hello", "--plan"]),
        "Cannot combine --prompt with --plan."
    );
    assert_eq!(
        validation_error(&["mycel", "--prompt", "hello", "--session"]),
        "Cannot use --session without an id in prompt mode."
    );
    assert_eq!(
        validation_error(&["mycel", "--session=abc", "--continue"]),
        "Cannot combine --continue, --session."
    );
    assert_eq!(
        validation_error(&["mycel", "--yolo", "--auto"]),
        "Cannot combine --yolo with --auto."
    );
}

#[test]
fn prompt_and_model_reject_whitespace_and_output_requires_prompt() {
    assert_eq!(
        validation_error(&["mycel", "--prompt", "   "]),
        "Prompt cannot be empty."
    );
    assert_eq!(
        validation_error(&["mycel", "--model", "   "]),
        "Model cannot be empty."
    );
    assert_eq!(
        validation_error(&["mycel", "--output-format", "stream-json"]),
        "Output format is only supported in prompt mode."
    );
}

#[test]
fn output_format_flag_beats_environment_and_invalid_environment_fails() {
    let mut environment =
        HashMap::from([("MYCEL_OUTPUT_FORMAT".to_owned(), "stream-json".to_owned())]);
    let validated =
        validate(parse(&["mycel", "-p", "hello"]), &environment).expect("environment format");
    let ValidatedMode::Prompt(request) = validated.mode else {
        panic!("expected prompt mode");
    };
    assert_eq!(request.output_format, OutputFormat::StreamJson);

    environment.insert("MYCEL_OUTPUT_FORMAT".to_owned(), "not-a-format".to_owned());
    let explicit = validate(
        parse(&["mycel", "-p", "hello", "--output-format", "text"]),
        &environment,
    )
    .expect("flag takes precedence");
    let ValidatedMode::Prompt(request) = explicit.mode else {
        panic!("expected prompt mode");
    };
    assert_eq!(request.output_format, OutputFormat::Text);

    let error = validate(parse(&["mycel", "-p", "hello"]), &environment)
        .expect_err("invalid environment value");
    assert_eq!(
        error.to_string(),
        "Invalid MYCEL_OUTPUT_FORMAT value \"not-a-format\". Expected one of: text, stream-json."
    );

    let interactive = validate(parse(&["mycel"]), &environment)
        .expect("ambient prompt format is ignored in interactive mode");
    assert!(matches!(interactive.mode, ValidatedMode::Interactive(_)));
}

#[test]
fn headless_goal_create_uses_reserved_word_escape_and_replace_semantics() {
    let escaped = validate(
        parse(&["mycel", "--prompt", "/goal -- status"]),
        &HashMap::new(),
    )
    .expect("escaped goal objective");
    let ValidatedMode::Prompt(escaped) = escaped.mode else {
        panic!("expected prompt mode");
    };
    let goal = escaped.goal.expect("goal create request");
    assert_eq!(goal.objective, "status");
    assert!(!goal.replace);

    let replace = validate(
        parse(&["mycel", "--prompt", "/goal replace -- do the work"]),
        &HashMap::new(),
    )
    .expect("replacement goal");
    let ValidatedMode::Prompt(replace) = replace.mode else {
        panic!("expected prompt mode");
    };
    let goal = replace.goal.expect("goal replace request");
    assert_eq!(goal.objective, "do the work");
    assert!(goal.replace);

    let control = validate(
        parse(&["mycel", "--prompt", "/goal status"]),
        &HashMap::new(),
    )
    .expect("goal control falls through as a normal prompt");
    let ValidatedMode::Prompt(control) = control.mode else {
        panic!("expected prompt mode");
    };
    assert!(control.goal.is_none());
}

#[test]
fn subcommand_shapes_preserve_provider_catalog_arguments() {
    let provider = parse(&[
        "mycel",
        "provider",
        "catalog",
        "list",
        "openai",
        "--filter",
        "gpt",
        "--url",
        "https://catalog.invalid/api.json",
        "--json",
    ]);
    let Some(Command::Provider(provider)) = provider.command else {
        panic!("expected provider command");
    };
    let ProviderCommand::Catalog(catalog) = provider.command else {
        panic!("expected catalog command");
    };
    let CatalogCommand::List {
        provider_id,
        filter,
        url,
        json,
    } = catalog.command
    else {
        panic!("expected catalog list");
    };
    assert_eq!(provider_id.as_deref(), Some("openai"));
    assert_eq!(filter.as_deref(), Some("gpt"));
    assert_eq!(url, "https://catalog.invalid/api.json");
    assert!(json);
}

#[test]
fn doctor_export_login_and_provider_mutation_shapes_parse_without_runtime() {
    let doctor = parse(&["mycel", "doctor", "config", "custom.toml"]);
    let Some(Command::Doctor(doctor)) = doctor.command else {
        panic!("expected doctor command");
    };
    let Some(DoctorTarget::Config { path }) = doctor.target else {
        panic!("expected doctor config target");
    };
    assert_eq!(path.expect("custom path").to_string_lossy(), "custom.toml");

    let doctor_all = parse(&["mycel", "doctor"]);
    let Some(Command::Doctor(doctor_all)) = doctor_all.command else {
        panic!("expected doctor command");
    };
    assert!(doctor_all.target.is_none());

    let export = parse(&[
        "mycel",
        "export",
        "session-1",
        "--output",
        "debug.zip",
        "--yes",
        "--no-include-global-log",
    ]);
    let Some(Command::Export(export)) = export.command else {
        panic!("expected export command");
    };
    assert_eq!(export.session_id.as_deref(), Some("session-1"));
    assert_eq!(
        export.output.expect("output path").to_string_lossy(),
        "debug.zip"
    );
    assert!(export.yes);
    assert!(!export.include_global_log);

    assert!(matches!(
        parse(&["mycel", "login"]).command,
        Some(Command::Login)
    ));

    let provider_login = parse(&["mycel", "provider", "login", "kimi"]);
    assert!(matches!(
        provider_login.command,
        Some(Command::Provider(provider))
            if provider.command == (ProviderCommand::Login {
                provider: ProviderAuthTarget::Kimi,
            })
    ));
    let provider_logout = parse(&["mycel", "provider", "logout", "kimi"]);
    assert!(matches!(
        provider_logout.command,
        Some(Command::Provider(provider))
            if provider.command == (ProviderCommand::Logout {
                provider: ProviderAuthTarget::Kimi,
            })
    ));

    let list = parse(&["mycel", "provider", "list", "--json"]);
    assert!(matches!(
        list.command,
        Some(Command::Provider(provider))
            if provider.command == ProviderCommand::List { json: true }
    ));
    let remove = parse(&["mycel", "provider", "remove", "openai"]);
    assert!(matches!(
        remove.command,
        Some(Command::Provider(provider))
            if provider.command == (ProviderCommand::Remove {
                provider_id: "openai".to_owned(),
            })
    ));

    let add = parse(&[
        "mycel",
        "provider",
        "add",
        "https://registry.invalid/api.json",
        "--api-key",
        "secret",
    ]);
    let Some(Command::Provider(add)) = add.command else {
        panic!("expected provider command");
    };
    assert!(matches!(
        add.command,
        ProviderCommand::Add {
            ref url,
            api_key: Some(ref api_key),
        } if url == "https://registry.invalid/api.json" && api_key.expose() == "secret"
    ));

    let catalog_add = parse(&[
        "mycel",
        "provider",
        "catalog",
        "add",
        "anthropic",
        "--api-key",
        "secret",
        "--default-model",
        "claude",
        "--url",
        "https://catalog.invalid/api.json",
    ]);
    let Some(Command::Provider(provider)) = catalog_add.command else {
        panic!("expected provider command");
    };
    let ProviderCommand::Catalog(catalog) = provider.command else {
        panic!("expected catalog command");
    };
    assert!(matches!(
        catalog.command,
        CatalogCommand::Add {
            ref provider_id,
            api_key: Some(ref api_key),
            default_model: Some(ref model),
            ref url,
        } if provider_id == "anthropic"
            && api_key.expose() == "secret"
            && model == "claude"
            && url == "https://catalog.invalid/api.json"
    ));
}

#[test]
fn provider_parser_rejects_unsupported_auth_and_unsafe_import_inputs() {
    for args in [
        ["mycel", "provider", "login", "openai"].as_slice(),
        ["mycel", "provider", "logout", "anthropic"].as_slice(),
        ["mycel", "provider", "login"].as_slice(),
    ] {
        assert!(Cli::try_parse_from(args).is_err(), "accepted {args:?}");
    }

    for url in [
        "http://catalog.example/api.json",
        "file:///tmp/api.json",
        "https://user:secret@catalog.example/api.json",
        "https://catalog.example/api.json?channel=latest",
        "https://catalog.example/api",
    ] {
        let cli = parse(&["mycel", "provider", "add", url]);
        let Some(Command::Provider(provider)) = cli.command else {
            panic!("expected provider command")
        };
        let error = validate_provider_command(&provider.command)
            .expect_err("unsafe URL should fail")
            .to_string();
        assert!(!error.contains("secret"));
    }

    assert!(Cli::try_parse_from(["mycel", "provider", "remove", "../escape"]).is_err());
    assert!(Cli::try_parse_from(["mycel", "provider", "catalog", "list", "openai",]).is_err());
}

#[test]
fn provider_api_keys_are_redacted_in_parsed_debug_output() {
    let cli = parse(&[
        "mycel",
        "provider",
        "catalog",
        "add",
        "openai",
        "--api-key",
        "do-not-print-me",
        "--url",
        "http://127.0.0.1/catalog.json",
    ]);
    let debug = format!("{cli:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("do-not-print-me"));
}

#[test]
fn production_binary_requires_real_config_and_runs_provider_management() {
    let home = tempfile::tempdir().expect("temporary MYCEL_HOME");
    ProcessCommand::cargo_bin("mycel")
        .expect("mycel binary")
        .env("MYCEL_HOME", home.path())
        .args(["--prompt", "hello", "--output-format", "stream-json"])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "error: runtime prompt execution failed: could not read",
        ));

    fs::write(
        home.path().join("config.toml"),
        r#"
default_model = "local"

[providers.local]
type = "openai"
base_url = "http://127.0.0.1:11434/v1"
api_key = "contract-secret"

[models.local]
provider = "local"
model = "gpt-test"
max_context_size = 8192
"#,
    )
    .expect("provider config");

    ProcessCommand::cargo_bin("mycel")
        .expect("mycel binary")
        .env("MYCEL_HOME", home.path())
        .args(["provider", "list", "--json"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains(r#""id":"local""#)
                .and(predicate::str::contains(r#""credential":"configured""#))
                .and(predicate::str::contains("contract-secret").not()),
        )
        .stderr(predicate::str::is_empty());
}

#[test]
fn production_doctor_is_offline_and_missing_defaults_are_skipped() {
    let home = tempfile::tempdir().expect("temporary MYCEL_HOME");
    ProcessCommand::cargo_bin("mycel")
        .expect("mycel binary")
        .env("MYCEL_HOME", home.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("SKIP config.toml")
                .and(predicate::str::contains("SKIP tui.toml"))
                .and(predicate::str::contains(
                    "All checked config files are valid.",
                )),
        )
        .stderr(predicate::str::is_empty());
}

#[test]
fn production_export_supports_explicit_session_without_network() {
    let home = tempfile::tempdir().expect("temporary MYCEL_HOME");
    let records = home
        .path()
        .join("sessions/session-1/agents/main/records.jsonl");
    fs::create_dir_all(records.parent().expect("record parent")).expect("session directories");
    fs::write(
        &records,
        "{\"type\":\"metadata\",\"protocol_version\":\"1.4\",\"created_at\":0}\n",
    )
    .expect("records");
    SessionIndex::new(home.path())
        .register_session("session-1", home.path(), &[])
        .expect("session index");
    let output = home.path().join("session.zip");

    ProcessCommand::cargo_bin("mycel")
        .expect("mycel binary")
        .env("MYCEL_HOME", home.path())
        .args([
            "export",
            "session-1",
            "--output",
            output.to_str().expect("UTF-8 output"),
            "--yes",
            "--no-include-global-log",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(output.to_string_lossy()))
        .stderr(predicate::str::is_empty());
    assert!(fs::read(output).expect("ZIP").starts_with(b"PK\x03\x04"));
}

#[test]
fn production_export_defaults_to_newest_session_in_current_directory() {
    let home = tempfile::tempdir().expect("temporary MYCEL_HOME");
    let records = home
        .path()
        .join("sessions/session-1/agents/main/records.jsonl");
    fs::create_dir_all(records.parent().expect("record parent")).expect("session directories");
    fs::write(
        &records,
        "{\"type\":\"metadata\",\"protocol_version\":\"1.4\",\"created_at\":0}\n",
    )
    .expect("records");
    SessionIndex::new(home.path())
        .register_session("session-1", home.path(), &[])
        .expect("session index");
    let output = home.path().join("newest.zip");

    ProcessCommand::cargo_bin("mycel")
        .expect("mycel binary")
        .env("MYCEL_HOME", home.path())
        .current_dir(home.path())
        .args([
            "export",
            "--output",
            output.to_str().expect("UTF-8 output"),
            "--yes",
            "--no-include-global-log",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(output.to_string_lossy()))
        .stderr(predicate::str::is_empty());
    assert!(fs::read(output).expect("ZIP").starts_with(b"PK\x03\x04"));
}

#[test]
fn help_exposes_product_flags_but_hides_compatibility_aliases() {
    ProcessCommand::cargo_bin("mycel")
        .expect("mycel binary")
        .arg("--help")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--session")
                .and(predicate::str::contains("--output-format"))
                .and(predicate::str::contains("provider"))
                .and(predicate::str::contains("--resume").not())
                .and(predicate::str::contains("--auto-approve").not()),
        );
}

#[test]
fn parser_errors_use_the_existing_cli_error_code() {
    ProcessCommand::cargo_bin("mycel")
        .expect("mycel binary")
        .args(["--output-format", "invalid"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("invalid value 'invalid'"));
}
