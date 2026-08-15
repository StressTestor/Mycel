use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use toml::Value;

const LOCAL_CONFIG_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct WorkspaceLocalConfig {
    pub project_root: PathBuf,
    pub config_path: PathBuf,
    pub additional_dirs: Vec<PathBuf>,
}

pub(crate) fn load_workspace_local_config(
    working_dir: &Path,
) -> Result<WorkspaceLocalConfig, String> {
    let project_root = find_project_root(working_dir);
    let config_path = project_root.join(".mycel/local.toml");
    let Some(document) = read_document(&config_path)? else {
        return Ok(WorkspaceLocalConfig {
            project_root,
            config_path,
            additional_dirs: Vec::new(),
        });
    };
    let additional_dirs = workspace_entries(&document)?
        .into_iter()
        .map(|entry| resolve_directory(&project_root, Path::new(&entry)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(WorkspaceLocalConfig {
        project_root,
        config_path,
        additional_dirs: normalize_dirs(additional_dirs),
    })
}

pub(crate) fn remember_workspace_additional_dir(
    working_dir: &Path,
    additional_dir: &Path,
) -> Result<WorkspaceLocalConfig, String> {
    let project_root = find_project_root(working_dir);
    let config_path = project_root.join(".mycel/local.toml");
    let additional_dir = resolve_directory(working_dir, additional_dir)?;
    let mut document = read_document(&config_path)?.unwrap_or_else(empty_document);
    let mut directories = workspace_entries(&document)?
        .into_iter()
        .map(|entry| resolve_directory(&project_root, Path::new(&entry)))
        .collect::<Result<Vec<_>, _>>()?;
    directories.push(additional_dir);
    directories = normalize_dirs(directories);

    let table = document
        .as_table_mut()
        .ok_or_else(|| "workspace local config root must be a TOML table".to_owned())?;
    let workspace = table
        .entry("workspace")
        .or_insert_with(|| Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .ok_or_else(|| "workspace local config [workspace] must be a TOML table".to_owned())?;
    workspace.insert(
        "additional_dir".to_owned(),
        Value::Array(
            directories
                .iter()
                .map(|path| Value::String(path.to_string_lossy().into_owned()))
                .collect(),
        ),
    );
    let encoded = toml::to_string(&document)
        .map_err(|error| format!("could not encode {}: {error}", config_path.display()))?;
    if encoded.len() as u64 > LOCAL_CONFIG_LIMIT {
        return Err(format!(
            "workspace local config {} would exceed the {} MiB limit",
            config_path.display(),
            LOCAL_CONFIG_LIMIT / (1024 * 1024)
        ));
    }
    write_private_atomic(&config_path, format!("{encoded}\n").as_bytes())?;
    Ok(WorkspaceLocalConfig {
        project_root,
        config_path,
        additional_dirs: directories,
    })
}

pub(crate) fn resolve_workspace_directory(
    working_dir: &Path,
    input: &str,
    user_home: Option<&Path>,
) -> Result<PathBuf, String> {
    let input = input.trim();
    if input.is_empty() || input.chars().any(char::is_control) {
        return Err("additional directory must be a non-empty path without controls".to_owned());
    }
    let path = if input == "~" {
        user_home
            .map(Path::to_path_buf)
            .ok_or_else(|| "cannot resolve '~' because HOME is not set".to_owned())?
    } else if let Some(suffix) = input.strip_prefix("~/") {
        user_home
            .map(|home| home.join(suffix))
            .ok_or_else(|| format!("cannot resolve {input:?} because HOME is not set"))?
    } else {
        let path = Path::new(input);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            working_dir.join(path)
        }
    };
    resolve_directory(working_dir, &path)
}

fn read_document(path: &Path) -> Result<Option<Value>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not inspect {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(format!(
            "workspace local config {} must be a regular file, not a symlink or special file",
            path.display()
        ));
    }
    if metadata.len() > LOCAL_CONFIG_LIMIT {
        return Err(format!(
            "workspace local config {} exceeds the {} MiB limit",
            path.display(),
            LOCAL_CONFIG_LIMIT / (1024 * 1024)
        ));
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let value = text
        .parse::<Value>()
        .map_err(|error| format!("invalid workspace local config {}: {error}", path.display()))?;
    if !value.is_table() {
        return Err(format!(
            "workspace local config {} root must be a TOML table",
            path.display()
        ));
    }
    Ok(Some(value))
}

fn workspace_entries(document: &Value) -> Result<Vec<String>, String> {
    let Some(workspace) = document.get("workspace") else {
        return Ok(Vec::new());
    };
    let workspace = workspace
        .as_table()
        .ok_or_else(|| "workspace local config [workspace] must be a TOML table".to_owned())?;
    let Some(entries) = workspace.get("additional_dir") else {
        return Ok(Vec::new());
    };
    let entries = entries
        .as_array()
        .ok_or_else(|| "workspace.additional_dir must be an array of paths".to_owned())?;
    entries
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .filter(|entry| !entry.trim().is_empty() && !entry.chars().any(char::is_control))
                .map(str::to_owned)
                .ok_or_else(|| {
                    "workspace.additional_dir entries must be non-empty path strings".to_owned()
                })
        })
        .collect()
}

fn empty_document() -> Value {
    Value::Table(toml::map::Map::new())
}

fn resolve_directory(base: &Path, path: &Path) -> Result<PathBuf, String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let metadata = fs::metadata(&path).map_err(|error| {
        format!(
            "additional directory {} is unavailable: {error}",
            path.display()
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "additional directory {} is not a directory",
            path.display()
        ));
    }
    fs::canonicalize(&path).map_err(|error| {
        format!(
            "could not resolve additional directory {}: {error}",
            path.display()
        )
    })
}

fn normalize_dirs(directories: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    directories
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn find_project_root(working_dir: &Path) -> PathBuf {
    let start = working_dir.to_path_buf();
    let mut current = start.clone();
    loop {
        if fs::symlink_metadata(current.join(".git")).is_ok() {
            return current;
        }
        let Some(parent) = current.parent() else {
            return start;
        };
        if parent == current {
            return start;
        }
        current = parent.to_path_buf();
    }
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("workspace local config {} has no parent", path.display()))?;
    ensure_private_directory(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(format!(
                "refusing to replace non-regular workspace local config {}",
                path.display()
            ));
        }
    }
    let temporary = parent.join(format!(".local.toml.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let write_result = (|| {
        let mut file = options
            .open(&temporary)
            .map_err(|error| format!("could not create {}: {error}", temporary.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
        fs::rename(&temporary, path).map_err(|error| {
            format!(
                "could not atomically replace {} from {}: {error}",
                path.display(),
                temporary.display()
            )
        })
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    if fs::symlink_metadata(path).is_err() {
        fs::create_dir_all(path)
            .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return Err(format!(
            "workspace config directory {} must be a real directory",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("could not protect {}: {error}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn remember_preserves_unknown_toml_and_loads_canonical_deduplicated_roots() {
        let temp = tempdir().expect("temp");
        let project = temp.path().join("project");
        let nested = project.join("src");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir_all(project.join(".git")).expect("git");
        fs::create_dir_all(project.join(".mycel")).expect("mycel");
        fs::create_dir_all(&nested).expect("nested");
        fs::create_dir_all(&first).expect("first");
        fs::create_dir_all(&second).expect("second");
        fs::write(
            project.join(".mycel/local.toml"),
            format!(
                "keep = 'value'\n[workspace]\nadditional_dir = [{}]\n",
                toml::Value::String(first.to_string_lossy().into_owned())
            ),
        )
        .expect("config");

        let first = fs::canonicalize(first).expect("canonical first");
        let second = fs::canonicalize(second).expect("canonical second");

        let remembered = remember_workspace_additional_dir(&nested, &second).expect("remember");
        assert_eq!(
            remembered.additional_dirs,
            vec![first.clone(), second.clone()]
        );
        let text = fs::read_to_string(&remembered.config_path).expect("read");
        assert!(text.contains("keep = \"value\""));
        let loaded = load_workspace_local_config(&nested).expect("load");
        assert_eq!(loaded.additional_dirs, vec![first, second]);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_config_and_non_directory_roots_fail_closed() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("temp");
        let project = temp.path().join("project");
        fs::create_dir_all(project.join(".git")).expect("git");
        fs::create_dir_all(project.join(".mycel")).expect("mycel");
        let outside = temp.path().join("outside.toml");
        fs::write(&outside, "[workspace]\nadditional_dir=[]\n").expect("outside");
        symlink(&outside, project.join(".mycel/local.toml")).expect("symlink");
        assert!(load_workspace_local_config(&project)
            .expect_err("symlink rejected")
            .contains("regular file"));

        fs::remove_file(project.join(".mycel/local.toml")).expect("remove symlink");
        let file = temp.path().join("not-a-dir");
        fs::write(&file, "x").expect("file");
        fs::write(
            project.join(".mycel/local.toml"),
            format!(
                "[workspace]\nadditional_dir=[{}]\n",
                toml::Value::String(file.to_string_lossy().into_owned())
            ),
        )
        .expect("config");
        assert!(load_workspace_local_config(&project)
            .expect_err("file root rejected")
            .contains("not a directory"));
    }
}
