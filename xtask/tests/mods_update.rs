mod support;

use std::fs;

use support::{
    NewHeadKind, checkout_head, create_local_path_mod, create_mod_fixture, prepare_engine_root,
    read_file, run_xtask, write_file,
};

fn xtask(args: &[&str], root: &std::path::Path) -> std::process::Output {
    run_xtask(root, args)
}

fn text(output: &std::process::Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_failure_contains(output: &std::process::Output, fragment: &str) {
    assert!(!output.status.success(), "{}", text(output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(fragment),
        "{}",
        text(output)
    );
}

#[test]
fn selector_requires_either_names_or_all() {
    let root = prepare_engine_root("[mods]\n");

    let missing = xtask(&["mods", "update"], root.path());
    assert_failure_contains(&missing, "required arguments were not provided");

    let conflict = xtask(&["mods", "update", "--all", "combat-core"], root.path());
    assert_failure_contains(&conflict, "cannot be used with '[NAME]...'");
}

#[test]
fn selector_rejects_duplicate_and_unknown_names() {
    let selected = create_mod_fixture("combat-core", NewHeadKind::Compatible);
    let root = prepare_engine_root(&format!(
        "[mods]\ncombat-core = {{ git = \"{}\", rev = \"{}\" }}\n",
        selected.remote().display(),
        selected.old_head()
    ));
    selected.install_checkout(root.mods_dir());

    let duplicate = xtask(
        &["mods", "update", "combat-core", "combat-core"],
        root.path(),
    );
    assert_failure_contains(&duplicate, "duplicate");

    let unknown = xtask(&["mods", "update", "missing-mod"], root.path());
    assert_failure_contains(&unknown, "unknown");
}

#[test]
fn dry_run_on_stale_pin_is_no_write_and_success() {
    let selected = create_mod_fixture("combat-core", NewHeadKind::Compatible);
    let root = prepare_engine_root(&format!(
        "[mods]\ncombat-core = {{ git = \"{}\", rev = \"{}\" }}\n",
        selected.remote().display(),
        selected.old_head()
    ));
    let checkout = selected.install_checkout(root.mods_dir());
    let before_mods = read_file(&root.path().join("mods.toml"));
    let before_lock = read_file(&root.path().join("Cargo.lock"));
    let before_head = checkout_head(&checkout);

    let output = xtask(&["mods", "update", "--dry-run", "combat-core"], root.path());

    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(before_mods, read_file(&root.path().join("mods.toml")));
    assert_eq!(before_lock, read_file(&root.path().join("Cargo.lock")));
    assert_eq!(before_head, checkout_head(&checkout));
}

#[test]
fn check_on_stale_pin_is_no_write_and_fails_without_using_dirt_as_the_signal() {
    let selected = create_mod_fixture("combat-core", NewHeadKind::Compatible);
    let root = prepare_engine_root(&format!(
        "[mods]\ncombat-core = {{ git = \"{}\", rev = \"{}\" }}\n",
        selected.remote().display(),
        selected.old_head()
    ));
    let checkout = selected.install_checkout(root.mods_dir());
    write_file(&checkout.join("untracked.txt"), "dirty\n");
    let before_mods = read_file(&root.path().join("mods.toml"));
    let before_lock = read_file(&root.path().join("Cargo.lock"));
    let before_head = checkout_head(&checkout);

    let output = xtask(&["mods", "update", "--check", "combat-core"], root.path());

    assert_failure_contains(&output, "stale");
    assert_eq!(before_mods, read_file(&root.path().join("mods.toml")));
    assert_eq!(before_lock, read_file(&root.path().join("Cargo.lock")));
    assert_eq!(before_head, checkout_head(&checkout));
}

#[test]
fn selected_only_update_changes_selected_pin_and_preserves_unselected() {
    let selected = create_mod_fixture("combat-core", NewHeadKind::Compatible);
    let other = create_mod_fixture("depot-storage", NewHeadKind::Compatible);
    let root = prepare_engine_root(&format!(
        concat!(
            "# header comment\n",
            "[mods]\n",
            "# keep combat-core ordered first\n",
            "combat-core = {{ git = \"{}\", rev = \"{}\", metadata = {{ note = \"keep\" }} }}\n",
            "# keep depot-storage ordered second\n",
            "depot-storage = {{ git = \"{}\", rev = \"{}\", metadata = {{ note = \"keep\" }} }}\n"
        ),
        selected.remote().display(),
        selected.old_head(),
        other.remote().display(),
        other.old_head(),
    ));
    let selected_checkout = selected.install_checkout(root.mods_dir());
    let other_checkout = other.install_checkout(root.mods_dir());
    let before_mods = read_file(&root.path().join("mods.toml"));

    let output = xtask(&["mods", "update", "combat-core"], root.path());

    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(selected.new_head(), checkout_head(&selected_checkout));
    assert_eq!(other.old_head(), checkout_head(&other_checkout));
    let after_mods = read_file(&root.path().join("mods.toml"));
    assert_eq!(
        after_mods,
        before_mods.replace(selected.old_head(), selected.new_head())
    );
    assert!(after_mods.contains("# header comment"));
    assert!(after_mods.contains("# keep combat-core ordered first"));
    assert!(after_mods.contains("# keep depot-storage ordered second"));
    assert!(after_mods.contains("metadata = { note = \"keep\" }"));
}

#[test]
fn all_update_skips_path_source_and_explicit_path_selection_fails() {
    let selected = create_mod_fixture("combat-core", NewHeadKind::Compatible);
    let root = prepare_engine_root(&format!(
        concat!(
            "[mods]\n",
            "combat-core = {{ git = \"{}\", rev = \"{}\" }}\n",
            "local-helper = {{ path = \"../mods/local-helper\" }}\n"
        ),
        selected.remote().display(),
        selected.old_head(),
    ));
    let selected_checkout = selected.install_checkout(root.mods_dir());
    let local_path = create_local_path_mod(root.mods_dir(), "local-helper");
    let before_local = read_file(&local_path.join("Cargo.toml"));
    let before_local_lib = read_file(&local_path.join("src/lib.rs"));

    let all = xtask(&["mods", "update", "--all"], root.path());
    assert!(all.status.success(), "{}", text(&all));
    assert_eq!(selected.new_head(), checkout_head(&selected_checkout));
    assert_eq!(before_local, read_file(&local_path.join("Cargo.toml")));
    assert_eq!(before_local_lib, read_file(&local_path.join("src/lib.rs")));

    let explicit = xtask(&["mods", "update", "local-helper"], root.path());
    assert_failure_contains(&explicit, "path");
}

#[test]
fn multiple_remote_resolution_failure_blocks_all_mutation() {
    let selected = create_mod_fixture("combat-core", NewHeadKind::Compatible);
    let root = prepare_engine_root(&format!(
        concat!(
            "[mods]\n",
            "combat-core = {{ git = \"{}\", rev = \"{}\" }}\n",
            "depot-storage = {{ git = \"/definitely/missing.git\", rev = \"0000000000000000000000000000000000000000\" }}\n"
        ),
        selected.remote().display(),
        selected.old_head(),
    ));
    let checkout = selected.install_checkout(root.mods_dir());
    let before_mods = read_file(&root.path().join("mods.toml"));
    let before_head = checkout_head(&checkout);

    let output = xtask(
        &["mods", "update", "combat-core", "depot-storage"],
        root.path(),
    );

    assert_failure_contains(&output, "ls-remote");
    assert_eq!(before_mods, read_file(&root.path().join("mods.toml")));
    assert_eq!(before_head, checkout_head(&checkout));
}

#[test]
fn dirty_selected_checkout_blocks_apply() {
    let selected = create_mod_fixture("combat-core", NewHeadKind::Compatible);
    let root = prepare_engine_root(&format!(
        "[mods]\ncombat-core = {{ git = \"{}\", rev = \"{}\" }}\n",
        selected.remote().display(),
        selected.old_head()
    ));
    let checkout = selected.install_checkout(root.mods_dir());
    write_file(&checkout.join("dirty.txt"), "dirty\n");
    let before_mods = read_file(&root.path().join("mods.toml"));
    let before_head = checkout_head(&checkout);

    let output = xtask(&["mods", "update", "combat-core"], root.path());

    assert_failure_contains(&output, "clean");
    assert_eq!(before_mods, read_file(&root.path().join("mods.toml")));
    assert_eq!(before_head, checkout_head(&checkout));
}

#[test]
fn rollback_restores_exact_files_and_removes_absent_checkout() {
    let selected = create_mod_fixture("combat-core", NewHeadKind::Compatible);
    let failing = create_mod_fixture("depot-storage", NewHeadKind::Incompatible);
    let root = prepare_engine_root(&format!(
        concat!(
            "[mods]\n",
            "combat-core = {{ git = \"{}\", rev = \"{}\" }}\n",
            "depot-storage = {{ git = \"{}\", rev = \"{}\" }}\n"
        ),
        selected.remote().display(),
        selected.old_head(),
        failing.remote().display(),
        failing.old_head(),
    ));
    let checkout = selected.install_checkout(root.mods_dir());
    let before_mods = read_file(&root.path().join("mods.toml"));
    let before_lock = read_file(&root.path().join("Cargo.lock"));
    let before_head = checkout_head(&checkout);
    let absent_checkout = root.mods_dir().join("depot-storage");
    if absent_checkout.exists() {
        fs::remove_dir_all(&absent_checkout).expect("remove preexisting checkout");
    }

    let output = xtask(&["mods", "update", "--all"], root.path());

    assert_failure_contains(&output, "cargo");
    assert_eq!(before_mods, read_file(&root.path().join("mods.toml")));
    assert_eq!(before_lock, read_file(&root.path().join("Cargo.lock")));
    assert_eq!(before_head, checkout_head(&checkout));
    assert!(!absent_checkout.exists());
}
