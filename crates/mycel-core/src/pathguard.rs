//! Path canonicalization and the compiled protected-path floor for the gate.
//!
//! Two jobs:
//!
//!   1. `resolve_path_forms` turns a model-controlled path string into the
//!      lexical-absolute and symlink-canonical forms used for matching, so that
//!      `~`, relative, `./..`, a symlinked parent, and case respellings all fold
//!      to the same comparable value. Matching succeeds if EITHER form lines up.
//!
//!   2. `protected_floor_check` is a compiled denylist that blocks writes to the
//!      gate's own binary directory, its installed config, and its substrate
//!      before the substrate db is ever opened. This seals the gate against a
//!      same-session self-disarm (a Write that stubs `~/.mycel/bin/mycel-gate`
//!      or neuters the installed config) and cannot be removed by config: config
//!      may add denies, never subtract a floor rule.
//!
//! The resolver folds are applied at match time and are never baked into a
//! stored pattern, so non-gate callers that pass raw values through
//! `Signature::matches` keep their exact-string semantics.

use std::path::{Component, Path, PathBuf};

use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::{EvaluationMatch, EvaluationOutcome, RefusalMode, Severity};

/// Synthetic antibody id stamped on floor refusals. The floor is not a stored
/// antibody, so it has no real id; this phantom keeps `EvaluationMatch`
/// well-formed without reaching into the substrate.
const FLOOR_ANTIBODY_ID: Uuid = Uuid::nil();

/// Source pointer carried on every protected-path floor refusal. Greppable and
/// stable so callers/tests can assert the block came from the floor.
pub const FLOOR_SOURCE_POINTER: &str = "mycel-gate:protected-path-floor";

/// Resolution context for [`resolve_path_forms`].
#[derive(Debug, Clone, Default)]
pub struct PathContext {
    /// HOME to expand a leading `~` against. `None` disables `~` expansion.
    pub home: Option<PathBuf>,
    /// Directory a relative path is resolved against. For the gate this MUST be
    /// the payload cwd (the agent's cwd), never the gate process cwd. `None`
    /// leaves a relative path unresolved (it can still match another relative).
    pub cwd: Option<PathBuf>,
}

/// The comparable forms of a path: the purely lexical absolute form (always
/// present) and the symlink-resolved canonical form (absent when the deepest
/// existing ancestor cannot be canonicalized).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathForms {
    pub lexical: String,
    pub canonical: Option<String>,
}

/// A pattern is a glob (and must NOT be path-canonicalized) if it carries glob
/// metacharacters. A concrete path has none.
pub fn is_glob(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?')
}

/// Resolve a model-controlled path string into its lexical + canonical folded
/// forms. Never fails: the canonical form is best-effort and is `None` when the
/// deepest existing ancestor cannot be canonicalized.
pub fn resolve_path_forms(raw: &str, ctx: &PathContext) -> PathForms {
    let lexical_path = lexical_absolute(raw, ctx);
    let canonical_path = canonical_from_lexical(&lexical_path);
    PathForms {
        lexical: fold_resolved(&lexical_path),
        canonical: canonical_path.as_deref().map(fold_resolved),
    }
}

/// True if any resolved form of `a` equals any resolved form of `b`. Used by the
/// concrete-path (non-glob) `file_pattern` matcher, which is equality-based.
pub fn forms_equal(a: &PathForms, b: &PathForms) -> bool {
    for x in a.forms() {
        for y in b.forms() {
            if x == y {
                return true;
            }
        }
    }
    false
}

/// True if any resolved form of `target` sits at or under any resolved form of
/// `root` (component-wise, so `/a/bin` is not "under" `/a/binary`). Used by the
/// prefix-based protected-path floor.
pub fn path_under_any(target: &PathForms, root: &PathForms) -> bool {
    for t in target.forms() {
        for r in root.forms() {
            if Path::new(t).starts_with(Path::new(r)) {
                return true;
            }
        }
    }
    false
}

impl PathForms {
    fn forms(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.lexical).chain(self.canonical.iter())
    }
}

/// Roots the floor protects, derived from the resolved mycel home. Tight on
/// purpose: only the gate's own binary dir, its installed config, and its
/// substrate. Broad rules (`.git/hooks/**`, shell rc, dotfiles) are deliberately
/// NOT here - they would brick husky/lefthook/pre-commit and dotfiles repos.
fn floor_roots(mycel_home: &Path) -> [PathBuf; 3] {
    [
        mycel_home.join("bin"),         // mycel-gate + sibling binaries
        mycel_home.join("config.toml"), // installed config that wires [[hooks]]
        mycel_home.join("substrate"),   // db + audit log
    ]
}

/// Decide whether a write to `file_path` targets a protected floor path. `ctx`
/// supplies HOME + the payload cwd used to resolve the model-controlled path.
///
/// Returns `Some(refusal)` to BLOCK. Fail-closed: a path that lexically points
/// into a protected root is blocked even if it (or its parents) cannot be
/// canonicalized, and a symlinked parent that resolves into a protected root is
/// blocked via the canonical form.
pub fn protected_floor_check(
    file_path: &str,
    ctx: &PathContext,
    mycel_home: &Path,
) -> Option<EvaluationMatch> {
    let target = resolve_path_forms(file_path, ctx);
    // Roots are absolute paths under our control; resolve them the same way
    // (HOME for a `~`-based mycel_home, no cwd needed) so a symlinked ~/.mycel is
    // handled symmetrically with the target.
    let root_ctx = PathContext {
        home: ctx.home.clone(),
        cwd: None,
    };
    for root in floor_roots(mycel_home) {
        let root_forms = resolve_path_forms(&root.to_string_lossy(), &root_ctx);
        if path_under_any(&target, &root_forms) {
            return Some(floor_refusal(&root));
        }
    }
    None
}

fn floor_refusal(root: &Path) -> EvaluationMatch {
    EvaluationMatch {
        antibody_id: FLOOR_ANTIBODY_ID,
        outcome: EvaluationOutcome::Refuse,
        severity: Severity::Refuse,
        refusal_mode: RefusalMode::Hard,
        remediation: format!(
            "refusing to write inside the mycel guard's own protected path ({}); this would \
             disarm the fail-closed gate. change the repo and re-run install.sh instead",
            root.display()
        ),
        source_pointer: FLOOR_SOURCE_POINTER.to_string(),
    }
}

// --- resolution internals ---------------------------------------------------

fn expand_home(raw: &str, home: Option<&Path>) -> PathBuf {
    if raw == "~" {
        if let Some(h) = home {
            return h.to_path_buf();
        }
    } else if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(h) = home {
            return h.join(rest);
        }
    }
    PathBuf::from(raw)
}

fn lexical_absolute(raw: &str, ctx: &PathContext) -> PathBuf {
    let expanded = expand_home(raw, ctx.home.as_deref());
    let joined = if expanded.is_absolute() {
        expanded
    } else if let Some(cwd) = ctx.cwd.as_deref() {
        cwd.join(expanded)
    } else {
        expanded
    };
    lexical_collapse(&joined)
}

/// Collapse `.` and `..` purely lexically - never touches the filesystem and
/// never climbs above the root (`..` at the root is dropped, not escaped).
fn lexical_collapse(path: &Path) -> PathBuf {
    let mut comps: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match comps.last() {
                Some(Component::Normal(_)) => {
                    comps.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {
                    // at the root: cannot escape, drop the `..`.
                }
                _ => comps.push(Component::ParentDir),
            },
            other => comps.push(other),
        }
    }
    let mut out = PathBuf::new();
    for c in comps {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Resolve symlinks by canonicalizing the deepest EXISTING ancestor of `lexical`
/// and re-appending the not-yet-created tail. `None` if no ancestor can be
/// canonicalized (defeats a symlinked parent while never ENOENT-blocking a new
/// file whose parent exists).
fn canonical_from_lexical(lexical: &Path) -> Option<PathBuf> {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = lexical.to_path_buf();
    loop {
        if cur.exists() {
            let mut canon = cur.canonicalize().ok()?;
            for seg in tail.iter().rev() {
                canon.push(seg);
            }
            return Some(canon);
        }
        let name = cur.file_name()?.to_os_string();
        tail.push(name);
        if !cur.pop() {
            return None;
        }
    }
}

fn deepest_existing(path: &Path) -> Option<PathBuf> {
    let mut cur = path.to_path_buf();
    loop {
        if cur.exists() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

/// Fold a path to its comparison form: NFC-normalized always, ASCII-case-lowered
/// only when the underlying volume is case-insensitive. Case-folding is
/// volume-conditional on purpose - unconditional lowercasing would false-deny on
/// a case-sensitive volume (Linux CI, case-sensitive APFS).
fn fold_resolved(path: &Path) -> String {
    let case_insensitive = deepest_existing(path)
        .map(|ancestor| volume_is_case_insensitive(&ancestor))
        .unwrap_or(false);
    let nfc: String = path.to_string_lossy().nfc().collect();
    if case_insensitive {
        nfc.to_lowercase()
    } else {
        nfc
    }
}

/// Probe whether the volume holding `existing` (which MUST exist) is
/// case-insensitive, by flipping the ASCII case of the final component and
/// checking whether the flipped spelling resolves to the same inode. Returns
/// `false` (assume case-sensitive) when it cannot tell.
pub fn volume_is_case_insensitive(existing: &Path) -> bool {
    let Some(name) = existing.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let flipped: String = name.chars().map(flip_ascii_case).collect();
    if flipped == name {
        // no ASCII letters to flip: cannot probe, assume case-sensitive.
        return false;
    }
    let probe = existing.with_file_name(flipped);
    same_file(existing, &probe)
}

fn flip_ascii_case(c: char) -> char {
    if c.is_ascii_uppercase() {
        c.to_ascii_lowercase()
    } else if c.is_ascii_lowercase() {
        c.to_ascii_uppercase()
    } else {
        c
    }
}

#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (a.symlink_metadata(), b.symlink_metadata()) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(home: &Path, cwd: &Path) -> PathContext {
        PathContext {
            home: Some(home.to_path_buf()),
            cwd: Some(cwd.to_path_buf()),
        }
    }

    /// Build a fake install: `<home>/.mycel/bin/mycel-gate` present.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().to_path_buf();
        let mycel_home = home.join(".mycel");
        std::fs::create_dir_all(mycel_home.join("bin")).unwrap();
        std::fs::create_dir_all(mycel_home.join("substrate")).unwrap();
        std::fs::write(mycel_home.join("bin").join("mycel-gate"), b"stub").unwrap();
        std::fs::write(mycel_home.join("config.toml"), b"cfg").unwrap();
        (dir, home, mycel_home)
    }

    #[test]
    fn glob_pattern_is_detected() {
        assert!(is_glob("**/.mycel/**"));
        assert!(is_glob("src/*.rs"));
        assert!(is_glob("a?b"));
        assert!(!is_glob("/home/u/.mycel/bin/mycel-gate"));
        assert!(!is_glob("~/.mycel/config.toml"));
    }

    #[test]
    fn variants_fold_to_the_same_form_and_match_symmetrically() {
        let (_dir, home, mycel_home) = fixture();
        let bin = mycel_home.join("bin");
        let gate = bin.join("mycel-gate");
        let c = ctx(&home, &bin);

        let absolute = resolve_path_forms(&gate.to_string_lossy(), &c);
        let tilde = resolve_path_forms("~/.mycel/bin/mycel-gate", &c);
        let relative = resolve_path_forms("mycel-gate", &c); // cwd == bin
        let dotdot = resolve_path_forms("../bin/./mycel-gate", &c);
        let dot = resolve_path_forms("./mycel-gate", &c);

        for other in [&tilde, &relative, &dotdot, &dot] {
            assert!(
                forms_equal(&absolute, other),
                "expected {other:?} to fold equal to {absolute:?}"
            );
        }
    }

    #[test]
    fn symlinked_parent_resolves_into_protected_root() {
        let (_dir, home, mycel_home) = fixture();
        let link = home.join("evil");
        #[cfg(unix)]
        std::os::unix::fs::symlink(mycel_home.join("bin"), &link).unwrap();
        #[cfg(not(unix))]
        return;

        let c = ctx(&home, &home);
        let refusal = protected_floor_check("evil/mycel-gate", &c, &mycel_home);
        assert!(
            refusal.is_some(),
            "a write through a symlinked parent into bin must be blocked"
        );
        // the raw `~` and relative spellings too.
        assert!(protected_floor_check("~/.mycel/bin/mycel-gate", &c, &mycel_home).is_some());
    }

    #[test]
    fn glob_root_is_not_canonicalized() {
        // A glob is never handed to the canonicalizer; the caller must gate on
        // is_glob first. Resolving a glob string as a concrete path would fail
        // to match anything real, which is exactly why is_glob short-circuits.
        assert!(is_glob("**/.mycel/**"));
    }

    #[test]
    fn floor_matches_exact_and_new_files_under_roots_and_allows_outside() {
        let (_dir, home, mycel_home) = fixture();
        let c = ctx(&home, &home);

        // exact protected paths.
        assert!(
            protected_floor_check(
                &mycel_home.join("bin").join("mycel-gate").to_string_lossy(),
                &c,
                &mycel_home
            )
            .is_some(),
            "exact gate binary must be protected"
        );
        assert!(
            protected_floor_check(
                &mycel_home.join("config.toml").to_string_lossy(),
                &c,
                &mycel_home
            )
            .is_some(),
            "installed config must be protected"
        );
        assert!(
            protected_floor_check(
                &mycel_home
                    .join("substrate")
                    .join("mycel.db")
                    .to_string_lossy(),
                &c,
                &mycel_home
            )
            .is_some(),
            "substrate db must be protected"
        );

        // not-yet-created file under a protected root (bin exists, file does not).
        assert!(
            protected_floor_check(
                &mycel_home.join("bin").join("brand-new").to_string_lossy(),
                &c,
                &mycel_home
            )
            .is_some(),
            "a new file under bin must resolve as protected via the deepest ancestor"
        );

        // new file OUTSIDE protected roots is allowed.
        assert!(
            protected_floor_check(
                &home.join("project").join("main.rs").to_string_lossy(),
                &c,
                &mycel_home
            )
            .is_none(),
            "a normal project file must not be floored"
        );
        // a sibling that shares a prefix but is not under bin/ is allowed
        // (component-wise, `binary` is not under `bin`).
        std::fs::create_dir_all(mycel_home.join("binary")).unwrap();
        assert!(
            protected_floor_check(
                &mycel_home.join("binary").join("x").to_string_lossy(),
                &c,
                &mycel_home
            )
            .is_none(),
            "component-wise: bin-prefixed sibling dir must not be floored"
        );
    }

    #[test]
    fn case_fold_is_volume_conditional() {
        let (_dir, home, mycel_home) = fixture();
        let bin = mycel_home.join("bin");
        let case_insensitive = volume_is_case_insensitive(&bin);
        let c = ctx(&home, &home);

        // a case-respelled, not-yet-created file under bin. On a case-insensitive
        // volume the fold lowercases both sides so it is floored; on a
        // case-sensitive volume BIN != bin and it is not floored.
        let respelled = mycel_home.join("BIN").join("brand-new");
        let floored =
            protected_floor_check(&respelled.to_string_lossy(), &c, &mycel_home).is_some();
        assert_eq!(
            floored, case_insensitive,
            "case-fold must match the volume: case_insensitive={case_insensitive}, floored={floored}"
        );
    }
}
