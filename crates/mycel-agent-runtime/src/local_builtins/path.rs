use std::{
    ffi::OsString,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

use super::LocalToolConfig;

#[derive(Clone, Copy, Debug)]
pub(super) enum PathKind {
    File,
    Directory,
    WritableFile,
}

pub(super) fn resolve_local_path(
    config: &LocalToolConfig,
    input: &str,
    kind: PathKind,
) -> Result<PathBuf, String> {
    if input.is_empty() {
        return Err("path cannot be empty".to_owned());
    }
    let raw = Path::new(input);
    let candidate = normalize_absolute(if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        config.cwd().join(raw)
    })?;
    let candidate = real_parent_form(&candidate)?;
    if !matches!(kind, PathKind::Directory) && is_sensitive_path(&candidate) {
        return Err(format!("access to sensitive path {input:?} is denied"));
    }

    match kind {
        PathKind::File | PathKind::Directory => {
            let canonical = std::fs::canonicalize(&candidate)
                .map_err(|error| format!("cannot resolve {input:?}: {error}"))?;
            require_within_real_root(config, &canonical, input)?;
            let metadata = std::fs::metadata(&canonical)
                .map_err(|error| format!("cannot inspect {input:?}: {error}"))?;
            if matches!(kind, PathKind::File) && !metadata.is_file() {
                return Err(format!("{input:?} is not a file"));
            }
            if matches!(kind, PathKind::Directory) && !metadata.is_dir() {
                return Err(format!("{input:?} is not a directory"));
            }
            Ok(canonical)
        }
        PathKind::WritableFile => {
            let lexical_root = config.roots().find(|root| candidate.starts_with(root));
            let allowed_file = config
                .allowed_writable_files()
                .iter()
                .find(|allowed| allowed.as_path() == candidate);
            let authorization_root = match (lexical_root, allowed_file) {
                (Some(root), _) => root,
                (None, Some(allowed)) => allowed
                    .parent()
                    .ok_or_else(|| format!("allowed path {input:?} has no parent"))?,
                (None, None) => {
                    return Err(format!(
                        "path {input:?} is outside the configured workspace roots and exact file grants"
                    ))
                }
            };
            reject_symlink_components(authorization_root, &candidate, input)?;
            match std::fs::symlink_metadata(&candidate) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() {
                        return Err(format!("refusing to write through symlink {input:?}"));
                    }
                    if !metadata.is_file() {
                        return Err(format!("{input:?} is not a file"));
                    }
                    let canonical = std::fs::canonicalize(&candidate)
                        .map_err(|error| format!("cannot resolve {input:?}: {error}"))?;
                    require_writable_authorization(config, &canonical, input)?;
                    Ok(canonical)
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    let ancestor = nearest_existing_ancestor(&candidate)?;
                    let canonical_ancestor = std::fs::canonicalize(&ancestor)
                        .map_err(|error| format!("cannot resolve parent of {input:?}: {error}"))?;
                    if allowed_file.is_none() {
                        require_within_real_root(config, &canonical_ancestor, input)?;
                    } else if candidate.parent() != Some(canonical_ancestor.as_path()) {
                        return Err(format!("allowed path {input:?} has an unexpected parent"));
                    }
                    Ok(candidate)
                }
                Err(error) => Err(format!("cannot inspect {input:?}: {error}")),
            }
        }
    }
}

fn require_writable_authorization(
    config: &LocalToolConfig,
    canonical: &Path,
    input: &str,
) -> Result<(), String> {
    if config.roots().any(|root| canonical.starts_with(root))
        || config
            .allowed_writable_files()
            .iter()
            .any(|allowed| allowed == canonical)
    {
        Ok(())
    } else {
        Err(format!(
            "path {input:?} resolves outside the configured workspace roots and exact file grants"
        ))
    }
}

fn real_parent_form(path: &Path) -> Result<PathBuf, String> {
    let ancestor = nearest_existing_ancestor(path)?;
    let canonical = std::fs::canonicalize(&ancestor)
        .map_err(|error| format!("cannot resolve parent {ancestor:?}: {error}"))?;
    let suffix = path
        .strip_prefix(&ancestor)
        .map_err(|_| format!("cannot resolve path {path:?}"))?;
    Ok(canonical.join(suffix))
}

fn normalize_absolute(path: PathBuf) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("path {path:?} is not absolute after resolution"));
    }
    let mut parts: Vec<OsString> = Vec::new();
    let mut prefix = None;
    for component in path.components() {
        match component {
            Component::Prefix(value) => prefix = Some(value.as_os_str().to_owned()),
            Component::RootDir | Component::CurDir => {}
            Component::Normal(value) => parts.push(value.to_owned()),
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(format!("path {path:?} escapes the filesystem root"));
                }
            }
        }
    }
    let mut normalized = PathBuf::new();
    if let Some(prefix) = prefix {
        normalized.push(prefix);
    }
    normalized.push(std::path::MAIN_SEPARATOR.to_string());
    normalized.extend(parts);
    Ok(normalized)
}

fn require_within_real_root(
    config: &LocalToolConfig,
    canonical: &Path,
    input: &str,
) -> Result<(), String> {
    if config.roots().any(|root| canonical.starts_with(root)) {
        Ok(())
    } else {
        Err(format!(
            "path {input:?} resolves outside the configured workspace roots"
        ))
    }
}

fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf, String> {
    let mut current = path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("path {path:?} does not have a writable parent directory"))?;
    loop {
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(format!("parent path {current:?} is not a directory"));
                }
                return Ok(current);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if !current.pop() {
                    return Err(format!("no existing parent for {path:?}"));
                }
            }
            Err(error) => return Err(format!("cannot inspect parent {current:?}: {error}")),
        }
    }
}

fn reject_symlink_components(root: &Path, candidate: &Path, input: &str) -> Result<(), String> {
    let relative = candidate
        .strip_prefix(root)
        .map_err(|_| format!("path {input:?} is outside its workspace root"))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!("refusing to write through symlink {input:?}"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => break,
            Err(error) => return Err(format!("cannot inspect {current:?}: {error}")),
        }
    }
    Ok(())
}

pub(super) fn is_sensitive_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        ".env.example" | ".env.sample" | ".env.template"
    ) || matches!(
        lower.as_str(),
        "id_rsa.pub" | "id_ed25519.pub" | "id_ecdsa.pub"
    ) {
        return false;
    }
    if lower == ".env" || lower.starts_with(".env.") {
        return true;
    }
    const PREFIXES: [&str; 4] = ["id_rsa", "id_ed25519", "id_ecdsa", "credentials"];
    const DOT_VARIANTS: [&str; 10] = [
        ".bak",
        ".backup",
        ".copy",
        ".disabled",
        ".key",
        ".old",
        ".orig",
        ".pem",
        ".save",
        ".tmp",
    ];
    for prefix in PREFIXES {
        if lower == prefix {
            return true;
        }
        if let Some(suffix) = lower.strip_prefix(prefix) {
            if suffix.starts_with(['-', '_']) || DOT_VARIANTS.contains(&suffix) {
                return true;
            }
        }
    }

    let comparable_path = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    [".aws/credentials", ".gcp/credentials"]
        .iter()
        .any(|suffix| {
            comparable_path.ends_with(&format!("/{suffix}"))
                || comparable_path.contains(&format!("/{suffix}/"))
        })
}
