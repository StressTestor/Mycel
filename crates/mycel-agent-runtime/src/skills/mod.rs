//! Local, bounded skill discovery and prompt activation.
//!
//! Skills are data, not executable extensions. A registry scans explicitly
//! configured local roots for `SKILL.md`, parses a deliberately small
//! frontmatter subset, and produces escaped prompt blocks. Network access and
//! remote acquisition do not belong in this module.

use regex::{Captures, Regex};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use thiserror::Error;

const SKILL_FILE: &str = "SKILL.md";

/// Origin class used to resolve an un-namespaced collision.
///
/// Later variants have higher precedence: a project skill may intentionally
/// shadow a user, extra, or built-in skill. Equal-precedence roots are ordered
/// by canonical path and the lexically first candidate wins.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SkillSource {
    Builtin,
    Extra,
    User,
    Project,
}

impl fmt::Display for SkillSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Builtin => "builtin",
            Self::Extra => "extra",
            Self::User => "user",
            Self::Project => "project",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillRoot {
    pub path: PathBuf,
    pub source: SkillSource,
    /// Optional namespace for extension-owned skills, for example
    /// `example:review`. The namespace itself must use the skill-name grammar.
    pub namespace: Option<String>,
}

impl SkillRoot {
    pub fn builtin(path: impl Into<PathBuf>) -> Self {
        Self::new(path, SkillSource::Builtin, None)
    }

    pub fn extra(path: impl Into<PathBuf>) -> Self {
        Self::new(path, SkillSource::Extra, None)
    }

    pub fn namespaced_extra(path: impl Into<PathBuf>, namespace: impl Into<String>) -> Self {
        Self::new(path, SkillSource::Extra, Some(namespace.into()))
    }

    pub fn user(path: impl Into<PathBuf>) -> Self {
        Self::new(path, SkillSource::User, None)
    }

    pub fn project(path: impl Into<PathBuf>) -> Self {
        Self::new(path, SkillSource::Project, None)
    }

    fn new(path: impl Into<PathBuf>, source: SkillSource, namespace: Option<String>) -> Self {
        Self {
            path: path.into(),
            source,
            namespace,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SkillScanLimits {
    pub max_depth: usize,
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for SkillScanLimits {
    fn default() -> Self {
        Self {
            max_depth: 8,
            max_files: 4_096,
            max_file_bytes: 1024 * 1024,
            max_total_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileMetadata {
    pub is_file: bool,
    pub is_dir: bool,
    pub len: u64,
}

/// Filesystem boundary used by both production discovery and deterministic
/// tests. Implementations must return raw directory entries; the scanner sorts
/// them before making any precedence decision.
pub trait SkillFileSystem: Send + Sync {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    fn metadata(&self, path: &Path) -> io::Result<FileMetadata>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    fn read_bounded(&self, path: &Path, max_bytes: u64) -> io::Result<Vec<u8>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StdSkillFileSystem;

impl SkillFileSystem for StdSkillFileSystem {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        let metadata = std::fs::metadata(path)?;
        Ok(FileMetadata {
            is_file: metadata.is_file(),
            is_dir: metadata.is_dir(),
            len: metadata.len(),
        })
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect()
    }

    fn read_bounded(&self, path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
        let file = File::open(path)?;
        let mut bytes = Vec::new();
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "skill file exceeds configured byte limit",
            ));
        }
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillKind {
    Prompt,
    Inline,
    Flow,
    Reference,
}

impl SkillKind {
    fn parse(value: Option<&str>) -> Result<Self, SkillParseError> {
        match value
            .unwrap_or("prompt")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "prompt" => Ok(Self::Prompt),
            "inline" => Ok(Self::Inline),
            "flow" => Ok(Self::Flow),
            "reference" => Ok(Self::Reference),
            other => Err(SkillParseError::InvalidType(other.to_owned())),
        }
    }
}

impl fmt::Display for SkillKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Prompt => "prompt",
            Self::Inline => "inline",
            Self::Flow => "flow",
            Self::Reference => "reference",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub kind: SkillKind,
    pub when_to_use: Option<String>,
    pub disable_model_invocation: bool,
    pub has_sub_skills: bool,
    pub safe: bool,
    pub arguments: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillDefinition {
    /// Registry key after namespace and sub-skill qualification.
    pub id: String,
    pub metadata: SkillMetadata,
    pub body: String,
    pub source: SkillSource,
    pub root: PathBuf,
    pub directory: PathBuf,
    pub file: PathBuf,
    pub is_sub_skill: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillDiagnosticLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillDiagnosticCode {
    MissingRoot,
    InvalidRoot,
    InvalidNamespace,
    EscapesRoot,
    Io,
    DepthLimit,
    FileLimit,
    FileTooLarge,
    TotalBytesLimit,
    InvalidUtf8,
    InvalidFrontmatter,
    Duplicate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillDiagnostic {
    pub level: SkillDiagnosticLevel,
    pub code: SkillDiagnosticCode,
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillCatalog {
    skills: BTreeMap<String, SkillDefinition>,
    diagnostics: Vec<SkillDiagnostic>,
}

impl SkillCatalog {
    pub fn get(&self, id: &str) -> Option<&SkillDefinition> {
        self.skills.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &SkillDefinition)> {
        self.skills.iter().map(|(id, skill)| (id.as_str(), skill))
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn diagnostics(&self) -> &[SkillDiagnostic] {
        &self.diagnostics
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillReload {
    pub loaded: usize,
    pub diagnostics: Vec<SkillDiagnostic>,
}

/// Reloadable local skill registry. Reload constructs a complete new snapshot
/// before replacing the visible catalog.
pub struct SkillRegistry<F: SkillFileSystem = StdSkillFileSystem> {
    fs: Arc<F>,
    roots: Vec<SkillRoot>,
    limits: SkillScanLimits,
    catalog: SkillCatalog,
}

impl SkillRegistry<StdSkillFileSystem> {
    pub fn local(roots: Vec<SkillRoot>, limits: SkillScanLimits) -> Self {
        Self::new(Arc::new(StdSkillFileSystem), roots, limits)
    }
}

impl<F: SkillFileSystem> SkillRegistry<F> {
    pub fn new(fs: Arc<F>, roots: Vec<SkillRoot>, limits: SkillScanLimits) -> Self {
        Self {
            fs,
            roots,
            limits,
            catalog: SkillCatalog::default(),
        }
    }

    pub fn catalog(&self) -> &SkillCatalog {
        &self.catalog
    }

    pub fn reload(&mut self) -> SkillReload {
        let catalog = scan_skills(self.fs.as_ref(), &self.roots, self.limits);
        let result = SkillReload {
            loaded: catalog.len(),
            diagnostics: catalog.diagnostics.clone(),
        };
        self.catalog = catalog;
        result
    }

    pub fn activate(
        &self,
        id: &str,
        arguments: &[String],
        trigger: SkillTrigger,
        session_id: &str,
    ) -> Result<SkillActivation, SkillActivationError> {
        let skill = self
            .catalog
            .get(id)
            .ok_or_else(|| SkillActivationError::NotFound(id.to_owned()))?;
        ensure_activatable(skill, trigger)?;

        let expanded = expand_placeholders(skill, arguments, session_id);
        let rewritten = rewrite_media_placeholders(&expanded);
        let joined_arguments = arguments.join(" ");
        let prompt = format!(
            "<mycel-skill-loaded name=\"{}\" type=\"{}\" trigger=\"{}\" source=\"{}\" directory=\"{}\" arguments=\"{}\">\n{}\n</mycel-skill-loaded>",
            escape_xml_attribute(&skill.id),
            skill.metadata.kind,
            trigger,
            skill.source,
            escape_xml_attribute(&skill.directory.to_string_lossy()),
            escape_xml_attribute(&joined_arguments),
            escape_xml_text(&rewritten),
        );
        Ok(SkillActivation {
            id: skill.id.clone(),
            kind: skill.metadata.kind,
            trigger,
            prompt,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillTrigger {
    UserSlash,
    ModelTool,
    NestedSkill,
}

impl fmt::Display for SkillTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::UserSlash => "user",
            Self::ModelTool => "model",
            Self::NestedSkill => "nested",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillActivation {
    pub id: String,
    pub kind: SkillKind,
    pub trigger: SkillTrigger,
    pub prompt: String,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SkillActivationError {
    #[error("skill not found: {0}")]
    NotFound(String),
    #[error("reference skill cannot be activated: {0}")]
    Reference(String),
    #[error("flow skill can only be activated by the user: {0}")]
    FlowRequiresUser(String),
    #[error("model invocation is disabled for skill: {0}")]
    ModelInvocationDisabled(String),
}

#[derive(Debug, Error, Eq, PartialEq)]
enum SkillParseError {
    #[error("SKILL.md must begin with a frontmatter delimiter")]
    MissingFrontmatter,
    #[error("SKILL.md frontmatter has no closing delimiter")]
    UnterminatedFrontmatter,
    #[error("frontmatter line is not a key/value pair: {0}")]
    MalformedLine(String),
    #[error("required frontmatter field is missing: {0}")]
    MissingField(&'static str),
    #[error("invalid skill name: {0}")]
    InvalidName(String),
    #[error("unsupported skill type: {0}")]
    InvalidType(String),
    #[error("invalid boolean for {0}: {1}")]
    InvalidBoolean(String, String),
    #[error("invalid arguments list")]
    InvalidArguments,
    #[error("skill body is empty")]
    EmptyBody,
}

#[derive(Clone, Debug)]
struct ParsedSkill {
    metadata: SkillMetadata,
    body: String,
}

#[derive(Clone, Debug)]
enum FrontmatterValue {
    Scalar(String),
    List(Vec<String>),
}

#[derive(Clone, Debug)]
struct Candidate {
    definition: SkillDefinition,
    canonical_file: PathBuf,
}

struct ScanState<'a, F: SkillFileSystem> {
    fs: &'a F,
    limits: SkillScanLimits,
    files_seen: usize,
    total_bytes: u64,
    visited_dirs: BTreeSet<PathBuf>,
    candidates: Vec<Candidate>,
    diagnostics: Vec<SkillDiagnostic>,
}

fn scan_skills<F: SkillFileSystem>(
    fs: &F,
    roots: &[SkillRoot],
    limits: SkillScanLimits,
) -> SkillCatalog {
    let mut state = ScanState {
        fs,
        limits,
        files_seen: 0,
        total_bytes: 0,
        visited_dirs: BTreeSet::new(),
        candidates: Vec::new(),
        diagnostics: Vec::new(),
    };

    let mut roots = roots.to_vec();
    roots.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.namespace.cmp(&right.namespace))
    });
    for root in roots {
        state.visited_dirs.clear();
        if let Some(namespace) = root.namespace.as_deref() {
            if !valid_name(namespace) {
                state.push(
                    SkillDiagnosticLevel::Error,
                    SkillDiagnosticCode::InvalidNamespace,
                    root.path.clone(),
                    format!("invalid skill namespace: {namespace}"),
                );
                continue;
            }
        }
        let canonical_root = match fs.canonicalize(&root.path) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                state.push(
                    SkillDiagnosticLevel::Info,
                    SkillDiagnosticCode::MissingRoot,
                    root.path.clone(),
                    "skill root does not exist".to_owned(),
                );
                continue;
            }
            Err(error) => {
                state.push(
                    SkillDiagnosticLevel::Error,
                    SkillDiagnosticCode::InvalidRoot,
                    root.path.clone(),
                    format!("cannot canonicalize skill root: {error}"),
                );
                continue;
            }
        };
        match fs.metadata(&canonical_root) {
            Ok(metadata) if metadata.is_dir => {}
            Ok(_) => {
                state.push(
                    SkillDiagnosticLevel::Error,
                    SkillDiagnosticCode::InvalidRoot,
                    root.path.clone(),
                    "skill root is not a directory".to_owned(),
                );
                continue;
            }
            Err(error) => {
                state.push(
                    SkillDiagnosticLevel::Error,
                    SkillDiagnosticCode::Io,
                    root.path.clone(),
                    format!("cannot inspect skill root: {error}"),
                );
                continue;
            }
        }
        state.walk_directory(&root, &canonical_root, &canonical_root, 0, None);
    }

    state.candidates.sort_by(|left, right| {
        left.definition
            .source
            .cmp(&right.definition.source)
            .then_with(|| left.canonical_file.cmp(&right.canonical_file))
            .then_with(|| left.definition.id.cmp(&right.definition.id))
    });

    let mut selected: BTreeMap<String, Candidate> = BTreeMap::new();
    for candidate in std::mem::take(&mut state.candidates) {
        match selected.get(&candidate.definition.id) {
            None => {
                selected.insert(candidate.definition.id.clone(), candidate);
            }
            Some(previous) if candidate.definition.source > previous.definition.source => {
                let previous_path = previous.canonical_file.clone();
                let id = candidate.definition.id.clone();
                state.push(
                    SkillDiagnosticLevel::Warning,
                    SkillDiagnosticCode::Duplicate,
                    previous_path,
                    format!("skill {id} was shadowed by a higher-precedence source"),
                );
                selected.insert(id, candidate);
            }
            Some(_) => {
                state.push(
                    SkillDiagnosticLevel::Warning,
                    SkillDiagnosticCode::Duplicate,
                    candidate.canonical_file.clone(),
                    format!(
                        "duplicate skill {} ignored; deterministic first candidate retained",
                        candidate.definition.id
                    ),
                );
            }
        }
    }

    SkillCatalog {
        skills: selected
            .into_iter()
            .map(|(id, candidate)| (id, candidate.definition))
            .collect(),
        diagnostics: state.diagnostics,
    }
}

impl<F: SkillFileSystem> ScanState<'_, F> {
    fn walk_directory(
        &mut self,
        root: &SkillRoot,
        canonical_root: &Path,
        path: &Path,
        depth: usize,
        parent_id: Option<&str>,
    ) {
        if depth > self.limits.max_depth {
            self.push(
                SkillDiagnosticLevel::Warning,
                SkillDiagnosticCode::DepthLimit,
                path.to_path_buf(),
                "skill scan depth limit reached".to_owned(),
            );
            return;
        }
        let canonical_dir = match self.confined(canonical_root, path) {
            Some(path) => path,
            None => return,
        };
        if !self.visited_dirs.insert(canonical_dir.clone()) {
            return;
        }

        let skill_path = canonical_dir.join(SKILL_FILE);
        let has_skill = self
            .fs
            .metadata(&skill_path)
            .map(|metadata| metadata.is_file)
            .unwrap_or(false);
        if has_skill {
            if let Some(skill) =
                self.read_skill(root, canonical_root, &canonical_dir, &skill_path, parent_id)
            {
                let recurse = skill.definition.metadata.has_sub_skills;
                let full_id = skill.definition.id.clone();
                self.candidates.push(skill);
                if recurse {
                    for child in self.child_directories(canonical_root, &canonical_dir) {
                        self.walk_directory(
                            root,
                            canonical_root,
                            &child,
                            depth.saturating_add(1),
                            Some(&full_id),
                        );
                    }
                }
            }
            return;
        }

        for child in self.child_directories(canonical_root, &canonical_dir) {
            self.walk_directory(
                root,
                canonical_root,
                &child,
                depth.saturating_add(1),
                parent_id,
            );
        }
    }

    fn child_directories(&mut self, canonical_root: &Path, path: &Path) -> Vec<PathBuf> {
        let mut children = match self.fs.read_dir(path) {
            Ok(children) => children,
            Err(error) => {
                self.push(
                    SkillDiagnosticLevel::Warning,
                    SkillDiagnosticCode::Io,
                    path.to_path_buf(),
                    format!("cannot read skill directory: {error}"),
                );
                return Vec::new();
            }
        };
        children.sort();
        children
            .into_iter()
            .filter(|child| {
                let Some(canonical) = self.confined(canonical_root, child) else {
                    return false;
                };
                self.fs
                    .metadata(&canonical)
                    .map(|metadata| metadata.is_dir)
                    .unwrap_or(false)
            })
            .collect()
    }

    fn read_skill(
        &mut self,
        root: &SkillRoot,
        canonical_root: &Path,
        directory: &Path,
        path: &Path,
        parent_id: Option<&str>,
    ) -> Option<Candidate> {
        if self.files_seen >= self.limits.max_files {
            self.push(
                SkillDiagnosticLevel::Error,
                SkillDiagnosticCode::FileLimit,
                path.to_path_buf(),
                "skill file count limit reached".to_owned(),
            );
            return None;
        }
        self.files_seen += 1;
        let canonical_file = self.confined(canonical_root, path)?;
        let metadata = match self.fs.metadata(&canonical_file) {
            Ok(metadata) if metadata.is_file => metadata,
            Ok(_) => return None,
            Err(error) => {
                self.push(
                    SkillDiagnosticLevel::Warning,
                    SkillDiagnosticCode::Io,
                    canonical_file,
                    format!("cannot inspect skill file: {error}"),
                );
                return None;
            }
        };
        if metadata.len > self.limits.max_file_bytes {
            self.push(
                SkillDiagnosticLevel::Error,
                SkillDiagnosticCode::FileTooLarge,
                canonical_file,
                format!(
                    "skill file is {} bytes; limit is {}",
                    metadata.len, self.limits.max_file_bytes
                ),
            );
            return None;
        }
        if self.total_bytes.saturating_add(metadata.len) > self.limits.max_total_bytes {
            self.push(
                SkillDiagnosticLevel::Error,
                SkillDiagnosticCode::TotalBytesLimit,
                canonical_file,
                "aggregate skill byte limit reached".to_owned(),
            );
            return None;
        }
        self.total_bytes = self.total_bytes.saturating_add(metadata.len);
        let bytes = match self
            .fs
            .read_bounded(&canonical_file, self.limits.max_file_bytes)
        {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                self.push(
                    SkillDiagnosticLevel::Error,
                    SkillDiagnosticCode::FileTooLarge,
                    canonical_file,
                    error.to_string(),
                );
                return None;
            }
            Err(error) => {
                self.push(
                    SkillDiagnosticLevel::Warning,
                    SkillDiagnosticCode::Io,
                    canonical_file,
                    format!("cannot read skill file: {error}"),
                );
                return None;
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                self.push(
                    SkillDiagnosticLevel::Error,
                    SkillDiagnosticCode::InvalidUtf8,
                    canonical_file,
                    "skill file is not UTF-8".to_owned(),
                );
                return None;
            }
        };
        let parsed = match parse_skill(&text) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.push(
                    SkillDiagnosticLevel::Error,
                    SkillDiagnosticCode::InvalidFrontmatter,
                    canonical_file,
                    error.to_string(),
                );
                return None;
            }
        };
        let id = if let Some(parent) = parent_id {
            format!("{parent}.{}", parsed.metadata.name)
        } else if let Some(namespace) = root.namespace.as_deref() {
            format!("{namespace}:{}", parsed.metadata.name)
        } else {
            parsed.metadata.name.clone()
        };
        Some(Candidate {
            definition: SkillDefinition {
                id,
                metadata: parsed.metadata,
                body: parsed.body,
                source: root.source,
                root: canonical_root.to_path_buf(),
                directory: directory.to_path_buf(),
                file: canonical_file.clone(),
                is_sub_skill: parent_id.is_some(),
            },
            canonical_file,
        })
    }

    fn confined(&mut self, root: &Path, path: &Path) -> Option<PathBuf> {
        match self.fs.canonicalize(path) {
            Ok(canonical) if canonical.starts_with(root) => Some(canonical),
            Ok(canonical) => {
                self.push(
                    SkillDiagnosticLevel::Error,
                    SkillDiagnosticCode::EscapesRoot,
                    path.to_path_buf(),
                    format!(
                        "skill path resolves outside root: {}",
                        canonical.to_string_lossy()
                    ),
                );
                None
            }
            Err(error) => {
                self.push(
                    SkillDiagnosticLevel::Warning,
                    SkillDiagnosticCode::Io,
                    path.to_path_buf(),
                    format!("cannot canonicalize skill path: {error}"),
                );
                None
            }
        }
    }

    fn push(
        &mut self,
        level: SkillDiagnosticLevel,
        code: SkillDiagnosticCode,
        path: PathBuf,
        message: String,
    ) {
        self.diagnostics.push(SkillDiagnostic {
            level,
            code,
            path,
            message,
        });
    }
}

fn parse_skill(input: &str) -> Result<ParsedSkill, SkillParseError> {
    let normalized = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut lines = normalized.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Err(SkillParseError::MissingFrontmatter);
    }
    let mut frontmatter_lines = Vec::new();
    let mut found_end = false;
    let mut body_lines = Vec::new();
    for line in lines.by_ref() {
        if line.trim() == "---" {
            found_end = true;
            break;
        }
        frontmatter_lines.push(line);
    }
    if !found_end {
        return Err(SkillParseError::UnterminatedFrontmatter);
    }
    body_lines.extend(lines);
    let body = body_lines.join("\n").trim().to_owned();
    if body.is_empty() {
        return Err(SkillParseError::EmptyBody);
    }
    let fields = parse_frontmatter(&frontmatter_lines)?;
    let name = scalar(&fields, "name")
        .ok_or(SkillParseError::MissingField("name"))?
        .trim()
        .to_owned();
    if !valid_name(&name) {
        return Err(SkillParseError::InvalidName(name));
    }
    let description = scalar(&fields, "description")
        .ok_or(SkillParseError::MissingField("description"))?
        .trim()
        .to_owned();
    if description.is_empty() {
        return Err(SkillParseError::MissingField("description"));
    }
    let kind = SkillKind::parse(scalar(&fields, "type"))?;
    let disable_model_invocation = parse_bool_field(
        &fields,
        &["disable-model-invocation", "disable_model_invocation"],
        false,
    )?;
    let has_sub_skills = parse_bool_field(
        &fields,
        &[
            "has-sub-skill",
            "has-sub-skills",
            "has_sub_skill",
            "has_sub_skills",
        ],
        false,
    )?;
    let safe = parse_bool_field(&fields, &["safe"], false)?;
    let when_to_use = ["when-to-use", "when_to_use"]
        .into_iter()
        .find_map(|key| scalar(&fields, key).map(str::to_owned));
    let arguments = match fields.get("arguments") {
        None => Vec::new(),
        Some(FrontmatterValue::List(values)) => values.clone(),
        Some(FrontmatterValue::Scalar(value)) if value.trim().is_empty() => Vec::new(),
        Some(FrontmatterValue::Scalar(value)) => parse_inline_list(value)?,
    };
    if arguments.iter().any(|argument| !valid_name(argument)) {
        return Err(SkillParseError::InvalidArguments);
    }

    Ok(ParsedSkill {
        metadata: SkillMetadata {
            name,
            description,
            kind,
            when_to_use,
            disable_model_invocation,
            has_sub_skills,
            safe,
            arguments,
        },
        body,
    })
}

fn parse_frontmatter(
    lines: &[&str],
) -> Result<BTreeMap<String, FrontmatterValue>, SkillParseError> {
    let mut fields = BTreeMap::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        index += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, raw_value)) = trimmed.split_once(':') else {
            return Err(SkillParseError::MalformedLine(trimmed.to_owned()));
        };
        let key = key.trim().to_ascii_lowercase();
        if key.is_empty() {
            return Err(SkillParseError::MalformedLine(trimmed.to_owned()));
        }
        let raw_value = raw_value.trim();
        if raw_value.is_empty() {
            let mut values = Vec::new();
            while index < lines.len() {
                let candidate = lines[index].trim();
                if let Some(value) = candidate.strip_prefix('-') {
                    values.push(unquote(value.trim()));
                    index += 1;
                } else {
                    break;
                }
            }
            fields.insert(key, FrontmatterValue::List(values));
        } else {
            fields.insert(key, FrontmatterValue::Scalar(unquote(raw_value)));
        }
    }
    Ok(fields)
}

fn scalar<'a>(fields: &'a BTreeMap<String, FrontmatterValue>, key: &str) -> Option<&'a str> {
    match fields.get(key) {
        Some(FrontmatterValue::Scalar(value)) => Some(value),
        _ => None,
    }
}

fn parse_bool_field(
    fields: &BTreeMap<String, FrontmatterValue>,
    keys: &[&str],
    default: bool,
) -> Result<bool, SkillParseError> {
    let Some((key, value)) = keys
        .iter()
        .find_map(|key| scalar(fields, key).map(|value| ((*key).to_owned(), value)))
    else {
        return Ok(default);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(SkillParseError::InvalidBoolean(key, value.to_owned())),
    }
}

fn parse_inline_list(value: &str) -> Result<Vec<String>, SkillParseError> {
    let value = value.trim();
    let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return Ok(vec![unquote(value)]);
    };
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(inner.split(',').map(|part| unquote(part.trim())).collect())
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().enumerate().all(|(index, byte)| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => true,
            b'_' | b'-' | b'.' => index > 0,
            _ => false,
        })
}

fn ensure_activatable(
    skill: &SkillDefinition,
    trigger: SkillTrigger,
) -> Result<(), SkillActivationError> {
    if skill.metadata.kind == SkillKind::Reference {
        return Err(SkillActivationError::Reference(skill.id.clone()));
    }
    if skill.metadata.kind == SkillKind::Flow && trigger != SkillTrigger::UserSlash {
        return Err(SkillActivationError::FlowRequiresUser(skill.id.clone()));
    }
    if trigger != SkillTrigger::UserSlash && skill.metadata.disable_model_invocation {
        return Err(SkillActivationError::ModelInvocationDisabled(
            skill.id.clone(),
        ));
    }
    Ok(())
}

fn expand_placeholders(skill: &SkillDefinition, arguments: &[String], session_id: &str) -> String {
    static INDEXED: OnceLock<Regex> = OnceLock::new();
    static POSITIONAL: OnceLock<Regex> = OnceLock::new();
    let joined = arguments.join(" ");
    let mut output = skill.body.clone();
    let indexed =
        INDEXED.get_or_init(|| Regex::new(r"\$ARGUMENTS\[(\d+)\]").expect("static regex"));
    output = indexed
        .replace_all(&output, |captures: &Captures<'_>| {
            captures
                .get(1)
                .and_then(|index| index.as_str().parse::<usize>().ok())
                .and_then(|index| arguments.get(index))
                .map(String::as_str)
                .unwrap_or("")
                .to_owned()
        })
        .into_owned();
    output = output.replace("$ARGUMENTS", &joined);
    let positional = POSITIONAL.get_or_init(|| Regex::new(r"\$(\d+)\b").expect("static regex"));
    output = positional
        .replace_all(&output, |captures: &Captures<'_>| {
            captures
                .get(1)
                .and_then(|index| index.as_str().parse::<usize>().ok())
                .and_then(|index| arguments.get(index))
                .map(String::as_str)
                .unwrap_or("")
                .to_owned()
        })
        .into_owned();
    for (index, name) in skill.metadata.arguments.iter().enumerate() {
        output = output.replace(
            &format!("${name}"),
            arguments.get(index).map(String::as_str).unwrap_or(""),
        );
    }
    output
        .replace(
            "${MYCEL_SKILL_DIR}",
            skill.directory.to_string_lossy().as_ref(),
        )
        .replace("${MYCEL_SESSION_ID}", session_id)
}

/// Removes local media paths from model-visible Markdown while retaining a
/// stable semantic placeholder. This prevents a skill from smuggling large
/// media payloads or host-specific paths into the prompt.
pub fn rewrite_media_placeholders(input: &str) -> String {
    static MARKDOWN_MEDIA: OnceLock<Regex> = OnceLock::new();
    static HTML_MEDIA: OnceLock<Regex> = OnceLock::new();
    let markdown = MARKDOWN_MEDIA.get_or_init(|| {
        Regex::new(r#"!\[([^\]]*)\]\(([^\s\)]+)(?:\s+[\"'][^\)]*)?\)"#).expect("static regex")
    });
    let output = markdown.replace_all(input, |captures: &Captures<'_>| {
        let alt = captures.get(1).map(|value| value.as_str()).unwrap_or("");
        let target = captures.get(2).map(|value| value.as_str()).unwrap_or("");
        let kind = media_kind(target);
        if alt.trim().is_empty() {
            format!("[{kind}]")
        } else {
            format!("[{kind}: {}]", alt.trim())
        }
    });
    let html = HTML_MEDIA.get_or_init(|| {
        Regex::new(r"(?is)<\s*(/\s*)?(img|video|audio)\b[^>]*>").expect("static regex")
    });
    html.replace_all(&output, |captures: &Captures<'_>| {
        if captures.get(1).is_some() {
            String::new()
        } else {
            format!(
                "[{}]",
                captures
                    .get(2)
                    .map(|value| value.as_str().to_ascii_lowercase())
                    .unwrap_or_else(|| "media".to_owned())
            )
        }
    })
    .into_owned()
}

fn media_kind(target: &str) -> &'static str {
    let target = target
        .split(['?', '#'])
        .next()
        .unwrap_or(target)
        .to_ascii_lowercase();
    if [".mp3", ".wav", ".m4a", ".aac", ".flac", ".ogg"]
        .iter()
        .any(|extension| target.ends_with(extension))
    {
        "audio"
    } else if [".mp4", ".mov", ".webm", ".mkv", ".avi"]
        .iter()
        .any(|extension| target.ends_with(extension))
    {
        "video"
    } else {
        "image"
    }
}

fn escape_xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_xml_attribute(value: &str) -> String {
    escape_xml_text(value)
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryFs {
        nodes: Mutex<BTreeMap<PathBuf, MemoryNode>>,
        aliases: Mutex<BTreeMap<PathBuf, PathBuf>>,
    }

    #[derive(Clone)]
    enum MemoryNode {
        Directory,
        File(Vec<u8>),
    }

    impl MemoryFs {
        fn directory(&self, path: &str) {
            self.nodes
                .lock()
                .expect("nodes")
                .insert(PathBuf::from(path), MemoryNode::Directory);
        }

        fn file(&self, path: &str, content: impl Into<Vec<u8>>) {
            self.nodes
                .lock()
                .expect("nodes")
                .insert(PathBuf::from(path), MemoryNode::File(content.into()));
        }

        fn remove(&self, path: &str) {
            self.nodes.lock().expect("nodes").remove(Path::new(path));
        }

        fn alias(&self, from: &str, to: &str) {
            self.aliases
                .lock()
                .expect("aliases")
                .insert(PathBuf::from(from), PathBuf::from(to));
        }

        fn resolved(&self, path: &Path) -> PathBuf {
            self.aliases
                .lock()
                .expect("aliases")
                .get(path)
                .cloned()
                .unwrap_or_else(|| path.to_path_buf())
        }
    }

    impl SkillFileSystem for MemoryFs {
        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            let path = self.resolved(path);
            if self.nodes.lock().expect("nodes").contains_key(&path) {
                Ok(path)
            } else {
                Err(io::Error::new(io::ErrorKind::NotFound, "missing"))
            }
        }

        fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
            match self.nodes.lock().expect("nodes").get(path) {
                Some(MemoryNode::Directory) => Ok(FileMetadata {
                    is_file: false,
                    is_dir: true,
                    len: 0,
                }),
                Some(MemoryNode::File(bytes)) => Ok(FileMetadata {
                    is_file: true,
                    is_dir: false,
                    len: bytes.len() as u64,
                }),
                None => Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
            }
        }

        fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
            let nodes = self.nodes.lock().expect("nodes");
            if !matches!(nodes.get(path), Some(MemoryNode::Directory)) {
                return Err(io::Error::new(io::ErrorKind::NotFound, "missing"));
            }
            Ok(nodes
                .keys()
                .filter(|candidate| candidate.parent() == Some(path))
                .cloned()
                .collect())
        }

        fn read_bounded(&self, path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
            match self.nodes.lock().expect("nodes").get(path) {
                Some(MemoryNode::File(bytes)) if bytes.len() as u64 <= max_bytes => {
                    Ok(bytes.clone())
                }
                Some(MemoryNode::File(_)) => {
                    Err(io::Error::new(io::ErrorKind::InvalidData, "too large"))
                }
                _ => Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
            }
        }
    }

    fn skill(name: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: test skill\n---\n{body}\n")
    }

    fn root(path: &str, source: SkillSource) -> SkillRoot {
        SkillRoot {
            path: PathBuf::from(path),
            source,
            namespace: None,
        }
    }

    #[test]
    fn precedence_and_duplicate_order_are_deterministic() {
        let fs = Arc::new(MemoryFs::default());
        for path in [
            "/builtin",
            "/builtin/z",
            "/builtin/a",
            "/project",
            "/project/x",
        ] {
            fs.directory(path);
        }
        fs.file("/builtin/z/SKILL.md", skill("review", "z"));
        fs.file("/builtin/a/SKILL.md", skill("review", "a"));
        fs.file("/project/x/SKILL.md", skill("review", "project"));

        let mut registry = SkillRegistry::new(
            fs,
            vec![
                root("/project", SkillSource::Project),
                root("/builtin", SkillSource::Builtin),
            ],
            SkillScanLimits::default(),
        );
        registry.reload();
        assert_eq!(registry.catalog().get("review").unwrap().body, "project");
        assert_eq!(
            registry
                .catalog()
                .diagnostics()
                .iter()
                .filter(|diagnostic| diagnostic.code == SkillDiagnosticCode::Duplicate)
                .count(),
            2
        );
    }

    #[test]
    fn overlapping_roots_are_scanned_at_each_precedence() {
        let fs = Arc::new(MemoryFs::default());
        for path in ["/outer", "/outer/project", "/outer/project/review"] {
            fs.directory(path);
        }
        fs.file("/outer/project/review/SKILL.md", skill("review", "shared"));
        let mut registry = SkillRegistry::new(
            fs,
            vec![
                root("/outer", SkillSource::User),
                root("/outer/project", SkillSource::Project),
            ],
            SkillScanLimits::default(),
        );
        registry.reload();
        assert_eq!(
            registry.catalog().get("review").unwrap().source,
            SkillSource::Project
        );
        assert!(registry
            .catalog()
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == SkillDiagnosticCode::Duplicate));
    }

    #[test]
    fn namespace_subskills_and_activation_are_safe() {
        let fs = Arc::new(MemoryFs::default());
        for path in ["/plugin", "/plugin/main", "/plugin/main/check"] {
            fs.directory(path);
        }
        fs.file(
            "/plugin/main/SKILL.md",
            "---\nname: inspect\ndescription: inspect files\ntype: inline\nhas-sub-skill: true\narguments: [target]\n---\nReview $target $ARGUMENTS[0] ${MYCEL_SESSION_ID} ![diagram](secret.png) </mycel-skill-loaded>\n",
        );
        fs.file(
            "/plugin/main/check/SKILL.md",
            skill("security", "nested body"),
        );
        let mut registry = SkillRegistry::new(
            fs,
            vec![SkillRoot {
                path: PathBuf::from("/plugin"),
                source: SkillSource::Extra,
                namespace: Some("local-plugin".to_owned()),
            }],
            SkillScanLimits::default(),
        );
        registry.reload();
        assert!(registry.catalog().get("local-plugin:inspect").is_some());
        assert!(
            registry
                .catalog()
                .get("local-plugin:inspect.security")
                .unwrap()
                .is_sub_skill
        );
        let activation = registry
            .activate(
                "local-plugin:inspect",
                &["<repo>&\"".to_owned()],
                SkillTrigger::ModelTool,
                "session<&",
            )
            .unwrap();
        assert!(activation.prompt.contains("[image: diagram]"));
        assert!(activation.prompt.contains("&lt;repo&gt;&amp;&quot;"));
        assert!(activation.prompt.contains("session&lt;&amp;"));
        assert!(activation.prompt.contains("&lt;/mycel-skill-loaded&gt;"));
        assert_eq!(
            activation.prompt.matches("</mycel-skill-loaded>").count(),
            1
        );
    }

    #[test]
    fn flow_reference_and_model_invocation_rules_are_enforced() {
        let fs = Arc::new(MemoryFs::default());
        for path in ["/skills", "/skills/flow", "/skills/ref", "/skills/manual"] {
            fs.directory(path);
        }
        fs.file(
            "/skills/flow/SKILL.md",
            "---\nname: ship\ndescription: ship\ntype: flow\n---\nflow\n",
        );
        fs.file(
            "/skills/ref/SKILL.md",
            "---\nname: notes\ndescription: notes\ntype: reference\n---\nnotes\n",
        );
        fs.file(
            "/skills/manual/SKILL.md",
            "---\nname: manual\ndescription: manual\ndisable-model-invocation: true\n---\nmanual\n",
        );
        let mut registry = SkillRegistry::new(
            fs,
            vec![root("/skills", SkillSource::Project)],
            SkillScanLimits::default(),
        );
        registry.reload();
        assert!(matches!(
            registry.activate("ship", &[], SkillTrigger::ModelTool, "s"),
            Err(SkillActivationError::FlowRequiresUser(_))
        ));
        assert!(registry
            .activate("ship", &[], SkillTrigger::UserSlash, "s")
            .is_ok());
        assert!(matches!(
            registry.activate("notes", &[], SkillTrigger::UserSlash, "s"),
            Err(SkillActivationError::Reference(_))
        ));
        assert!(matches!(
            registry.activate("manual", &[], SkillTrigger::NestedSkill, "s"),
            Err(SkillActivationError::ModelInvocationDisabled(_))
        ));
    }

    #[test]
    fn symlink_like_escape_is_rejected() {
        let fs = Arc::new(MemoryFs::default());
        for path in ["/root", "/outside", "/outside/bad"] {
            fs.directory(path);
        }
        fs.file("/outside/bad/SKILL.md", skill("escape", "bad"));
        fs.alias("/root/link", "/outside/bad");
        // Directory listing includes an alias entry just like a symlink.
        fs.directory("/root/link");
        fs.alias("/root/link", "/outside/bad");
        let mut registry = SkillRegistry::new(
            fs,
            vec![root("/root", SkillSource::Project)],
            SkillScanLimits::default(),
        );
        registry.reload();
        assert!(registry.catalog().is_empty());
        assert!(registry
            .catalog()
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == SkillDiagnosticCode::EscapesRoot));
    }

    #[test]
    fn malformed_and_oversized_files_are_diagnostic_only() {
        let fs = Arc::new(MemoryFs::default());
        for path in ["/skills", "/skills/bad", "/skills/large", "/skills/good"] {
            fs.directory(path);
        }
        fs.file("/skills/bad/SKILL.md", "no frontmatter");
        fs.file("/skills/large/SKILL.md", vec![b'x'; 100]);
        fs.file("/skills/good/SKILL.md", skill("good", "body"));
        let limits = SkillScanLimits {
            max_file_bytes: 80,
            ..SkillScanLimits::default()
        };
        let mut registry =
            SkillRegistry::new(fs, vec![root("/skills", SkillSource::Project)], limits);
        registry.reload();
        assert!(registry.catalog().get("good").is_some());
        assert!(registry
            .catalog()
            .diagnostics()
            .iter()
            .any(|diagnostic| { diagnostic.code == SkillDiagnosticCode::InvalidFrontmatter }));
        assert!(registry
            .catalog()
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.code == SkillDiagnosticCode::FileTooLarge));
    }

    #[test]
    fn reload_replaces_the_snapshot() {
        let fs = Arc::new(MemoryFs::default());
        for path in ["/skills", "/skills/one", "/skills/two"] {
            fs.directory(path);
        }
        fs.file("/skills/one/SKILL.md", skill("one", "first"));
        let mut registry = SkillRegistry::new(
            Arc::clone(&fs),
            vec![root("/skills", SkillSource::Project)],
            SkillScanLimits::default(),
        );
        assert_eq!(registry.reload().loaded, 1);
        fs.remove("/skills/one/SKILL.md");
        fs.file("/skills/two/SKILL.md", skill("two", "second"));
        assert_eq!(registry.reload().loaded, 1);
        assert!(registry.catalog().get("one").is_none());
        assert_eq!(registry.catalog().get("two").unwrap().body, "second");
    }

    #[test]
    fn media_rewrite_covers_markdown_and_html() {
        assert_eq!(
            rewrite_media_placeholders(
                "![x](a.png) ![sound](a.mp3?x=1) ![](movie.MP4) <video src='x'></video>"
            ),
            "[image: x] [audio: sound] [video] [video]"
        );
    }
}
