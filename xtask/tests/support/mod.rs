use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

use toml_edit::{Array, DocumentMut, value};

#[derive(Debug)]
pub(crate) struct TempEngineRoot {
    _root: tempfile::TempDir,
    engine: PathBuf,
    mods: PathBuf,
}

impl TempEngineRoot {
    pub(crate) fn path(&self) -> &Path {
        &self.engine
    }

    pub(crate) fn mods_dir(&self) -> &Path {
        &self.mods
    }
}

#[derive(Debug)]
pub(crate) enum NewHeadKind {
    Compatible,
    Incompatible,
}

#[derive(Debug)]
pub(crate) struct ModFixture {
    _root: tempfile::TempDir,
    name: String,
    remote: PathBuf,
    old_head: String,
    new_head: String,
}

impl ModFixture {
    pub(crate) fn remote(&self) -> &Path {
        &self.remote
    }

    pub(crate) fn old_head(&self) -> &str {
        &self.old_head
    }

    pub(crate) fn new_head(&self) -> &str {
        &self.new_head
    }

    pub(crate) fn install_checkout(&self, mods_dir: &Path) -> PathBuf {
        let checkout = mods_dir.join(&self.name);
        run_git(
            [
                "clone",
                self.remote.to_str().expect("remote path"),
                checkout.to_str().expect("checkout path"),
            ]
            .as_slice(),
            None,
        );
        run_git(
            ["checkout", "--detach", self.old_head.as_str()].as_slice(),
            Some(&checkout),
        );
        checkout
    }
}

pub(crate) fn prepare_engine_root(mods_toml: &str) -> TempEngineRoot {
    let root = tempfile::tempdir().expect("temp swarm root");
    let engine = root.path().join("engine");
    let mods = root.path().join("mods");

    fs::create_dir_all(engine.join("src")).expect("create engine src");
    fs::create_dir_all(engine.join("tests")).expect("create engine tests");
    fs::create_dir_all(&mods).expect("create sibling mods dir");

    fs::write(
        engine.join("Cargo.toml"),
        engine_manifest_for_mods(mods_toml),
    )
    .expect("write engine Cargo.toml");
    fs::write(
        engine.join("src/lib.rs"),
        "pub fn dependency_identity() -> u32 { 1 }\n",
    )
    .expect("write engine lib.rs");
    fs::write(
        engine.join("tests/dependency_identity.rs"),
        "#[test]\nfn dependency_identity() {\n    assert_eq!(1, 1);\n}\n",
    )
    .expect("write dependency_identity test");
    fs::write(engine.join("Cargo.lock"), engine_lockfile()).expect("write engine Cargo.lock");
    fs::write(engine.join("mods.toml"), mods_toml).expect("write mods.toml");

    TempEngineRoot {
        _root: root,
        engine,
        mods,
    }
}

pub(crate) fn create_mod_fixture(name: &str, new_head: NewHeadKind) -> ModFixture {
    let root = tempfile::tempdir().expect("temp mod fixture");
    let worktree = root.path().join("worktree");
    let remote = root.path().join(format!("{name}.git"));
    let package_name = format!("swarm-mod-{name}");

    run_git(
        ["init", "--bare", remote.to_str().expect("remote path")].as_slice(),
        None,
    );
    run_git(
        [
            "init",
            "-b",
            "main",
            worktree.to_str().expect("worktree path"),
        ]
        .as_slice(),
        None,
    );
    run_git(
        ["config", "user.email", "test@example.com"].as_slice(),
        Some(&worktree),
    );
    run_git(
        ["config", "user.name", "Test User"].as_slice(),
        Some(&worktree),
    );

    write_mod_package(
        &worktree,
        &package_name,
        "0.1.0",
        "pub fn dependency_identity() -> u32 { 1 }\n",
    );
    commit_all(&worktree, "initial pinned commit");
    run_git(
        [
            "remote",
            "add",
            "origin",
            remote.to_str().expect("remote path"),
        ]
        .as_slice(),
        Some(&worktree),
    );
    run_git(["push", "-u", "origin", "main"].as_slice(), Some(&worktree));
    let old_head = git_output(["rev-parse", "HEAD"].as_slice(), Some(&worktree));

    match new_head {
        NewHeadKind::Compatible => write_mod_package(
            &worktree,
            &package_name,
            "0.1.0",
            "pub fn dependency_identity() -> u32 { 2 }\n",
        ),
        NewHeadKind::Incompatible => write_mod_package(
            &worktree,
            &package_name,
            "0.1.0",
            "compile_error!(\"incompatible mod head\");\n",
        ),
    }
    commit_all(&worktree, "new remote head");
    run_git(["push", "origin", "main"].as_slice(), Some(&worktree));
    run_git(
        ["symbolic-ref", "HEAD", "refs/heads/main"].as_slice(),
        Some(&remote),
    );
    let new_head = git_output(["rev-parse", "HEAD"].as_slice(), Some(&worktree));

    ModFixture {
        _root: root,
        name: name.to_string(),
        remote,
        old_head: old_head.trim().to_string(),
        new_head: new_head.trim().to_string(),
    }
}

pub(crate) fn create_local_path_mod(mods_dir: &Path, name: &str) -> PathBuf {
    let path = mods_dir.join(name);
    fs::create_dir_all(path.join("src")).expect("create local mod src");
    fs::write(
        path.join("Cargo.toml"),
        format!(
            r#"
[package]
name = "swarm-mod-{name}"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
path = "src/lib.rs"
"#
        ),
    )
    .expect("write local mod Cargo.toml");
    fs::write(
        path.join("src/lib.rs"),
        "pub fn dependency_identity() -> u32 { 1 }\n",
    )
    .expect("write local mod lib.rs");
    path
}

pub(crate) fn run_xtask(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(xtask_binary())
        .current_dir(root)
        .args(args)
        .output()
        .expect("xtask command")
}

pub(crate) fn checkout_head(path: &Path) -> String {
    git_output(["rev-parse", "HEAD"].as_slice(), Some(path))
        .trim()
        .to_string()
}

pub(crate) fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, contents).expect("write file");
}

pub(crate) fn read_file(path: &Path) -> String {
    fs::read_to_string(path).expect("read file")
}

pub(crate) fn run_git(args: &[&str], current_dir: Option<&Path>) {
    let output = git_command(args, current_dir)
        .output()
        .expect("git command");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(crate) fn git_output(args: &[&str], current_dir: Option<&Path>) -> String {
    let output = git_command(args, current_dir)
        .output()
        .expect("git command");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf8 git output")
}

fn git_command(args: &[&str], current_dir: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    if let Some(dir) = current_dir {
        command.current_dir(dir);
    }
    command.args(args);
    command
}

fn xtask_binary() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_xtask")
        .map(PathBuf::from)
        .expect("xtask binary path")
}

fn engine_manifest_for_mods(mods_toml: &str) -> String {
    let mods_doc = mods_toml
        .parse::<DocumentMut>()
        .expect("invalid synthetic mods.toml");
    let mods = mods_doc["mods"]
        .as_table()
        .expect("synthetic mods.toml is missing [mods]");

    let mut document = DocumentMut::new();
    document["package"]["name"] = value("swarm-engine");
    document["package"]["version"] = value("0.1.0");
    document["package"]["edition"] = value("2024");
    document["lib"]["path"] = value("src/lib.rs");

    let mut vanilla_mods = Array::new();
    for (name, _) in mods.iter() {
        let dependency_name = format!("swarm-mod-{name}");
        document["dependencies"][&dependency_name]["path"] = value(format!("../mods/{name}"));
        document["dependencies"][&dependency_name]["optional"] = value(true);
        vanilla_mods.push(format!("dep:{dependency_name}"));
    }
    document["features"]["vanilla_mods"] = value(vanilla_mods);

    document.to_string()
}

fn generic_engine_manifest() -> String {
    String::from(
        r#"
[package]
name = "swarm-engine"
version = "0.1.0"
edition = "2024"

[features]
vanilla_mods = []

[lib]
path = "src/lib.rs"
"#,
    )
}

fn engine_lockfile() -> String {
    static LOCKFILE: OnceLock<String> = OnceLock::new();
    LOCKFILE.get_or_init(generate_engine_lockfile).clone()
}

fn generate_engine_lockfile() -> String {
    let root = tempfile::tempdir().expect("temp lockfile fixture");
    let manifest = root.path().join("Cargo.toml");
    fs::write(&manifest, generic_engine_manifest()).expect("write lockfile manifest");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .args([
            "generate-lockfile",
            "--offline",
            "--manifest-path",
            manifest.to_str().expect("manifest path"),
        ])
        .output()
        .expect("cargo generate-lockfile");
    assert!(
        output.status.success(),
        "cargo generate-lockfile failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    fs::read_to_string(root.path().join("Cargo.lock")).expect("read generated lockfile")
}

fn write_mod_package(root: &Path, package_name: &str, version: &str, lib_rs: &str) {
    fs::create_dir_all(root.join("src")).expect("create mod src");
    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"
[package]
name = "{package_name}"
version = "{version}"
edition = "2024"
publish = false

[lib]
path = "src/lib.rs"
"#
        ),
    )
    .expect("write mod Cargo.toml");
    fs::write(root.join("src/lib.rs"), lib_rs).expect("write mod lib.rs");
}

fn commit_all(root: &Path, message: &str) {
    run_git(["add", "."].as_slice(), Some(root));
    run_git(["commit", "-m", message].as_slice(), Some(root));
}
