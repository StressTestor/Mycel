use std::{fs, path::Path};

const FORBIDDEN_PATHS: &[&str] = &["harness", "scripts/mycel-delegate"];

const FORBIDDEN_BUILD_MARKERS: &[&str] = &["pnpm", "setup-node", "node_modules", "harness/"];

const FORBIDDEN_CONTROL_PLANE_MARKERS: &[&str] = &[
    concat!("telemetry-logs", ".kimi.com"),
    concat!("KIMI_DISABLE_", "TELEMETRY"),
    concat!("KIMI_", "TELEMETRY"),
    concat!("marketplace", ".json"),
    concat!("moonshot_", "fetch"),
    concat!("moonshot_", "search"),
    concat!("update", ".kimi.com"),
];

fn repository_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("mycel-tests must remain under <repo>/crates")
}

fn read(relative: &str) -> String {
    let path = repository_root().join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn visit_source_files(root: &Path, visit: &mut impl FnMut(&Path)) {
    for entry in fs::read_dir(root)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", root.display()))
    {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        let file_type = entry.file_type().expect("entry type");
        if file_type.is_dir() {
            visit_source_files(&path, visit);
        } else if file_type.is_file()
            && matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("rs" | "toml" | "ts" | "tsx" | "js" | "mjs" | "json" | "yaml" | "yml")
            )
        {
            visit(&path);
        }
    }
}

#[test]
fn removed_product_surfaces_do_not_return() {
    for relative in FORBIDDEN_PATHS {
        assert!(
            !repository_root().join(relative).exists(),
            "non-CLI product surface returned: {relative}"
        );
    }
}

#[test]
fn build_install_and_primary_docs_are_rust_only() {
    for relative in [
        "Cargo.toml",
        ".github/workflows/monorepo-ci.yml",
        "README.md",
        "install.sh",
    ] {
        let source = read(relative);
        for marker in FORBIDDEN_BUILD_MARKERS {
            assert!(
                !source.contains(marker),
                "removed TypeScript build marker {marker:?} returned in {relative}"
            );
        }
    }
}

#[test]
fn retained_runtime_has_no_vendor_telemetry_update_or_marketplace_endpoint() {
    visit_source_files(&repository_root().join("crates"), &mut |path| {
        if path.ends_with("mycel-tests/tests/product_boundary.rs") {
            return;
        }
        let source = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        for marker in FORBIDDEN_CONTROL_PLANE_MARKERS {
            assert!(
                !source.contains(marker),
                "forbidden vendor control-plane marker {marker:?} in {}",
                path.display()
            );
        }
    });
}
