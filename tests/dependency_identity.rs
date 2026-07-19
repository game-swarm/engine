use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Metadata {
    packages: Vec<Package>,
}

#[derive(Debug, Deserialize)]
struct Package {
    name: String,
    version: String,
    id: String,
    source: Option<String>,
    manifest_path: String,
}

#[test]
fn shared_dependencies_resolve_to_single_expected_packages() {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to execute cargo metadata");

    assert!(
        output.status.success(),
        "cargo metadata failed with status {}:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let metadata: Metadata = serde_json::from_slice(&output.stdout)
        .expect("cargo metadata returned invalid JSON for format version 1");

    assert_single_package(&metadata, "swarm-engine-api", "0.1.0", None);
    assert_single_package(&metadata, "swarm-engine-plugin-sdk", "0.1.0", None);
    assert_single_package(
        &metadata,
        "bevy",
        "0.19.0",
        Some("registry+https://github.com/rust-lang/crates.io-index"),
    );

    let swarm_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("engine manifest must have a workspace parent");
    assert_canonical_path_package(
        &metadata,
        "swarm-engine-api",
        &swarm_root.join("engine-api/crates/swarm-engine-api/Cargo.toml"),
    );
    assert_canonical_path_package(
        &metadata,
        "swarm-engine-plugin-sdk",
        &swarm_root.join("engine-api/crates/swarm-engine-plugin-sdk/Cargo.toml"),
    );
}

fn assert_canonical_path_package(metadata: &Metadata, package_name: &str, expected: &Path) {
    let package = metadata
        .packages
        .iter()
        .find(|package| package.name == package_name)
        .unwrap_or_else(|| panic!("missing resolved package {package_name}"));
    let actual = PathBuf::from(&package.manifest_path)
        .canonicalize()
        .unwrap_or_else(|error| {
            panic!("failed to canonicalize {}: {error}", package.manifest_path)
        });
    let expected = expected
        .canonicalize()
        .unwrap_or_else(|error| panic!("failed to canonicalize {}: {error}", expected.display()));
    assert_eq!(
        actual, expected,
        "resolved {package_name} manifest is not the canonical sibling path"
    );
}

fn assert_single_package(
    metadata: &Metadata,
    package_name: &str,
    expected_version: &str,
    expected_source: Option<&str>,
) {
    let matches: Vec<&Package> = metadata
        .packages
        .iter()
        .filter(|package| package.name == package_name)
        .collect();
    let identities: HashSet<(&str, Option<&str>)> = matches
        .iter()
        .map(|package| (package.id.as_str(), package.source.as_deref()))
        .collect();

    assert_eq!(
        matches.len(),
        1,
        "expected exactly one resolved {package_name} package, found {}: {matches:#?}",
        matches.len()
    );
    assert_eq!(
        identities.len(),
        matches.len(),
        "resolved {package_name} records do not have unique package ID/source pairs: {matches:#?}"
    );

    let package = matches[0];
    assert_eq!(
        package.version, expected_version,
        "unexpected {package_name} version in resolved package: {package:#?}"
    );
    assert_eq!(
        package.source.as_deref(),
        expected_source,
        "unexpected {package_name} source in resolved package: {package:#?}"
    );
}
