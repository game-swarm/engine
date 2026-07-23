use std::{path::Path, process::Command};

use super::update::{CargoAdapter, UpdateError};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ProcessCargo;

impl CargoAdapter for ProcessCargo {
    fn update_lock(&self, manifest: &Path) -> Result<(), UpdateError> {
        self.refresh_metadata(manifest)
    }

    fn verify_locked(&self, manifest: &Path) -> Result<(), UpdateError> {
        self.run_locked_gates(manifest)
    }
}

impl ProcessCargo {
    pub(crate) fn refresh_metadata(&self, manifest: &Path) -> Result<(), UpdateError> {
        run_cargo([
            "metadata",
            "--manifest-path",
            manifest
                .to_str()
                .ok_or_else(|| UpdateError::new("manifest path is not valid utf-8"))?,
            "--format-version",
            "1",
            "--features",
            "vanilla_mods",
        ])
    }

    pub(crate) fn run_locked_gates(&self, manifest: &Path) -> Result<(), UpdateError> {
        let manifest = manifest
            .to_str()
            .ok_or_else(|| UpdateError::new("manifest path is not valid utf-8"))?;

        run_cargo([
            "test",
            "--locked",
            "--manifest-path",
            manifest,
            "--test",
            "dependency_identity",
            "--features",
            "vanilla_mods",
        ])?;
        run_cargo([
            "check",
            "--locked",
            "--manifest-path",
            manifest,
            "--all-targets",
            "--features",
            "vanilla_mods",
        ])
    }
}

fn run_cargo<const N: usize>(args: [&str; N]) -> Result<(), UpdateError> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .args(args)
        .output()
        .map_err(|error| UpdateError::new(format!("failed to run cargo: {error}")))?;
    if output.status.success() {
        return Ok(());
    }

    Err(UpdateError::new(format!(
        "cargo {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(root: &Path) -> std::path::PathBuf {
        std::fs::create_dir_all(root.join("src")).expect("create src");
        std::fs::create_dir_all(root.join("tests")).expect("create tests");
        std::fs::write(
            root.join("Cargo.toml"),
            r#"
[package]
name = "cargo-fixture"
version = "0.1.0"
edition = "2024"

[features]
vanilla_mods = []
"#,
        )
        .expect("write manifest");
        std::fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n")
            .expect("write lib");
        std::fs::write(
            root.join("tests/dependency_identity.rs"),
            r#"
#[test]
fn dependency_identity() {
    assert_eq!(2 + 2, 4);
}
"#,
        )
        .expect("write test");
        root.join("Cargo.toml")
    }

    #[test]
    fn refresh_metadata_writes_lockfile_and_locked_gates_pass() {
        let root = tempfile::tempdir().expect("tempdir");
        let manifest = write_fixture(root.path());
        let cargo = ProcessCargo;

        cargo.refresh_metadata(&manifest).expect("refresh metadata");
        assert!(manifest.with_file_name("Cargo.lock").exists());

        cargo.run_locked_gates(&manifest).expect("locked gates");
    }
}
