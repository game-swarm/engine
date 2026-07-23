use std::collections::HashSet;
use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Args;

use super::cargo::ProcessCargo;
use super::config::{CommitSha, ModEntry, ModName, ModSource, ModsConfig};
use super::git::{CheckoutSnapshot, ProcessGit};

#[derive(Debug, Args)]
pub(crate) struct UpdateArgs {
    #[arg(
        value_name = "NAME",
        required_unless_present = "all",
        conflicts_with = "all"
    )]
    names: Vec<String>,

    #[arg(long)]
    all: bool,

    #[arg(long, conflicts_with = "check")]
    dry_run: bool,

    #[arg(long, conflicts_with = "dry_run")]
    check: bool,
}

pub(crate) trait GitAdapter {
    fn remote_head(&self, repository: &str) -> Result<CommitSha, UpdateError>;
}

pub(crate) trait CargoAdapter {
    fn update_lock(&self, manifest: &Path) -> Result<(), UpdateError>;

    fn verify_locked(&self, manifest: &Path) -> Result<(), UpdateError>;
}

#[derive(Debug)]
pub(crate) struct UpdateError {
    message: String,
}

impl UpdateError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Clone, Debug)]
struct PlannedMod {
    name: ModName,
    repository: String,
    old_revision: CommitSha,
    new_revision: CommitSha,
}

impl PlannedMod {
    fn changed(&self) -> bool {
        self.old_revision != self.new_revision
    }
}

#[derive(Debug)]
struct CheckoutPlan {
    plan: PlannedMod,
    snapshot: CheckoutSnapshot,
    path: PathBuf,
}

#[derive(Debug)]
struct FileSnapshot {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
    permissions: Option<fs::Permissions>,
    candidate: Option<Option<Vec<u8>>>,
}

impl FileSnapshot {
    fn capture(path: PathBuf) -> Result<Self, UpdateError> {
        let (bytes, permissions) = read_file_state(&path)?;
        Ok(Self {
            path,
            bytes,
            permissions,
            candidate: None,
        })
    }

    fn require_bytes(&self) -> Result<&[u8], UpdateError> {
        self.bytes.as_deref().ok_or_else(|| {
            UpdateError::new(format!("required file is missing: {}", self.path.display()))
        })
    }

    fn still_matches(&self) -> Result<bool, UpdateError> {
        let (current, _) = read_file_state(&self.path)?;
        Ok(current == self.bytes)
    }

    fn write_candidate(&mut self, bytes: &[u8]) -> Result<(), UpdateError> {
        atomic_write(&self.path, bytes, self.permissions.as_ref())?;
        self.candidate = Some(Some(bytes.to_vec()));
        Ok(())
    }

    fn capture_candidate(&mut self) -> Result<(), UpdateError> {
        let (current, _) = read_file_state(&self.path)?;
        self.candidate = (current != self.bytes).then_some(current);
        Ok(())
    }

    fn restore(&self) -> Result<(), UpdateError> {
        let Some(candidate) = &self.candidate else {
            return Ok(());
        };
        let (current, _) = read_file_state(&self.path)?;
        if current != *candidate {
            return Err(UpdateError::new(format!(
                "refusing rollback because {} no longer matches the command-written candidate",
                self.path.display()
            )));
        }
        match &self.bytes {
            Some(bytes) => atomic_write(&self.path, bytes, self.permissions.as_ref()),
            None => match fs::symlink_metadata(&self.path) {
                Ok(metadata) if metadata.file_type().is_file() => fs::remove_file(&self.path)
                    .map_err(|error| {
                        UpdateError::new(format!(
                            "failed to remove {} during rollback: {error}",
                            self.path.display()
                        ))
                    }),
                Ok(_) => Err(UpdateError::new(format!(
                    "refusing to remove non-file during rollback: {}",
                    self.path.display()
                ))),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(UpdateError::new(format!(
                    "failed to inspect {} during rollback: {error}",
                    self.path.display()
                ))),
            },
        }
    }
}

pub(crate) fn run(args: UpdateArgs) -> Result<(), UpdateError> {
    let engine_root = find_engine_root()?;
    let manifest = engine_root.join("Cargo.toml");
    let mods_path = engine_root.join("mods.toml");
    let lock_path = engine_root.join("Cargo.lock");
    let mods_root = engine_root
        .parent()
        .ok_or_else(|| UpdateError::new("engine root must have a parent directory"))?
        .join("mods");

    let mut mods_snapshot = FileSnapshot::capture(mods_path)?;
    let source = std::str::from_utf8(mods_snapshot.require_bytes()?)
        .map_err(|_| UpdateError::new("mods.toml is not valid UTF-8"))?;
    let mut config =
        ModsConfig::parse(source).map_err(|error| UpdateError::new(error.to_string()))?;
    let entries = config
        .entries()
        .map_err(|error| UpdateError::new(error.to_string()))?;
    let selected = select_entries(&entries, &args)?;

    let git = ProcessGit;
    let mut plans = Vec::with_capacity(selected.len());
    for entry in selected {
        let ModSource::Git {
            repository,
            revision,
        } = entry.source
        else {
            unreachable!("path entries are rejected or skipped during selection");
        };
        let new_revision = git.remote_head(&repository)?;
        plans.push(PlannedMod {
            name: entry.name,
            repository,
            old_revision: revision,
            new_revision,
        });
    }

    print_plan(&plans);
    if args.dry_run {
        return Ok(());
    }
    if args.check {
        let stale: Vec<_> = plans.iter().filter(|plan| plan.changed()).collect();
        if stale.is_empty() {
            return Ok(());
        }
        return Err(UpdateError::new(format!(
            "stale mod pins: {}",
            stale
                .iter()
                .map(|plan| plan.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    let changed: Vec<_> = plans.into_iter().filter(PlannedMod::changed).collect();
    if changed.is_empty() {
        return Ok(());
    }

    let mut checkouts = Vec::with_capacity(changed.len());
    for plan in changed {
        let path = mods_root.join(plan.name.as_str());
        let snapshot = git.inspect_checkout(&path, &plan.repository)?;
        checkouts.push(CheckoutPlan {
            plan,
            snapshot,
            path,
        });
    }

    let mut lock_snapshot = FileSnapshot::capture(lock_path)?;
    if !lock_snapshot.still_matches()? {
        return Err(UpdateError::new(
            "Cargo.lock changed before the update could start",
        ));
    }
    let cargo = ProcessCargo;
    let mut attempted = 0;
    let result = (|| {
        for checkout in &checkouts {
            attempted += 1;
            git.update_checkout(
                &checkout.path,
                &checkout.plan.repository,
                &checkout.plan.new_revision,
            )?;
        }

        if !mods_snapshot.still_matches()? {
            return Err(UpdateError::new(
                "mods.toml changed while the update was in progress",
            ));
        }
        for checkout in &checkouts {
            config
                .set_revision(&checkout.plan.name, &checkout.plan.new_revision)
                .map_err(|error| UpdateError::new(error.to_string()))?;
        }
        mods_snapshot.write_candidate(config.render().as_bytes())?;
        let update_result = cargo.update_lock(&manifest);
        lock_snapshot.capture_candidate()?;
        update_result?;
        cargo.verify_locked(&manifest)
    })();

    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(rollback(
            error,
            &mods_snapshot,
            &lock_snapshot,
            &checkouts[..attempted],
            &git,
        )),
    }
}

fn select_entries(entries: &[ModEntry], args: &UpdateArgs) -> Result<Vec<ModEntry>, UpdateError> {
    if args.all {
        let mut selected = Vec::new();
        for entry in entries {
            match &entry.source {
                ModSource::Git { .. } => selected.push(entry.clone()),
                ModSource::Path { .. } => {
                    println!("{} path source skipped", entry.name.as_str());
                }
            }
        }
        return Ok(selected);
    }

    let mut seen = HashSet::new();
    for name in &args.names {
        if !seen.insert(name) {
            return Err(UpdateError::new(format!("duplicate mod name '{name}'")));
        }
    }

    let mut selected = Vec::with_capacity(args.names.len());
    for name in &args.names {
        let entry = entries
            .iter()
            .find(|entry| entry.name.as_str() == name)
            .ok_or_else(|| UpdateError::new(format!("unknown mod '{name}'")))?;
        if matches!(entry.source, ModSource::Path { .. }) {
            return Err(UpdateError::new(format!(
                "mod '{name}' uses a path source and cannot be updated"
            )));
        }
        selected.push(entry.clone());
    }
    Ok(selected)
}

fn print_plan(plans: &[PlannedMod]) {
    for plan in plans {
        if plan.changed() {
            println!(
                "{} {} -> {}",
                plan.name.as_str(),
                plan.old_revision.as_str(),
                plan.new_revision.as_str()
            );
        } else {
            println!(
                "{} {} unchanged",
                plan.name.as_str(),
                plan.old_revision.as_str()
            );
        }
    }
}

fn find_engine_root() -> Result<PathBuf, UpdateError> {
    let current = env::current_dir()
        .map_err(|error| UpdateError::new(format!("failed to read current directory: {error}")))?;
    current
        .ancestors()
        .find(|path| path.join("mods.toml").is_file() && path.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            UpdateError::new("could not locate engine root containing mods.toml and Cargo.toml")
        })
}

fn rollback(
    initiating: UpdateError,
    mods_snapshot: &FileSnapshot,
    lock_snapshot: &FileSnapshot,
    checkouts: &[CheckoutPlan],
    git: &ProcessGit,
) -> UpdateError {
    let mut errors = Vec::new();
    if let Err(error) = mods_snapshot.restore() {
        errors.push(error.to_string());
    }
    if let Err(error) = lock_snapshot.restore() {
        errors.push(error.to_string());
    }
    for checkout in checkouts.iter().rev() {
        if let Err(error) = git.restore_checkout(
            checkout.snapshot.clone(),
            &checkout.plan.repository,
            &checkout.plan.new_revision,
        ) {
            errors.push(error.to_string());
        }
    }

    if errors.is_empty() {
        initiating
    } else {
        UpdateError::new(format!(
            "{initiating}; rollback also failed: {}",
            errors.join("; ")
        ))
    }
}

fn read_file_state(path: &Path) -> Result<(Option<Vec<u8>>, Option<fs::Permissions>), UpdateError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((None, None)),
        Err(error) => {
            return Err(UpdateError::new(format!(
                "failed to inspect {}: {error}",
                path.display()
            )));
        }
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(UpdateError::new(format!(
            "expected a real file: {}",
            path.display()
        )));
    }
    let bytes = fs::read(path)
        .map_err(|error| UpdateError::new(format!("failed to read {}: {error}", path.display())))?;
    Ok((Some(bytes), Some(metadata.permissions())))
}

fn atomic_write(
    path: &Path,
    bytes: &[u8],
    expected_permissions: Option<&fs::Permissions>,
) -> Result<(), UpdateError> {
    let parent = path.parent().ok_or_else(|| {
        UpdateError::new(format!("file has no parent directory: {}", path.display()))
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            UpdateError::new(format!("file name is not valid UTF-8: {}", path.display()))
        })?;
    let permissions = expected_permissions.cloned();

    for attempt in 0..100 {
        let temporary = parent.join(format!(
            ".{file_name}.xtask.{}.{}",
            std::process::id(),
            attempt
        ));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(UpdateError::new(format!(
                    "failed to create {}: {error}",
                    temporary.display()
                )));
            }
        };

        let write_result = file.write_all(bytes).and_then(|()| file.flush());
        let permission_result = permissions
            .clone()
            .map_or(Ok(()), |permissions| file.set_permissions(permissions));
        drop(file);
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(UpdateError::new(format!(
                "failed to write {}: {error}",
                temporary.display()
            )));
        }
        if let Err(error) = permission_result {
            let _ = fs::remove_file(&temporary);
            return Err(UpdateError::new(format!(
                "failed to preserve permissions on {}: {error}",
                temporary.display()
            )));
        }
        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(UpdateError::new(format!(
                "failed to replace {}: {error}",
                path.display()
            )));
        }
        return Ok(());
    }

    Err(UpdateError::new(format!(
        "failed to allocate a temporary file beside {}",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_refuses_to_replace_file_when_candidate_changed() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("mods.toml");
        fs::write(&path, b"original").expect("original");
        let mut snapshot = FileSnapshot::capture(path.clone()).expect("snapshot");
        snapshot
            .write_candidate(b"candidate")
            .expect("write candidate");
        fs::write(&path, b"replacement").expect("replacement");

        let error = snapshot.restore().unwrap_err();

        assert!(error.to_string().contains("candidate"));
        assert_eq!(fs::read(path).expect("read replacement"), b"replacement");
    }

    #[test]
    fn rollback_does_nothing_when_command_never_wrote_file() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("Cargo.lock");
        fs::write(&path, b"original").expect("original");
        let snapshot = FileSnapshot::capture(path.clone()).expect("snapshot");
        fs::write(&path, b"replacement").expect("replacement");

        snapshot.restore().expect("no-op restore");

        assert_eq!(fs::read(path).expect("read replacement"), b"replacement");
    }
}
