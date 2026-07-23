use std::{env, fs, path::Path, path::PathBuf, process::Command};

use super::config::CommitSha;
use super::update::{GitAdapter, UpdateError};

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum CheckoutSnapshot {
    Missing { path: PathBuf },
    Existing { path: PathBuf, head: CommitSha },
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ProcessGit;

impl GitAdapter for ProcessGit {
    fn remote_head(&self, repository: &str) -> Result<CommitSha, UpdateError> {
        self.resolve_remote_head(repository)
    }
}

impl ProcessGit {
    pub(crate) fn resolve_remote_head(&self, repository: &str) -> Result<CommitSha, UpdateError> {
        let output = git_command()
            .args(["ls-remote", "--symref", "--exit-code", repository, "HEAD"])
            .output()
            .map_err(|error| UpdateError::new(format!("failed to run git ls-remote: {error}")))?;

        if !output.status.success() {
            return Err(command_error("git ls-remote", &output));
        }

        let stdout = String::from_utf8(output.stdout)
            .map_err(|_| UpdateError::new("git ls-remote produced non-utf8 output"))?;
        parse_remote_head(repository, &stdout)
    }

    pub(crate) fn inspect_checkout(
        &self,
        path: &Path,
        repository: &str,
    ) -> Result<CheckoutSnapshot, UpdateError> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CheckoutSnapshot::Missing {
                    path: path.to_path_buf(),
                });
            }
            Err(error) => {
                return Err(UpdateError::new(format!(
                    "failed to inspect {}: {error}",
                    path.display()
                )));
            }
        };
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(UpdateError::new(format!(
                "checkout must be a real directory: {}",
                path.display()
            )));
        }

        let git_dir = path.join(".git");
        let git_metadata = fs::symlink_metadata(&git_dir).map_err(|error| {
            UpdateError::new(format!("failed to inspect {}: {error}", git_dir.display()))
        })?;
        if !git_metadata.file_type().is_dir() || git_metadata.file_type().is_symlink() {
            return Err(UpdateError::new(format!(
                "checkout must have a real .git directory: {}",
                path.display()
            )));
        }

        self.ensure_clean(path)?;
        self.ensure_origin(path, repository)?;
        let head = self.current_head(path)?;
        self.ensure_detached(path, &head)?;

        Ok(CheckoutSnapshot::Existing {
            path: path.to_path_buf(),
            head,
        })
    }

    pub(crate) fn update_checkout(
        &self,
        path: &Path,
        repository: &str,
        commit: &CommitSha,
    ) -> Result<CheckoutSnapshot, UpdateError> {
        let snapshot = self.inspect_checkout(path, repository)?;
        match &snapshot {
            CheckoutSnapshot::Missing { path } => {
                let path_arg = path.to_str().ok_or_else(|| {
                    UpdateError::new(format!(
                        "checkout path must be valid UTF-8: {}",
                        path.display()
                    ))
                })?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|error| {
                        UpdateError::new(format!("failed to create {}: {error}", parent.display()))
                    })?;
                }
                let result = (|| {
                    run_git(path.parent(), ["init", "-q", path_arg])?;
                    run_git(Some(path), ["remote", "add", "origin", repository])?;
                    run_git(
                        Some(path),
                        ["fetch", "--depth", "1", "origin", commit.as_str()],
                    )?;
                    run_git(Some(path), ["checkout", "--detach", commit.as_str()])?;
                    self.ensure_head(path, commit, "update")?;
                    self.ensure_detached(path, commit)
                })();
                if let Err(error) = result {
                    return match remove_created_checkout(path) {
                        Ok(()) => Err(error),
                        Err(cleanup_error) => Err(UpdateError::new(format!(
                            "{error}; additionally failed to clean up new checkout: {cleanup_error}"
                        ))),
                    };
                }
            }
            CheckoutSnapshot::Existing { path, .. } => {
                run_git(
                    Some(path),
                    ["fetch", "--depth", "1", "origin", commit.as_str()],
                )?;
                run_git(Some(path), ["checkout", "--detach", commit.as_str()])?;
                self.ensure_head(path, commit, "update")?;
                self.ensure_detached(path, commit)?;
            }
        }
        Ok(snapshot)
    }

    pub(crate) fn restore_checkout(
        &self,
        snapshot: CheckoutSnapshot,
        repository: &str,
        expected_revision: &CommitSha,
    ) -> Result<(), UpdateError> {
        match snapshot {
            CheckoutSnapshot::Missing { path } => {
                match self.inspect_checkout(&path, repository)? {
                    CheckoutSnapshot::Missing { .. } => {}
                    CheckoutSnapshot::Existing { head, .. } => {
                        ensure_expected_revision(&path, &head, expected_revision)?;
                        remove_created_checkout(&path)?;
                    }
                }
            }
            CheckoutSnapshot::Existing { path, head } => {
                let current = self.inspect_checkout(&path, repository)?;
                let CheckoutSnapshot::Existing {
                    head: current_head, ..
                } = current
                else {
                    return Err(UpdateError::new(format!(
                        "refusing rollback because checkout is missing: {}",
                        path.display()
                    )));
                };
                ensure_expected_revision(&path, &current_head, expected_revision)?;
                run_git(Some(&path), ["checkout", "--detach", head.as_str()])?;
                self.ensure_head(&path, &head, "restore")?;
                self.ensure_detached(&path, &head)?;
            }
        }
        Ok(())
    }

    fn current_head(&self, path: &Path) -> Result<CommitSha, UpdateError> {
        let output = run_git_output(Some(path), ["rev-parse", "--verify", "HEAD"])?;
        let head = output.trim();
        if !is_sha40(head) {
            return Err(UpdateError::new(format!(
                "checkout head is not a 40-character sha: {}",
                path.display()
            )));
        }
        Ok(CommitSha::new(head.to_string()))
    }

    fn ensure_detached(&self, path: &Path, head: &CommitSha) -> Result<(), UpdateError> {
        let output = git_command()
            .current_dir(path)
            .args(["symbolic-ref", "-q", "HEAD"])
            .output()
            .map_err(|error| UpdateError::new(format!("failed to run git: {error}")))?;
        match output.status.code() {
            Some(0) => Err(UpdateError::new(format!(
                "checkout must be detached at {}: {}",
                head.as_str(),
                path.display()
            ))),
            Some(1) => Ok(()),
            _ => Err(command_error("git symbolic-ref", &output)),
        }
    }

    fn ensure_head(
        &self,
        path: &Path,
        expected: &CommitSha,
        operation: &str,
    ) -> Result<(), UpdateError> {
        let actual = self.current_head(path)?;
        if actual == *expected {
            return Ok(());
        }
        Err(UpdateError::new(format!(
            "checkout HEAD mismatch after {operation}: expected {}, got {}: {}",
            expected.as_str(),
            actual.as_str(),
            path.display()
        )))
    }

    fn ensure_clean(&self, path: &Path) -> Result<(), UpdateError> {
        let output = run_git_output(
            Some(path),
            ["status", "--porcelain=v2", "--untracked-files=all"],
        )?;
        if !output.trim().is_empty() {
            return Err(UpdateError::new(format!(
                "checkout must be clean, including untracked files: {}",
                path.display()
            )));
        }
        Ok(())
    }

    fn ensure_origin(&self, path: &Path, repository: &str) -> Result<(), UpdateError> {
        let output = run_git_output(Some(path), ["remote", "get-url", "origin"])?;
        if normalize_remote(output.trim()) != normalize_remote(repository) {
            return Err(UpdateError::new(format!(
                "checkout origin does not match repository: {}",
                path.display()
            )));
        }
        Ok(())
    }
}

fn ensure_expected_revision(
    path: &Path,
    actual: &CommitSha,
    expected: &CommitSha,
) -> Result<(), UpdateError> {
    if actual == expected {
        return Ok(());
    }
    Err(UpdateError::new(format!(
        "refusing rollback because checkout no longer matches expected revision {} (found {}): {}",
        expected.as_str(),
        actual.as_str(),
        path.display()
    )))
}

fn parse_remote_head(repository: &str, stdout: &str) -> Result<CommitSha, UpdateError> {
    let mut symbolic_head = None;
    let mut commits = Vec::new();

    for line in stdout.lines() {
        let Some((left, right)) = line.split_once('\t') else {
            continue;
        };
        if right != "HEAD" {
            continue;
        }
        if let Some(reference) = left.strip_prefix("ref: ") {
            let is_branch = reference
                .strip_prefix("refs/heads/")
                .is_some_and(|branch| !branch.is_empty());
            if !is_branch || symbolic_head.replace(reference).is_some() {
                return Err(UpdateError::new(format!(
                    "git ls-remote did not report exactly one symbolic branch for HEAD in {repository}"
                )));
            }
        } else if is_sha40(left) {
            commits.push(left);
        } else {
            return Err(UpdateError::new(format!(
                "git ls-remote reported an invalid HEAD record for {repository}"
            )));
        }
    }

    if symbolic_head.is_none() {
        return Err(UpdateError::new(format!(
            "git ls-remote did not report exactly one symbolic branch for HEAD in {repository}"
        )));
    }
    if commits.len() != 1 {
        return Err(UpdateError::new(format!(
            "git ls-remote did not report exactly one HEAD commit for {repository}"
        )));
    }

    Ok(CommitSha::new(commits[0].to_string()))
}

fn remove_created_checkout(path: &Path) -> Result<(), UpdateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path).map_err(|error| {
                UpdateError::new(format!("failed to remove {}: {error}", path.display()))
            })
        }
        Ok(_) => Err(UpdateError::new(format!(
            "refusing to remove non-directory checkout path: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(UpdateError::new(format!(
            "failed to inspect {} for cleanup: {error}",
            path.display()
        ))),
    }
}

fn command_error(operation: &str, output: &std::process::Output) -> UpdateError {
    UpdateError::new(format!(
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    sanitize_git_environment(&mut command);
    command
}

fn sanitize_git_environment(command: &mut Command) {
    let environment = env::vars_os().filter(|(key, _)| !key.to_string_lossy().starts_with("GIT_"));
    command.env_clear().envs(environment);
}

fn run_git_output<'a, I>(current_dir: Option<&Path>, args: I) -> Result<String, UpdateError>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut command = git_command();
    if let Some(dir) = current_dir {
        command.current_dir(dir);
    }
    let output = command
        .args(args)
        .output()
        .map_err(|error| UpdateError::new(format!("failed to run git: {error}")))?;
    if !output.status.success() {
        return Err(command_error("git", &output));
    }
    String::from_utf8(output.stdout).map_err(|_| UpdateError::new("git produced non-utf8 output"))
}

fn run_git<'a, I>(current_dir: Option<&Path>, args: I) -> Result<(), UpdateError>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut command = git_command();
    if let Some(dir) = current_dir {
        command.current_dir(dir);
    }
    let output = command
        .args(args)
        .output()
        .map_err(|error| UpdateError::new(format!("failed to run git: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(command_error("git", &output))
}

fn normalize_remote(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    trimmed.strip_suffix(".git").unwrap_or(trimmed).to_string()
}

fn is_sha40(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(args: &[&str], current_dir: Option<&Path>) {
        let mut command = git_command();
        if let Some(dir) = current_dir {
            command.current_dir(dir);
        }
        let output = command.args(args).output().expect("git command");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_output(args: &[&str], current_dir: Option<&Path>) -> String {
        let mut command = git_command();
        if let Some(dir) = current_dir {
            command.current_dir(dir);
        }
        let output = command.args(args).output().expect("git command");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("utf8 git output")
    }

    fn init_remote() -> (tempfile::TempDir, PathBuf, CommitSha) {
        let temp = tempfile::tempdir().expect("tempdir");
        let worktree = temp.path().join("remote-worktree");
        let bare = temp.path().join("remote.git");

        git(
            ["init", "--bare", bare.to_str().expect("bare path")].as_slice(),
            None,
        );
        git(
            [
                "init",
                "-b",
                "main",
                worktree.to_str().expect("worktree path"),
            ]
            .as_slice(),
            None,
        );
        git(
            ["config", "user.email", "test@example.com"].as_slice(),
            Some(&worktree),
        );
        git(
            ["config", "user.name", "Test User"].as_slice(),
            Some(&worktree),
        );
        fs::write(worktree.join("README.md"), "fixture\n").expect("write fixture");
        git(["add", "README.md"].as_slice(), Some(&worktree));
        git(["commit", "-m", "initial"].as_slice(), Some(&worktree));
        git(
            ["remote", "add", "origin", bare.to_str().expect("bare path")].as_slice(),
            Some(&worktree),
        );
        git(["push", "-u", "origin", "main"].as_slice(), Some(&worktree));
        git(
            ["symbolic-ref", "HEAD", "refs/heads/main"].as_slice(),
            Some(&bare),
        );
        let head = git_output(["rev-parse", "HEAD"].as_slice(), Some(&worktree));
        (temp, bare, CommitSha::new(head.trim().to_string()))
    }

    fn add_remote_commit(temp: &tempfile::TempDir) -> CommitSha {
        let worktree = temp.path().join("remote-worktree");
        fs::write(worktree.join("CHANGE.md"), "change\n").expect("write change");
        git(["add", "CHANGE.md"].as_slice(), Some(&worktree));
        git(["commit", "-m", "change"].as_slice(), Some(&worktree));
        git(["push", "origin", "main"].as_slice(), Some(&worktree));
        let head = git_output(["rev-parse", "HEAD"].as_slice(), Some(&worktree));
        CommitSha::new(head.trim().to_string())
    }

    #[test]
    fn resolve_remote_head_reads_symbolic_head_record() {
        let (_temp, bare, head) = init_remote();
        let git = ProcessGit;

        let resolved = git
            .resolve_remote_head(bare.to_str().expect("bare path"))
            .expect("remote head");

        assert_eq!(resolved, head);
    }

    #[test]
    fn remote_head_parser_rejects_duplicate_oid_records() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let output = format!("ref: refs/heads/main\tHEAD\n{oid}\tHEAD\n{oid}\tHEAD\n");

        let error = parse_remote_head("fixture", &output).unwrap_err();

        assert!(error.to_string().contains("exactly one HEAD commit"));
    }

    #[test]
    fn update_checkout_and_restore_existing_checkout() {
        let (temp, bare, original_head) = init_remote();
        let root = tempfile::tempdir().expect("root");
        let checkout = root.path().join("mods").join("combat-core");
        let adapter = ProcessGit;
        git(
            [
                "clone",
                bare.to_str().expect("bare path"),
                checkout.to_str().expect("checkout"),
            ]
            .as_slice(),
            None,
        );
        git(
            ["checkout", "--detach", original_head.as_str()].as_slice(),
            Some(&checkout),
        );
        let updated_head = add_remote_commit(&temp);

        let snapshot = adapter
            .update_checkout(&checkout, bare.to_str().expect("bare path"), &updated_head)
            .expect("update checkout");

        assert!(checkout.is_dir());
        assert!(checkout.join(".git").is_dir());
        assert_eq!(adapter.current_head(&checkout).expect("head"), updated_head);
        assert!(
            git_output(
                ["status", "--porcelain=v2", "--untracked-files=all"].as_slice(),
                Some(&checkout)
            )
            .trim()
            .is_empty()
        );

        adapter
            .restore_checkout(snapshot, bare.to_str().expect("bare path"), &updated_head)
            .expect("restore");

        assert!(checkout.is_dir());
        assert_eq!(
            adapter.current_head(&checkout).expect("head"),
            original_head
        );
    }

    #[test]
    fn update_checkout_can_create_and_remove_missing_checkout() {
        let (_temp, bare, head) = init_remote();
        let root = tempfile::tempdir().expect("root");
        let checkout = root.path().join("mods").join("combat-core");
        let git = ProcessGit;

        let snapshot = git
            .update_checkout(&checkout, bare.to_str().expect("bare path"), &head)
            .expect("update checkout");
        assert!(checkout.is_dir());

        git.restore_checkout(snapshot, bare.to_str().expect("bare path"), &head)
            .expect("restore");
        assert!(!checkout.exists());
    }

    #[test]
    fn sanitized_git_command_ignores_hostile_repository_environment() {
        let (_temp, bare, head) = init_remote();
        let root = tempfile::tempdir().expect("root");
        let checkout = root.path().join("checkout");
        git(
            [
                "clone",
                bare.to_str().expect("bare path"),
                checkout.to_str().expect("checkout"),
            ]
            .as_slice(),
            None,
        );
        let hostile = root.path().join("hostile.git");
        git(
            ["init", "--bare", hostile.to_str().expect("hostile path")].as_slice(),
            None,
        );

        let mut command = git_command();
        command
            .env("GIT_DIR", &hostile)
            .env("GIT_WORK_TREE", root.path().join("hostile-worktree"));
        sanitize_git_environment(&mut command);
        let output = command
            .current_dir(&checkout)
            .args(["checkout", "--detach", head.as_str()])
            .output()
            .expect("git checkout");

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(ProcessGit.current_head(&checkout).expect("head"), head);
    }

    #[test]
    fn restore_missing_checkout_refuses_replacement_head() {
        let (temp, bare, original_head) = init_remote();
        let root = tempfile::tempdir().expect("root");
        let checkout = root.path().join("mods").join("combat-core");
        let adapter = ProcessGit;
        let snapshot = adapter
            .update_checkout(&checkout, bare.to_str().expect("bare path"), &original_head)
            .expect("create checkout");
        let replacement_head = add_remote_commit(&temp);
        git(
            ["fetch", "--depth", "1", "origin", replacement_head.as_str()].as_slice(),
            Some(&checkout),
        );
        git(
            ["checkout", "--detach", replacement_head.as_str()].as_slice(),
            Some(&checkout),
        );

        let error = adapter
            .restore_checkout(snapshot, bare.to_str().expect("bare path"), &original_head)
            .unwrap_err();

        assert!(error.to_string().contains("expected revision"));
        assert!(checkout.exists());
        assert_eq!(
            adapter.current_head(&checkout).expect("replacement head"),
            replacement_head
        );
    }

    #[test]
    fn failed_new_checkout_is_removed() {
        let (_temp, bare, _head) = init_remote();
        let root = tempfile::tempdir().expect("root");
        let checkout = root.path().join("mods").join("combat-core");
        let missing_commit = CommitSha::new("ffffffffffffffffffffffffffffffffffffffff".into());

        ProcessGit
            .update_checkout(
                &checkout,
                bare.to_str().expect("bare path"),
                &missing_commit,
            )
            .unwrap_err();

        assert!(!checkout.exists());
    }

    #[cfg(unix)]
    #[test]
    fn inspect_checkout_rejects_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let checkout = root.path().join("combat-core");
        symlink(root.path().join("missing"), &checkout).expect("dangling symlink");

        let error = ProcessGit
            .inspect_checkout(&checkout, "fixture")
            .unwrap_err();

        assert!(error.to_string().contains("real directory"));
    }

    #[cfg(unix)]
    #[test]
    fn update_checkout_rejects_non_utf8_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let root = tempfile::tempdir().expect("root");
        let checkout = root.path().join(OsString::from_vec(vec![0xff]));
        let commit = CommitSha::new("0123456789abcdef0123456789abcdef01234567".into());

        let error = ProcessGit
            .update_checkout(&checkout, "fixture", &commit)
            .unwrap_err();

        assert!(error.to_string().contains("valid UTF-8"));
        assert!(!checkout.exists());
    }

    #[test]
    fn ensure_detached_propagates_symbolic_ref_errors() {
        let root = tempfile::tempdir().expect("root");
        let head = CommitSha::new("0123456789abcdef0123456789abcdef01234567".into());

        let error = ProcessGit.ensure_detached(root.path(), &head).unwrap_err();

        assert!(error.to_string().contains("git symbolic-ref failed"));
    }

    #[test]
    fn inspect_checkout_rejects_dirty_worktree() {
        let (_temp, bare, head) = init_remote();
        let root = tempfile::tempdir().expect("root");
        let checkout = root.path().join("mods").join("combat-core");
        git(
            [
                "clone",
                bare.to_str().expect("bare path"),
                checkout.to_str().expect("checkout"),
            ]
            .as_slice(),
            None,
        );
        git(
            ["checkout", "--detach", head.as_str()].as_slice(),
            Some(&checkout),
        );
        fs::write(checkout.join("untracked.txt"), "dirty\n").expect("write dirty");
        let git = ProcessGit;

        let error = git
            .inspect_checkout(&checkout, bare.to_str().expect("bare path"))
            .unwrap_err();
        assert!(error.to_string().contains("untracked"));
    }
}
