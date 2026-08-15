//! Strict local plugin installation ledger.
//!
//! Rust owns manifest validation and execution. This module translates and
//! updates `<MYCEL_HOME>/plugins/installed.json`, installs only explicit local
//! directories into a confined managed root, and never downloads a plugin or
//! follows symlinks while copying or mutating plugin state.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, SecondsFormat, Utc};
use mycel_agent_runtime::plugins::{LocalPluginRegistry, PluginLimits, PluginRegistration};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const INSTALLED_RELATIVE_PATH: &str = "plugins/installed.json";
const MAX_INSTALLED_BYTES: u64 = 1024 * 1024;
const MAX_INSTALLED_PLUGINS: usize = 256;
const MAX_COPY_ENTRIES: usize = 4096;
const MAX_COPY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_COPY_DEPTH: usize = 32;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstalledFile {
    version: u8,
    plugins: Vec<InstalledPlugin>,
}

impl Default for InstalledFile {
    fn default() -> Self {
        Self {
            version: 1,
            plugins: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InstalledPlugin {
    id: String,
    root: PathBuf,
    source: String,
    enabled: bool,
    installed_at: String,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    original_source: Option<String>,
    #[serde(default)]
    capabilities: PluginCapabilities,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PluginCapabilities {
    #[serde(default)]
    mcp_servers: BTreeMap<String, PluginMcpState>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PluginMcpState {
    enabled: bool,
}

pub fn load_plugin_registrations(home: &Path) -> Result<Vec<PluginRegistration>, String> {
    registrations_from_installed(read_installed_file(home)?)
}

fn read_installed_file(home: &Path) -> Result<InstalledFile, String> {
    let path = home.join(INSTALLED_RELATIVE_PATH);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(InstalledFile::default())
        }
        Err(error) => {
            return Err(format!(
                "could not inspect plugin installation ledger {}: {error}",
                path.display()
            ))
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "plugin installation ledger {} must not be a symbolic link",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "plugin installation ledger {} is not a regular file",
            path.display()
        ));
    }
    if metadata.len() > MAX_INSTALLED_BYTES {
        return Err(format!(
            "plugin installation ledger {} exceeds the {} byte limit",
            path.display(),
            MAX_INSTALLED_BYTES
        ));
    }
    let source = fs::read(&path).map_err(|error| {
        format!(
            "could not read plugin installation ledger {}: {error}",
            path.display()
        )
    })?;
    if source.len() as u64 > MAX_INSTALLED_BYTES {
        return Err(format!(
            "plugin installation ledger {} exceeds the {} byte limit",
            path.display(),
            MAX_INSTALLED_BYTES
        ));
    }
    let installed: InstalledFile = serde_json::from_slice(&source).map_err(|_| {
        format!(
            "plugin installation ledger {} is invalid JSON",
            path.display()
        )
    })?;
    if installed.version != 1 {
        return Err(format!(
            "plugin installation ledger {} has unsupported version {}",
            path.display(),
            installed.version
        ));
    }
    if installed.plugins.len() > MAX_INSTALLED_PLUGINS {
        return Err(format!(
            "plugin installation ledger {} contains more than {} plugins",
            path.display(),
            MAX_INSTALLED_PLUGINS
        ));
    }

    validate_installed_file(&installed)?;
    Ok(installed)
}

fn validate_installed_file(installed: &InstalledFile) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    let mut roots = BTreeSet::new();
    for plugin in &installed.plugins {
        validate_installed_plugin(plugin)?;
        if !ids.insert(plugin.id.clone()) {
            return Err(format!("duplicate installed plugin id {:?}", plugin.id));
        }
        if !roots.insert(plugin.root.clone()) {
            return Err(format!(
                "multiple installed plugins use root {}",
                plugin.root.display()
            ));
        }
    }
    Ok(())
}

fn registrations_from_installed(
    installed: InstalledFile,
) -> Result<Vec<PluginRegistration>, String> {
    validate_installed_file(&installed)?;
    let registrations = installed
        .plugins
        .into_iter()
        .map(|plugin| {
            let disabled_mcp_servers = plugin
                .capabilities
                .mcp_servers
                .into_iter()
                .filter_map(|(name, state)| (!state.enabled).then_some(name))
                .collect();
            PluginRegistration {
                root: plugin.root,
                enabled: plugin.enabled,
                disabled_mcp_servers,
                expected_id: Some(plugin.id),
            }
        })
        .collect();
    Ok(registrations)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginInstallResult {
    pub id: String,
    pub version: String,
    pub replaced: bool,
    pub managed_root: PathBuf,
}

pub fn install_local_plugin(
    home: &Path,
    source: &Path,
    now: DateTime<Utc>,
) -> Result<PluginInstallResult, String> {
    if !source.is_absolute() {
        return Err(format!(
            "plugin source must be an absolute local directory: {}",
            source.display()
        ));
    }
    let source = fs::canonicalize(source).map_err(|error| {
        format!(
            "could not resolve local plugin source {}: {error}",
            source.display()
        )
    })?;
    let source_metadata = fs::metadata(&source).map_err(|error| {
        format!(
            "could not inspect local plugin source {}: {error}",
            source.display()
        )
    })?;
    if !source_metadata.is_dir() {
        return Err(format!(
            "local plugin source is not a directory: {}",
            source.display()
        ));
    }
    let source_info = inspect_plugin(&source, None)?;

    let mut installed = read_installed_file(home)?;
    let existing_index = installed
        .plugins
        .iter()
        .position(|plugin| plugin.id == source_info.id);
    if existing_index.is_none() && installed.plugins.len() >= MAX_INSTALLED_PLUGINS {
        return Err(format!(
            "cannot install more than {MAX_INSTALLED_PLUGINS} local plugins"
        ));
    }

    let managed_dir = prepare_managed_directory(home)?;
    let managed_root = managed_dir.join(&source_info.id);
    let nonce = Uuid::new_v4();
    let staging_root = managed_dir.join(format!(".{}-{nonce}.staging", source_info.id));
    let backup_root = managed_dir.join(format!(".{}-{nonce}.backup", source_info.id));
    copy_local_tree(&source, &staging_root)?;

    let staged_info = match inspect_plugin(&staging_root, Some(&source_info.id)) {
        Ok(info) => info,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging_root);
            return Err(error);
        }
    };

    let now = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let replaced = existing_index.is_some();
    let existing = existing_index.map(|index| installed.plugins[index].clone());
    let record = InstalledPlugin {
        id: staged_info.id.clone(),
        root: managed_root.clone(),
        source: "local-path".to_owned(),
        enabled: existing.as_ref().is_none_or(|plugin| plugin.enabled),
        installed_at: existing
            .as_ref()
            .map_or_else(|| now.clone(), |plugin| plugin.installed_at.clone()),
        updated_at: Some(now),
        original_source: Some(source.to_string_lossy().into_owned()),
        capabilities: existing
            .map(|plugin| plugin.capabilities)
            .unwrap_or_default(),
    };
    if let Some(index) = existing_index {
        installed.plugins[index] = record;
    } else {
        installed.plugins.push(record);
    }
    installed
        .plugins
        .sort_by(|left, right| left.id.cmp(&right.id));
    validate_installed_file(&installed)?;

    let had_managed_root = match fs::symlink_metadata(&managed_root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                let _ = fs::remove_dir_all(&staging_root);
                return Err(format!(
                    "managed plugin target {} must be a real directory",
                    managed_root.display()
                ));
            }
            fs::rename(&managed_root, &backup_root).map_err(|error| {
                let _ = fs::remove_dir_all(&staging_root);
                format!(
                    "could not stage existing managed plugin {}: {error}",
                    managed_root.display()
                )
            })?;
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging_root);
            return Err(format!(
                "could not inspect managed plugin target {}: {error}",
                managed_root.display()
            ));
        }
    };
    if let Err(error) = fs::rename(&staging_root, &managed_root) {
        if had_managed_root {
            let _ = fs::rename(&backup_root, &managed_root);
        }
        let _ = fs::remove_dir_all(&staging_root);
        return Err(format!(
            "could not activate managed plugin {}: {error}",
            managed_root.display()
        ));
    }
    if let Err(error) = write_installed_file(home, &installed) {
        let _ = fs::remove_dir_all(&managed_root);
        if had_managed_root {
            let _ = fs::rename(&backup_root, &managed_root);
        }
        return Err(error);
    }
    if had_managed_root {
        fs::remove_dir_all(&backup_root).map_err(|error| {
            format!(
                "plugin was installed, but the previous managed copy {} could not be removed: {error}",
                backup_root.display()
            )
        })?;
    }
    Ok(PluginInstallResult {
        id: staged_info.id,
        version: staged_info.version,
        replaced,
        managed_root,
    })
}

pub fn set_installed_plugin_enabled(
    home: &Path,
    id: &str,
    enabled: bool,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let mut installed = read_installed_file(home)?;
    let plugin = installed
        .plugins
        .iter_mut()
        .find(|plugin| plugin.id == id)
        .ok_or_else(|| format!("plugin {id:?} is not installed"))?;
    plugin.enabled = enabled;
    plugin.updated_at = Some(now.to_rfc3339_opts(SecondsFormat::Secs, true));
    write_installed_file(home, &installed)
}

pub fn set_installed_plugin_mcp_enabled(
    home: &Path,
    id: &str,
    server: &str,
    enabled: bool,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let mut installed = read_installed_file(home)?;
    let plugin_index = installed
        .plugins
        .iter()
        .position(|plugin| plugin.id == id)
        .ok_or_else(|| format!("plugin {id:?} is not installed"))?;
    let registration = registration_from_plugin(&installed.plugins[plugin_index]);
    let info = inspect_registration(registration)?;
    if !info.mcp_servers.iter().any(|name| name == server) {
        return Err(format!(
            "plugin {id:?} does not declare MCP server {server:?}"
        ));
    }
    let plugin = &mut installed.plugins[plugin_index];
    plugin
        .capabilities
        .mcp_servers
        .insert(server.to_owned(), PluginMcpState { enabled });
    plugin.updated_at = Some(now.to_rfc3339_opts(SecondsFormat::Secs, true));
    write_installed_file(home, &installed)
}

pub fn remove_installed_plugin(home: &Path, id: &str) -> Result<(), String> {
    let mut installed = read_installed_file(home)?;
    let index = installed
        .plugins
        .iter()
        .position(|plugin| plugin.id == id)
        .ok_or_else(|| format!("plugin {id:?} is not installed"))?;
    let removed = installed.plugins.remove(index);
    write_installed_file(home, &installed)?;

    let managed_root = home.join("plugins/managed").join(id);
    if removed.root != managed_root {
        return Ok(());
    }
    match fs::symlink_metadata(&managed_root) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "plugin was removed from the ledger, but managed root {} is a symbolic link and was not deleted",
            managed_root.display()
        )),
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&managed_root).map_err(|error| {
            format!(
                "plugin was removed from the ledger, but managed root {} could not be deleted: {error}",
                managed_root.display()
            )
        }),
        Ok(_) => Err(format!(
            "plugin was removed from the ledger, but managed root {} is not a directory and was not deleted",
            managed_root.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "plugin was removed from the ledger, but managed root {} could not be inspected: {error}",
            managed_root.display()
        )),
    }
}

fn registration_from_plugin(plugin: &InstalledPlugin) -> PluginRegistration {
    PluginRegistration {
        root: plugin.root.clone(),
        enabled: plugin.enabled,
        disabled_mcp_servers: plugin
            .capabilities
            .mcp_servers
            .iter()
            .filter_map(|(name, state)| (!state.enabled).then_some(name.clone()))
            .collect(),
        expected_id: Some(plugin.id.clone()),
    }
}

fn inspect_plugin(
    root: &Path,
    expected_id: Option<&str>,
) -> Result<mycel_agent_runtime::plugins::PluginInfo, String> {
    let mut registration = PluginRegistration::enabled(root.to_path_buf());
    registration.expected_id = expected_id.map(str::to_owned);
    inspect_registration(registration)
}

fn inspect_registration(
    registration: PluginRegistration,
) -> Result<mycel_agent_runtime::plugins::PluginInfo, String> {
    let mut registry = LocalPluginRegistry::local(vec![registration], PluginLimits::default());
    let reload = registry.reload();
    if reload.loaded != 1 {
        let reason = reload
            .diagnostics
            .first()
            .map(|diagnostic| diagnostic.message.as_str())
            .unwrap_or("plugin manifest did not load");
        return Err(format!("local plugin validation failed: {reason}"));
    }
    registry
        .list()
        .into_iter()
        .next()
        .ok_or_else(|| "local plugin validation produced no plugin".to_owned())
}

fn prepare_managed_directory(home: &Path) -> Result<PathBuf, String> {
    if !home.is_absolute() {
        return Err(format!("MYCEL_HOME must be absolute: {}", home.display()));
    }
    let plugins = home.join("plugins");
    ensure_real_directory(&plugins)?;
    let managed = plugins.join("managed");
    ensure_real_directory(&managed)?;
    Ok(managed)
}

fn ensure_real_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "plugin state directory {} must not be a symbolic link",
            path.display()
        )),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(format!(
            "plugin state path {} is not a directory",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|error| {
                format!(
                    "could not create plugin state directory {}: {error}",
                    path.display()
                )
            })?;
            let metadata = fs::symlink_metadata(path).map_err(|error| {
                format!(
                    "could not verify plugin state directory {}: {error}",
                    path.display()
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "plugin state directory {} is not a real directory",
                    path.display()
                ));
            }
            Ok(())
        }
        Err(error) => Err(format!(
            "could not inspect plugin state directory {}: {error}",
            path.display()
        )),
    }
}

fn copy_local_tree(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir(destination).map_err(|error| {
        format!(
            "could not create plugin staging directory {}: {error}",
            destination.display()
        )
    })?;
    let mut entries = 1usize;
    let mut bytes = 0u64;
    let mut pending = vec![(source.to_path_buf(), destination.to_path_buf(), 0usize)];
    while let Some((source_dir, destination_dir, depth)) = pending.pop() {
        if depth > MAX_COPY_DEPTH {
            let _ = fs::remove_dir_all(destination);
            return Err(format!(
                "local plugin exceeds the maximum directory depth of {MAX_COPY_DEPTH}"
            ));
        }
        let children = fs::read_dir(&source_dir).map_err(|error| {
            let _ = fs::remove_dir_all(destination);
            format!(
                "could not read local plugin directory {}: {error}",
                source_dir.display()
            )
        })?;
        for child in children {
            let child = child.map_err(|error| {
                let _ = fs::remove_dir_all(destination);
                format!("could not enumerate local plugin files: {error}")
            })?;
            entries = entries.saturating_add(1);
            if entries > MAX_COPY_ENTRIES {
                let _ = fs::remove_dir_all(destination);
                return Err(format!(
                    "local plugin exceeds the {MAX_COPY_ENTRIES} entry limit"
                ));
            }
            let source_path = child.path();
            let destination_path = destination_dir.join(child.file_name());
            let metadata = fs::symlink_metadata(&source_path).map_err(|error| {
                let _ = fs::remove_dir_all(destination);
                format!(
                    "could not inspect local plugin entry {}: {error}",
                    source_path.display()
                )
            })?;
            if metadata.file_type().is_symlink() {
                let _ = fs::remove_dir_all(destination);
                return Err(format!(
                    "local plugin entry {} is a symbolic link",
                    source_path.display()
                ));
            }
            if metadata.is_dir() {
                fs::create_dir(&destination_path).map_err(|error| {
                    let _ = fs::remove_dir_all(destination);
                    format!(
                        "could not create plugin directory {}: {error}",
                        destination_path.display()
                    )
                })?;
                fs::set_permissions(&destination_path, metadata.permissions()).map_err(
                    |error| {
                        let _ = fs::remove_dir_all(destination);
                        format!(
                            "could not preserve plugin directory permissions for {}: {error}",
                            destination_path.display()
                        )
                    },
                )?;
                pending.push((source_path, destination_path, depth + 1));
            } else if metadata.is_file() {
                bytes = bytes.saturating_add(metadata.len());
                if bytes > MAX_COPY_BYTES {
                    let _ = fs::remove_dir_all(destination);
                    return Err(format!(
                        "local plugin exceeds the {MAX_COPY_BYTES} byte copy limit"
                    ));
                }
                fs::copy(&source_path, &destination_path).map_err(|error| {
                    let _ = fs::remove_dir_all(destination);
                    format!(
                        "could not copy local plugin file {}: {error}",
                        source_path.display()
                    )
                })?;
            } else {
                let _ = fs::remove_dir_all(destination);
                return Err(format!(
                    "local plugin entry {} is not a regular file or directory",
                    source_path.display()
                ));
            }
        }
    }
    Ok(())
}

fn write_installed_file(home: &Path, installed: &InstalledFile) -> Result<(), String> {
    validate_installed_file(installed)?;
    let mut data = serde_json::to_vec_pretty(installed)
        .map_err(|_| "could not serialize plugin installation ledger".to_owned())?;
    data.push(b'\n');
    if data.len() as u64 > MAX_INSTALLED_BYTES {
        return Err(format!(
            "plugin installation ledger would exceed the {MAX_INSTALLED_BYTES} byte limit"
        ));
    }
    let plugins = home.join("plugins");
    ensure_real_directory(&plugins)?;
    let final_path = home.join(INSTALLED_RELATIVE_PATH);
    match fs::symlink_metadata(&final_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(format!(
                "plugin installation ledger {} must be a regular file",
                final_path.display()
            ))
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "could not inspect plugin installation ledger {}: {error}",
                final_path.display()
            ))
        }
    }
    let temporary = plugins.join(format!(".installed-{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|error| {
        format!(
            "could not create temporary plugin installation ledger {}: {error}",
            temporary.display()
        )
    })?;
    let write_result = file
        .write_all(&data)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            format!(
                "could not write temporary plugin installation ledger {}: {error}",
                temporary.display()
            )
        });
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, &final_path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "could not atomically replace plugin installation ledger {}: {error}",
            final_path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&final_path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            format!(
                "plugin installation ledger {} was written but private permissions could not be enforced: {error}",
                final_path.display()
            )
        })?;
    }
    Ok(())
}

fn validate_installed_plugin(plugin: &InstalledPlugin) -> Result<(), String> {
    if !valid_component(&plugin.id) {
        return Err(format!("invalid installed plugin id {:?}", plugin.id));
    }
    if plugin.source != "local-path" {
        return Err(format!(
            "installed plugin {:?} has unsupported source {:?}",
            plugin.id, plugin.source
        ));
    }
    if !plugin.root.is_absolute() {
        return Err(format!(
            "installed plugin {:?} root must be an absolute local path",
            plugin.id
        ));
    }
    if plugin.installed_at.trim().is_empty() {
        return Err(format!(
            "installed plugin {:?} has an empty installedAt field",
            plugin.id
        ));
    }
    for name in plugin.capabilities.mcp_servers.keys() {
        if !valid_component(name) {
            return Err(format!(
                "installed plugin {:?} has invalid MCP state name {:?}",
                plugin.id, name
            ));
        }
    }
    // Retain these fields in the accepted bridge shape, but never use them as
    // executable input or remote acquisition metadata.
    let _ = (&plugin.updated_at, &plugin.original_source);
    Ok(())
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().enumerate().all(|(index, byte)| match byte {
            b'a'..=b'z' | b'0'..=b'9' => true,
            b'_' | b'-' => index > 0,
            _ => false,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn fixed_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 14, 12, 0, 0)
            .single()
            .expect("time")
    }

    fn write_plugin(root: &Path, name: &str, version: &str, with_mcp: bool) {
        fs::create_dir_all(root).expect("plugin root");
        let mut manifest = serde_json::json!({"name":name,"version":version});
        if with_mcp {
            manifest["mcpServers"] = serde_json::json!({
                "docs": {
                    "transport":"streamable-http",
                    "url":"http://127.0.0.1:8123/mcp"
                }
            });
        }
        fs::write(
            root.join("mycel.plugin.json"),
            serde_json::to_vec(&manifest).expect("manifest JSON"),
        )
        .expect("manifest");
    }

    #[test]
    fn missing_ledger_is_an_empty_local_registry() {
        let temp = TempDir::new().expect("temp");
        assert!(load_plugin_registrations(temp.path())
            .expect("missing ledger")
            .is_empty());
    }

    #[test]
    fn installed_state_becomes_expected_identity_and_disabled_mcp_state() {
        let temp = TempDir::new().expect("temp");
        let plugin_root = temp.path().join("plugins/managed/reviewer");
        fs::create_dir_all(&plugin_root).expect("plugin root");
        let ledger = temp.path().join(INSTALLED_RELATIVE_PATH);
        fs::create_dir_all(ledger.parent().expect("ledger parent")).expect("plugins dir");
        fs::write(
            &ledger,
            serde_json::to_vec(&serde_json::json!({
                "version":1,
                "plugins":[{
                    "id":"reviewer",
                    "root":plugin_root,
                    "source":"local-path",
                    "enabled":true,
                    "installedAt":"2026-08-14T00:00:00Z",
                    "capabilities":{"mcpServers":{"docs":{"enabled":false}}}
                }]
            }))
            .expect("JSON"),
        )
        .expect("ledger");

        let registrations = load_plugin_registrations(temp.path()).expect("registrations");
        assert_eq!(registrations.len(), 1);
        assert_eq!(registrations[0].expected_id.as_deref(), Some("reviewer"));
        assert!(registrations[0].disabled_mcp_servers.contains("docs"));
    }

    #[test]
    fn malformed_remote_duplicate_and_relative_records_fail_closed() {
        let temp = TempDir::new().expect("temp");
        let ledger = temp.path().join(INSTALLED_RELATIVE_PATH);
        fs::create_dir_all(ledger.parent().expect("ledger parent")).expect("plugins dir");
        for plugins in [
            serde_json::json!([{
                "id":"remote","root":"/tmp/remote","source":"url","enabled":true,
                "installedAt":"now"
            }]),
            serde_json::json!([{
                "id":"relative","root":"relative","source":"local-path","enabled":true,
                "installedAt":"now"
            }]),
            serde_json::json!([
                {"id":"same","root":"/tmp/a","source":"local-path","enabled":true,"installedAt":"now"},
                {"id":"same","root":"/tmp/b","source":"local-path","enabled":true,"installedAt":"now"}
            ]),
        ] {
            fs::write(
                &ledger,
                serde_json::to_vec(&serde_json::json!({"version":1,"plugins":plugins}))
                    .expect("JSON"),
            )
            .expect("ledger");
            assert!(load_plugin_registrations(temp.path()).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ledger_is_rejected() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp");
        let target = temp.path().join("outside.json");
        fs::write(&target, br#"{"version":1,"plugins":[]}"#).expect("target");
        let ledger = temp.path().join(INSTALLED_RELATIVE_PATH);
        fs::create_dir_all(ledger.parent().expect("ledger parent")).expect("plugins dir");
        symlink(target, ledger).expect("symlink");
        assert!(load_plugin_registrations(temp.path())
            .expect_err("symlink rejected")
            .contains("symbolic link"));
    }

    #[test]
    fn local_install_is_managed_atomic_and_preserves_state_on_update() {
        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        write_plugin(&source, "reviewer", "1.0.0", true);
        fs::write(source.join("payload.txt"), "first").expect("payload");

        let first = install_local_plugin(&home, &source, fixed_time()).expect("install");
        assert_eq!(first.id, "reviewer");
        assert_eq!(first.version, "1.0.0");
        assert!(!first.replaced);
        assert_eq!(
            fs::read_to_string(first.managed_root.join("payload.txt")).expect("payload"),
            "first"
        );
        set_installed_plugin_enabled(&home, "reviewer", false, fixed_time()).expect("disable");
        set_installed_plugin_mcp_enabled(&home, "reviewer", "docs", false, fixed_time())
            .expect("disable MCP");

        write_plugin(&source, "reviewer", "1.1.0", true);
        fs::write(source.join("payload.txt"), "second").expect("payload");
        let second = install_local_plugin(&home, &source, fixed_time()).expect("update");
        assert!(second.replaced);
        assert_eq!(second.version, "1.1.0");
        assert_eq!(
            fs::read_to_string(second.managed_root.join("payload.txt")).expect("payload"),
            "second"
        );
        let registrations = load_plugin_registrations(&home).expect("registrations");
        assert_eq!(registrations.len(), 1);
        assert!(!registrations[0].enabled);
        assert!(registrations[0].disabled_mcp_servers.contains("docs"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(home.join(INSTALLED_RELATIVE_PATH))
                .expect("ledger metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn remove_deletes_only_the_exact_managed_copy() {
        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        write_plugin(&source, "reviewer", "1.0.0", false);
        let installed = install_local_plugin(&home, &source, fixed_time()).expect("install");
        assert!(installed.managed_root.exists());

        remove_installed_plugin(&home, "reviewer").expect("remove");
        assert!(!installed.managed_root.exists());
        assert!(source.exists());
        assert!(load_plugin_registrations(&home)
            .expect("registrations")
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn install_rejects_symlinks_anywhere_in_the_source_tree() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        write_plugin(&source, "reviewer", "1.0.0", false);
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, "secret").expect("outside");
        symlink(&outside, source.join("linked.txt")).expect("symlink");

        let error = install_local_plugin(&home, &source, fixed_time())
            .expect_err("source symlink rejected");
        assert!(error.contains("symbolic link"), "{error}");
        assert!(!home.join("plugins/managed/reviewer").exists());
    }
}
