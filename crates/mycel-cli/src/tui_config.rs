use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use toml::Value;

use crate::tui::theme::Theme;

const CONFIG_LIMIT: u64 = 1024 * 1024;
pub(crate) const INVALID_TUI_CONFIG_MESSAGE: &str =
    "Invalid TUI config in ~/.mycel/tui.toml; using defaults.";
/// Startup warning surfaced when the configured theme is `light`, which the
/// rebuilt TUI resolves to amanita (see `active_theme`).
pub(crate) const LIGHT_THEME_WARNING: &str =
    "light theme is not supported by the rebuilt TUI yet; using amanita";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThemeName {
    Auto,
    Dark,
    Light,
    Named(String),
}

impl ThemeName {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Auto => "auto",
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Named(name) => name.as_str(),
        }
    }

    // Validates against `Theme::ALL` by name to avoid constructing a
    // throwaway `Theme`.
    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim().to_ascii_lowercase();
        match value.as_str() {
            "auto" => Ok(Self::Auto),
            "dark" => Ok(Self::Dark),
            "light" => Ok(Self::Light),
            named if Theme::ALL.iter().any(|(candidate, _)| *candidate == named) => {
                Ok(Self::Named(value))
            }
            _ => Err(format!(
                "theme must be one of: auto, dark, light, {}",
                Theme::ALL.map(|(name, _)| name).join(", ")
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationCondition {
    Unfocused,
    Always,
}

impl NotificationCondition {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unfocused => "unfocused",
            Self::Always => "always",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TuiConfig {
    pub theme: ThemeName,
    pub editor_command: Option<String>,
    pub disable_paste_burst: bool,
    pub notifications_enabled: bool,
    pub notification_condition: NotificationCondition,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            theme: ThemeName::Auto,
            editor_command: None,
            disable_paste_burst: false,
            notifications_enabled: true,
            notification_condition: NotificationCondition::Unfocused,
        }
    }
}

pub(crate) fn load_tui_config(home: &Path) -> (TuiConfig, Option<String>) {
    match try_load_tui_config(&home.join("tui.toml")) {
        Ok(config) => (config, None),
        Err(_) => (
            TuiConfig::default(),
            Some(INVALID_TUI_CONFIG_MESSAGE.to_owned()),
        ),
    }
}

fn try_load_tui_config(path: &Path) -> Result<TuiConfig, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TuiConfig::default())
        }
        Err(error) => return Err(format!("could not inspect {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(format!(
            "{} must be a regular file, not a symlink or special file",
            path.display()
        ));
    }
    if metadata.len() > CONFIG_LIMIT {
        return Err(format!("{} exceeds the 1 MiB limit", path.display()));
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(TuiConfig::default());
    }
    let value = text
        .parse::<Value>()
        .map_err(|error| format!("invalid {}: {error}", path.display()))?;
    let table = value
        .as_table()
        .ok_or_else(|| "TUI config root must be a TOML table".to_owned())?;
    let mut config = TuiConfig::default();
    if let Some(value) = table.get("theme") {
        config.theme = ThemeName::parse(
            value
                .as_str()
                .ok_or_else(|| "theme must be a string".to_owned())?,
        )?;
    }
    if let Some(value) = table.get("disable_paste_burst") {
        config.disable_paste_burst = value
            .as_bool()
            .ok_or_else(|| "disable_paste_burst must be a boolean".to_owned())?;
    }
    if let Some(editor) = table.get("editor") {
        let editor = editor
            .as_table()
            .ok_or_else(|| "editor must be a TOML table".to_owned())?;
        if let Some(command) = editor.get("command") {
            let command = command
                .as_str()
                .ok_or_else(|| "editor.command must be a string".to_owned())?
                .trim();
            if command.len() > 4096 || command.chars().any(char::is_control) {
                return Err(
                    "editor.command must be at most 4096 bytes and contain no controls".to_owned(),
                );
            }
            config.editor_command = (!command.is_empty()).then(|| command.to_owned());
        }
    }
    if let Some(notifications) = table.get("notifications") {
        let notifications = notifications
            .as_table()
            .ok_or_else(|| "notifications must be a TOML table".to_owned())?;
        if let Some(enabled) = notifications.get("enabled") {
            config.notifications_enabled = enabled
                .as_bool()
                .ok_or_else(|| "notifications.enabled must be a boolean".to_owned())?;
        }
        if let Some(condition) = notifications.get("notification_condition") {
            config.notification_condition = match condition
                .as_str()
                .ok_or_else(|| "notification_condition must be a string".to_owned())?
            {
                "unfocused" => NotificationCondition::Unfocused,
                "always" => NotificationCondition::Always,
                _ => return Err("notification_condition must be unfocused or always".to_owned()),
            };
        }
    }
    Ok(config)
}

pub(crate) fn save_tui_config(home: &Path, config: &TuiConfig) -> Result<PathBuf, String> {
    let path = home.join("tui.toml");
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(format!(
                "{} must be a regular file, not a symlink or special file",
                path.display()
            ));
        }
    }
    ensure_private_directory(home)?;
    let editor = config.editor_command.as_deref().unwrap_or("");
    let encoded = format!(
        "# ~/.mycel/tui.toml\n# Client preferences for Mycel.\n# Agent/runtime settings stay in ~/.mycel/config.toml.\n\ntheme = {}\ndisable_paste_burst = {}\n\n[editor]\ncommand = {}\n\n[notifications]\nenabled = {}\nnotification_condition = {}\n",
        toml::Value::String(config.theme.as_str().to_owned()),
        config.disable_paste_burst,
        toml::Value::String(editor.to_owned()),
        config.notifications_enabled,
        toml::Value::String(config.notification_condition.as_str().to_owned()),
    );
    if encoded.len() as u64 > CONFIG_LIMIT {
        return Err("TUI config would exceed the 1 MiB limit".to_owned());
    }
    let temporary = home.join(format!(".tui-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| format!("could not create {}: {error}", temporary.display()))?;
        file.write_all(encoded.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
        fs::rename(&temporary, &path).map_err(|error| {
            format!(
                "could not atomically replace {} from {}: {error}",
                path.display(),
                temporary.display()
            )
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map(|()| path)
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    if fs::symlink_metadata(path).is_err() {
        fs::create_dir_all(path)
            .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(format!("{} must be a real directory", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not protect {}: {error}", path.display()))?;
    }
    Ok(())
}

/// Resolve a configured theme name to a concrete `Theme`. Named themes map by
/// name (falling back to amanita if the name is somehow unknown). The
/// auto/dark/light aliases all resolve to amanita: the seven built-in themes
/// are dark and no light palette has been designed, so a configured `light`
/// gets `LIGHT_THEME_WARNING` at startup rather than a silent dark card.
pub(crate) fn active_theme(name: &ThemeName) -> Theme {
    match name {
        // A `Named` value only arises from `ThemeName::parse`, which validated
        // it against `Theme::ALL`; failing to resolve here means the two paths
        // drifted. Loud in debug, amanita fallback in release.
        ThemeName::Named(named) => Theme::by_name(named).unwrap_or_else(|| {
            debug_assert!(false, "parse-validated theme {named:?} did not resolve");
            Theme::amanita()
        }),
        ThemeName::Auto | ThemeName::Dark | ThemeName::Light => Theme::amanita(),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn active_theme_resolves_named_and_aliases_to_amanita() {
        assert_eq!(
            active_theme(&ThemeName::Named("hacker".to_owned())).name,
            "hacker"
        );
        assert_eq!(active_theme(&ThemeName::Auto).name, "amanita");
        assert_eq!(active_theme(&ThemeName::Dark).name, "amanita");
        assert_eq!(active_theme(&ThemeName::Light).name, "amanita");
    }

    #[test]
    fn every_builtin_name_round_trips_from_parse_to_resolution() {
        // The end-to-end guarantee behind `active_theme`'s debug_assert: every
        // name the validator accepts resolves to the theme of that exact name.
        for (name, _) in Theme::ALL {
            let parsed = ThemeName::parse(name).expect("builtin name parses");
            assert_eq!(active_theme(&parsed).name, name);
        }
    }

    #[test]
    #[should_panic(expected = "did not resolve")]
    fn unvalidated_named_theme_is_loud_in_debug() {
        // Constructing `Named` without `parse` is a programming error; the
        // debug assert makes the drift loud instead of silently amanita.
        active_theme(&ThemeName::Named("nope".to_owned()));
    }

    #[test]
    fn defaults_partial_config_and_private_round_trip_are_stable() {
        let temp = tempdir().expect("temp");
        let (missing, warning) = load_tui_config(temp.path());
        assert_eq!(missing, TuiConfig::default());
        assert!(warning.is_none());
        let configured = TuiConfig {
            theme: ThemeName::Light,
            editor_command: Some("nvim".to_owned()),
            disable_paste_burst: true,
            notifications_enabled: false,
            notification_condition: NotificationCondition::Always,
        };
        let path = save_tui_config(temp.path(), &configured).expect("save");
        assert_eq!(load_tui_config(temp.path()), (configured, None));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn malformed_and_symlinked_configs_warn_and_fall_back_without_following() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temp");
        fs::write(temp.path().join("tui.toml"), "theme = 1").expect("invalid");
        let (config, warning) = load_tui_config(temp.path());
        assert_eq!(config, TuiConfig::default());
        assert_eq!(warning.as_deref(), Some(INVALID_TUI_CONFIG_MESSAGE));
        fs::remove_file(temp.path().join("tui.toml")).expect("remove");
        let target = temp.path().join("target");
        fs::write(&target, "theme = 'light'").expect("target");
        symlink(&target, temp.path().join("tui.toml")).expect("symlink");
        let (_, warning) = load_tui_config(temp.path());
        assert_eq!(warning.as_deref(), Some(INVALID_TUI_CONFIG_MESSAGE));
        assert!(save_tui_config(temp.path(), &TuiConfig::default()).is_err());
    }

    #[test]
    fn theme_names_parse() {
        assert!(ThemeName::parse("hacker").is_ok());
        assert!(ThemeName::parse("amanita").is_ok());
        assert!(ThemeName::parse("dark").is_ok());
        assert!(ThemeName::parse("bogus").is_err());
    }
}
