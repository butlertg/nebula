//! Git worktree operations — shelled out to the `git` CLI on purpose:
//! libgit2's worktree support lags git's, these are rare user-initiated ops,
//! and git's stderr is the best error message we could show.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Shown when the `git` binary itself is missing. Every other git failure
/// carries git's own stderr; this one git never gets to print, so spelling out
/// the fix is on us — otherwise the user sees "No such file or directory" and
/// blames the directory they just picked. Kept to one line: the TUI shows it
/// in the footer flash, which truncates.
pub const GIT_MISSING: &str =
    "git was not found on your PATH — nebula needs it. Install git (https://git-scm.com/downloads), then restart nebula.";

/// True when `err` came from `git` being absent, so callers can pass the
/// message through instead of layering their own (wrong) explanation on top.
pub fn is_missing(err: &anyhow::Error) -> bool {
    err.chain().any(|c| c.to_string() == GIT_MISSING)
}

/// `git` never even started. NotFound means the binary isn't installed — the
/// one git failure with no stderr to quote, so the explanation has to be ours.
fn spawn_err(e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        anyhow!(GIT_MISSING)
    } else {
        anyhow::Error::new(e).context("run git")
    }
}

async fn git(repo: &Path, args: &[&str]) -> Result<String> {
    git_with_index(repo, args, None).await
}

/// `git`, optionally against a scratch index file instead of the repo's own.
/// Staging into a throwaway index is what lets the snapshot helpers read the
/// working tree without touching what the user has staged.
async fn git_with_index(repo: &Path, args: &[&str], index: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let output = cmd.output().await.map_err(spawn_err)?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `git init` an existing directory.
pub async fn init(path: &Path) -> Result<()> {
    git(path, &["init"]).await?;
    Ok(())
}

/// Verify `path` is inside a git repo and return its toplevel.
pub async fn repo_toplevel(path: &Path) -> Result<PathBuf> {
    let out = git(path, &["rev-parse", "--show-toplevel"]).await?;
    Ok(PathBuf::from(out.trim()))
}

pub async fn current_branch(repo: &Path) -> Result<String> {
    let out = git(repo, &["branch", "--show-current"]).await?;
    let branch = out.trim();
    if branch.is_empty() {
        // Detached HEAD — fall back to the short hash.
        let hash = git(repo, &["rev-parse", "--short", "HEAD"]).await?;
        return Ok(format!("detached@{}", hash.trim()));
    }
    Ok(branch.to_string())
}

#[derive(Debug, Clone)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub branch: String,
}

/// Parse `git worktree list --porcelain`. The first entry is the main
/// checkout.
pub async fn list_worktrees(repo: &Path) -> Result<Vec<WorktreeEntry>> {
    let out = git(repo, &["worktree", "list", "--porcelain"]).await?;
    let mut entries = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut head: Option<String> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(done_path) = path.take() {
                entries.push(WorktreeEntry {
                    path: done_path,
                    branch: branch
                        .take()
                        .unwrap_or_else(|| detached_label(head.as_deref())),
                });
            }
            head = None;
            path = Some(PathBuf::from(p));
        } else if let Some(sha) = line.strip_prefix("HEAD ") {
            head = Some(sha.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.trim_start_matches("refs/heads/").to_string());
        }
    }
    if let Some(done_path) = path {
        entries.push(WorktreeEntry {
            path: done_path,
            branch: branch.unwrap_or_else(|| detached_label(head.as_deref())),
        });
    }
    Ok(entries)
}

/// Display name for a checkout with no branch (detached HEAD).
fn detached_label(head: Option<&str>) -> String {
    match head {
        Some(sha) => format!("detached @ {}", &sha[..sha.len().min(7)]),
        None => "(detached)".into(),
    }
}

/// Directory a new worktree for `branch` should live in:
/// `<repo>/../<repo-name>-worktrees/<branch>` (slashes in branch → dashes).
pub fn worktree_dir(repo: &Path, branch: &str) -> PathBuf {
    let repo_name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let safe_branch = branch.replace('/', "-");
    repo.parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
        .join(format!("{repo_name}-worktrees"))
        .join(safe_branch)
}

/// `git worktree add <path> -b <branch> [base]`. Falls back to checking out an
/// existing branch when `-b` fails because it already exists.
pub async fn add_worktree(repo: &Path, branch: &str, base: Option<&str>) -> Result<PathBuf> {
    let path = worktree_dir(repo, branch);
    if path.exists() {
        bail!("worktree path already exists: {}", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path_str = path.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "add", &path_str, "-b", branch];
    if let Some(base) = base {
        args.push(base);
    }
    match git(repo, &args).await {
        Ok(_) => Ok(path),
        Err(e) if e.to_string().contains("already exists") => {
            // Branch exists: check it out instead of creating.
            git(repo, &["worktree", "add", &path_str, branch]).await?;
            Ok(path)
        }
        Err(e) => Err(e),
    }
}

pub async fn remove_worktree(repo: &Path, worktree_path: &Path, force: bool) -> Result<()> {
    // Checkout already gone (manual rm -rf): `git worktree remove` would fail,
    // but the user's intent is already satisfied — just drop git's stale
    // bookkeeping so the entry leaves `git worktree list`.
    if !worktree_path.exists() {
        let _ = git(repo, &["worktree", "prune"]).await;
        return Ok(());
    }
    let path_str = worktree_path.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path_str);
    match git(repo, &args).await {
        Ok(_) => Ok(()),
        // Directory exists but git no longer tracks it as a worktree (already
        // pruned, or its .git link was destroyed). Nothing for git to remove;
        // prune any leftover metadata and let the caller drop its row. The
        // directory itself is left alone — deleting an untracked dir is not
        // ours to do.
        Err(e)
            if e.to_string().contains("is not a working tree")
                || e.to_string().contains("does not exist") =>
        {
            let _ = git(repo, &["worktree", "prune"]).await;
            Ok(())
        }
        // Locked by a session that ran `git worktree lock` (Claude Code locks
        // its worktree and a killed session never unlocks). The caller has
        // already killed this worktree's sessions, so the lock is stale —
        // unlock and retry rather than surfacing git's refusal.
        Err(e) if e.to_string().contains("locked working tree") => {
            git(repo, &["worktree", "unlock", &path_str]).await?;
            git(repo, &args).await?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Record the working tree as a commit on `branch`, changing nothing about
/// the checkout itself.
///
/// This is deliberately not `checkout -b && add && commit`: an unattended
/// run finishes at 3am in a checkout the user may come back to, and leaving
/// them on a different branch — or with a swept-up index — is not something
/// they asked for. Instead the tree is staged into a scratch index and
/// written with plumbing, so HEAD, the real index, and every file on disk
/// are exactly as the run left them. The commit is reachable only from the
/// new branch.
///
/// Returns the new commit's short hash, or `None` when the tree is identical
/// to HEAD and there is nothing worth recording.
pub async fn snapshot_branch(repo: &Path, branch: &str, message: &str) -> Result<Option<String>> {
    let Some(commit) = snapshot_commit(repo, message, false).await? else {
        return Ok(None);
    };
    // `update-ref` rather than `branch`: it does not care that we are not on
    // the branch, and it fails loudly if the name is already taken.
    write_ref(repo, &format!("refs/heads/{branch}"), &commit).await?;
    let short = git(repo, &["rev-parse", "--short", &commit]).await?;
    Ok(Some(short.trim().to_string()))
}

/// Record the working tree under `refname`, which is a full ref path — the
/// run refs (`refs/nebula/runs/<id>/base`) live outside `refs/heads` so they
/// never show up in a branch list, and holding a ref is also what stops gc
/// from collecting the commit months later.
///
/// Unlike [`snapshot_branch`] this always writes a commit, even when the
/// tree matches HEAD: the pair of refs is what the run's diff is computed
/// from, and a missing base means no diff at all rather than an empty one.
/// Returns the full commit sha.
pub async fn snapshot_ref(repo: &Path, refname: &str, message: &str) -> Result<String> {
    let commit = snapshot_commit(repo, message, true)
        .await?
        .context("an allow-empty snapshot always produces a commit")?;
    write_ref(repo, refname, &commit).await?;
    Ok(commit)
}

async fn write_ref(repo: &Path, refname: &str, commit: &str) -> Result<()> {
    git(repo, &["update-ref", refname, commit]).await?;
    Ok(())
}

/// Commit the working tree as it stands without touching the checkout, and
/// return the new commit's full sha. `None` only when `allow_empty` is false
/// and the tree is identical to HEAD.
///
/// This is deliberately not `checkout -b && add && commit`: an unattended
/// run finishes at 3am in a checkout the user may come back to, and leaving
/// them on a different branch — or with a swept-up index — is not something
/// they asked for. Instead the tree is staged into a scratch index and
/// written with plumbing, so HEAD, the real index, and every file on disk
/// are exactly as the run left them. The commit is reachable only from the
/// ref the caller then writes.
async fn snapshot_commit(repo: &Path, message: &str, allow_empty: bool) -> Result<Option<String>> {
    // A commit needs a parent to diff against, and an unborn HEAD has none.
    // Nothing has ever been committed here, so there is no run to capture.
    let head = git(repo, &["rev-parse", "HEAD"])
        .await
        .map_err(|e| anyhow!("the checkout has no commits to build on ({e})"))?;
    let head = head.trim().to_string();

    // Scratch index, next to the repo's own git dir rather than in the
    // worktree — it must not show up as an untracked file in the very tree
    // we are about to stage. The pid keeps two concurrent runs apart.
    let common = git(repo, &["rev-parse", "--git-common-dir"]).await?;
    let common = repo.join(common.trim());
    let index = common.join(format!("nebula-snapshot-{}.index", std::process::id()));
    let _ = tokio::fs::remove_file(&index).await;

    let result = snapshot_into(repo, message, &head, &index, allow_empty).await;
    // Best effort: a leftover scratch index is harmless (git only reads it
    // when GIT_INDEX_FILE points at it) but there is no reason to keep one.
    let _ = tokio::fs::remove_file(&index).await;
    result
}

async fn snapshot_into(
    repo: &Path,
    message: &str,
    head: &str,
    index: &Path,
    allow_empty: bool,
) -> Result<Option<String>> {
    let idx = Some(index);
    // Seed from HEAD so unchanged files keep their stat cache, then let
    // `add -A` fold in every modification, addition and deletion. `-A`
    // still honours .gitignore, so build output stays out.
    git_with_index(repo, &["read-tree", head], idx).await?;
    git_with_index(repo, &["add", "-A"], idx).await?;
    let tree = git_with_index(repo, &["write-tree"], idx).await?;
    let tree = tree.trim().to_string();

    // Identical trees means the run touched nothing that git tracks; a
    // commit there would be an empty entry in a log the user has to read.
    let head_tree = git(repo, &["rev-parse", &format!("{head}^{{tree}}")]).await?;
    if tree == head_tree.trim() && !allow_empty {
        return Ok(None);
    }

    let commit = git_with_index(
        repo,
        &["commit-tree", &tree, "-p", head, "-m", message],
        idx,
    )
    .await?;
    Ok(Some(commit.trim().to_string()))
}

/// One file the run touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// git's status letter: `A`, `M`, `D`, `R100`, …
    pub status: String,
    pub path: String,
    pub insertions: u32,
    pub deletions: u32,
}

/// What changed between two commits, in the shape a report wants.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffSummary {
    pub files: Vec<FileChange>,
    pub insertions: u32,
    pub deletions: u32,
}

/// `git diff <from> <to>`, summarised. Two plumbing calls rather than one:
/// `--numstat` has the line counts and `--name-status` has the letters, and
/// no single format carries both. Both run with `-z`, so a path with a
/// space, a quote, or a newline in it survives.
pub async fn diff_summary(repo: &Path, from: &str, to: &str) -> Result<DiffSummary> {
    let numstat = git(repo, &["diff", "--numstat", "-M", "-z", from, to]).await?;
    let names = git(repo, &["diff", "--name-status", "-M", "-z", from, to]).await?;
    Ok(merge_diff(&numstat, &names))
}

/// `--numstat -z`: `<adds> TAB <dels> TAB <path> NUL`, and for a rename
/// `<adds> TAB <dels> TAB NUL <old> NUL <new> NUL` — the empty path field is
/// the tell. A binary file reports `-` for both counts.
fn parse_numstat(raw: &str) -> Vec<(String, u32, u32)> {
    let mut out = Vec::new();
    let mut fields = raw.split('\0');
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        let mut parts = field.splitn(3, '\t');
        let (Some(adds), Some(dels), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let count = |s: &str| s.parse::<u32>().unwrap_or(0);
        let path = if path.is_empty() {
            // Rename: the old name comes next, then the new one, which is
            // the one worth reporting.
            let _old = fields.next();
            match fields.next() {
                Some(new) => new.to_string(),
                None => continue,
            }
        } else {
            path.to_string()
        };
        out.push((path, count(adds), count(dels)));
    }
    out
}

/// `--name-status -z`: `<status> NUL <path> NUL`, and for a rename
/// `R<score> NUL <old> NUL <new> NUL`.
fn parse_name_status(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    while let Some(status) = fields.next() {
        let Some(path) = fields.next() else { break };
        let path = if status.starts_with('R') || status.starts_with('C') {
            match fields.next() {
                Some(new) => new.to_string(),
                None => break,
            }
        } else {
            path.to_string()
        };
        out.push((status.to_string(), path));
    }
    out
}

/// Line counts joined to status letters by path, keeping numstat's order —
/// pairing the two lists by position would mis-label every file after the
/// first one the formats disagree about.
fn merge_diff(numstat: &str, name_status: &str) -> DiffSummary {
    let statuses = parse_name_status(name_status);
    let files: Vec<FileChange> = parse_numstat(numstat)
        .into_iter()
        .map(|(path, insertions, deletions)| {
            let status = statuses
                .iter()
                .find(|(_, p)| *p == path)
                .map(|(s, _)| s.clone())
                .unwrap_or_else(|| "M".to_string());
            FileChange {
                status,
                path,
                insertions,
                deletions,
            }
        })
        .collect();
    DiffSummary {
        insertions: files.iter().map(|f| f.insertions).sum(),
        deletions: files.iter().map(|f| f.deletions).sum(),
        files,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn init_repo(dir: &Path) {
        git(dir, &["init", "-b", "main"]).await.unwrap();
        git(dir, &["config", "user.email", "t@t"]).await.unwrap();
        git(dir, &["config", "user.name", "t"]).await.unwrap();
        git(dir, &["commit", "--allow-empty", "-m", "init"])
            .await
            .unwrap();
    }

    #[test]
    fn missing_git_binary_explains_the_install() {
        let err = spawn_err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        ));
        assert!(is_missing(&err), "{err:#}");
        assert!(err.to_string().contains("Install git"));
        // Still recognized once a caller layers its own context on top.
        assert!(is_missing(&err.context("open /some/dir")));
    }

    #[test]
    fn other_spawn_failures_are_not_reported_as_missing_git() {
        let err = spawn_err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Permission denied (os error 13)",
        ));
        assert!(!is_missing(&err), "{err:#}");
    }

    #[tokio::test]
    async fn git_errors_are_not_reported_as_missing_git() {
        let tmp = tempfile::tempdir().unwrap();
        // A real git that says "not a repository" must keep saying so.
        let err = repo_toplevel(tmp.path()).await.unwrap_err();
        assert!(!is_missing(&err), "{err:#}");
    }

    /// The whole point of the plumbing route: an unattended run finishes in
    /// a checkout the user comes back to in the morning, so capturing it
    /// must leave HEAD, the branch, the index and the files exactly as they
    /// were. Only a new ref appears.
    #[tokio::test]
    async fn snapshot_branch_records_the_tree_without_disturbing_the_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo).await;

        // Something committed, something the "run" changed, something it
        // added, and something staged by the user beforehand.
        std::fs::write(repo.join("tracked.txt"), "one\n").unwrap();
        git(&repo, &["add", "."]).await.unwrap();
        git(&repo, &["commit", "-m", "tracked"]).await.unwrap();
        std::fs::write(repo.join("tracked.txt"), "two\n").unwrap();
        std::fs::write(repo.join("new.txt"), "fresh\n").unwrap();
        std::fs::write(repo.join("staged.txt"), "staged\n").unwrap();
        git(&repo, &["add", "staged.txt"]).await.unwrap();

        let head_before = git(&repo, &["rev-parse", "HEAD"]).await.unwrap();
        let branch_before = current_branch(&repo).await.unwrap();
        let status_before = git(&repo, &["status", "--porcelain"]).await.unwrap();

        let hash = snapshot_branch(&repo, "task/nightly/1", "captured")
            .await
            .unwrap()
            .expect("the tree differs from HEAD, so there is a commit");

        // Nothing about the checkout moved.
        assert_eq!(
            git(&repo, &["rev-parse", "HEAD"]).await.unwrap(),
            head_before
        );
        assert_eq!(current_branch(&repo).await.unwrap(), branch_before);
        assert_eq!(
            git(&repo, &["status", "--porcelain"]).await.unwrap(),
            status_before,
            "the user's staged and unstaged work is untouched"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked.txt")).unwrap(),
            "two\n"
        );

        // The branch exists, is a child of HEAD, and holds the working tree.
        let listed = git(&repo, &["show", "--stat", "--oneline", "task/nightly/1"])
            .await
            .unwrap();
        assert!(listed.contains(&hash), "{listed}");
        let shown = git(&repo, &["show", "task/nightly/1:tracked.txt"])
            .await
            .unwrap();
        assert_eq!(shown, "two\n");
        let added = git(&repo, &["show", "task/nightly/1:new.txt"])
            .await
            .unwrap();
        assert_eq!(added, "fresh\n");
        let parent = git(&repo, &["rev-parse", "task/nightly/1^"]).await.unwrap();
        assert_eq!(parent, head_before);

        // No scratch index left lying around.
        let leftovers: Vec<_> = std::fs::read_dir(repo.join(".git"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("nebula-snapshot-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// A run that changed nothing must not leave an empty commit in a log
    /// the user has to read every morning.
    /// The pair of refs a run is diffed between has to exist even when the
    /// run changed nothing — otherwise "it did nothing" is indistinguishable
    /// from "nebula lost track of it".
    #[tokio::test]
    async fn snapshot_ref_records_a_commit_even_on_a_clean_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo).await;
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        git(&repo, &["add", "."]).await.unwrap();
        git(&repo, &["commit", "-m", "a"]).await.unwrap();
        let head_before = git(&repo, &["rev-parse", "HEAD"]).await.unwrap();

        let sha = snapshot_ref(&repo, "refs/nebula/runs/r1/base", "before")
            .await
            .unwrap();
        assert_eq!(
            git(&repo, &["rev-parse", "refs/nebula/runs/r1/base"])
                .await
                .unwrap()
                .trim(),
            sha,
            "the ref points at the commit"
        );
        assert_eq!(
            git(&repo, &["rev-parse", "HEAD"]).await.unwrap(),
            head_before,
            "the checkout never moved"
        );
        assert!(
            !git(&repo, &["branch", "--list"])
                .await
                .unwrap()
                .contains("runs/r1"),
            "a run ref is not a branch"
        );
    }

    /// The whole point of the base/head pair: the diff is the run's own work,
    /// and what the checkout was already carrying is on both sides of it.
    #[tokio::test]
    async fn a_run_diff_excludes_what_was_already_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo).await;
        std::fs::write(repo.join("tracked.txt"), "one\n").unwrap();
        std::fs::write(repo.join("old.txt"), "gone soon\n").unwrap();
        git(&repo, &["add", "."]).await.unwrap();
        git(&repo, &["commit", "-m", "tracked"]).await.unwrap();

        // Somebody's uncommitted work, present before the run starts.
        std::fs::write(repo.join("USER_WIP.txt"), "mine\n").unwrap();
        let base = snapshot_ref(&repo, "refs/nebula/runs/r2/base", "before")
            .await
            .unwrap();

        // What the "run" then did: edit, add, delete.
        std::fs::write(repo.join("tracked.txt"), "one\ntwo\n").unwrap();
        std::fs::write(repo.join("new.txt"), "fresh\n").unwrap();
        std::fs::remove_file(repo.join("old.txt")).unwrap();
        let head = snapshot_ref(&repo, "refs/nebula/runs/r2/head", "after")
            .await
            .unwrap();

        let diff = diff_summary(&repo, &base, &head).await.unwrap();
        let paths: Vec<&str> = diff.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            !paths.contains(&"USER_WIP.txt"),
            "the pre-existing file is on both sides: {paths:?}"
        );
        assert_eq!(paths.len(), 3, "{paths:?}");
        let by_path = |p: &str| diff.files.iter().find(|f| f.path == p).unwrap().clone();
        assert_eq!(by_path("tracked.txt").status, "M");
        assert_eq!(by_path("tracked.txt").insertions, 1);
        assert_eq!(by_path("new.txt").status, "A");
        assert_eq!(by_path("old.txt").status, "D");
        assert_eq!(diff.insertions, 2);
        assert_eq!(diff.deletions, 1);
    }

    /// A rename reports the name the file ended up with — the one somebody
    /// would go looking for — not the one it left behind.
    #[tokio::test]
    async fn a_renamed_file_is_reported_under_its_new_name() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo).await;
        std::fs::write(repo.join("before.txt"), "same contents\nline two\n").unwrap();
        git(&repo, &["add", "."]).await.unwrap();
        git(&repo, &["commit", "-m", "one"]).await.unwrap();
        let base = snapshot_ref(&repo, "refs/nebula/runs/r3/base", "before")
            .await
            .unwrap();
        std::fs::rename(repo.join("before.txt"), repo.join("after.txt")).unwrap();
        let head = snapshot_ref(&repo, "refs/nebula/runs/r3/head", "after")
            .await
            .unwrap();

        let diff = diff_summary(&repo, &base, &head).await.unwrap();
        assert_eq!(diff.files.len(), 1, "{:?}", diff.files);
        assert_eq!(diff.files[0].path, "after.txt");
        assert!(diff.files[0].status.starts_with('R'), "{:?}", diff.files[0]);
    }

    /// The two `-z` formats are parsed, not eyeballed: a path with a space in
    /// it and a rename both have to survive the join.
    #[test]
    fn the_two_diff_formats_join_on_the_path() {
        let numstat = "3\t1\tsrc/a b.rs\0".to_string() + "0\t0\t\0old.rs\0new.rs\0";
        let names = "M\0src/a b.rs\0R100\0old.rs\0new.rs\0";
        let merged = merge_diff(&numstat, names);
        assert_eq!(merged.files.len(), 2, "{:?}", merged.files);
        assert_eq!(merged.files[0].path, "src/a b.rs");
        assert_eq!(merged.files[0].status, "M");
        assert_eq!(merged.files[1].path, "new.rs");
        assert_eq!(merged.files[1].status, "R100");
        assert_eq!(merged.insertions, 3);
        assert_eq!(merged.deletions, 1);
    }

    /// A binary file has no line counts; it is still a file that changed.
    #[test]
    fn a_binary_file_counts_as_changed_with_no_lines() {
        let merged = merge_diff("-\t-\tlogo.png\0", "M\0logo.png\0");
        assert_eq!(merged.files.len(), 1);
        assert_eq!(merged.files[0].insertions, 0);
        assert_eq!(merged.files[0].deletions, 0);
    }

    #[tokio::test]
    async fn snapshot_branch_says_nothing_to_commit_on_a_clean_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo).await;
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        git(&repo, &["add", "."]).await.unwrap();
        git(&repo, &["commit", "-m", "a"]).await.unwrap();

        assert!(snapshot_branch(&repo, "task/x/1", "nope")
            .await
            .unwrap()
            .is_none());
        assert!(
            git(&repo, &["rev-parse", "--verify", "task/x/1"])
                .await
                .is_err(),
            "no branch is created when there was nothing to record"
        );
    }

    /// A linked worktree keeps its git dir elsewhere (`.git` is a file), so
    /// the scratch index has to follow `--git-common-dir` rather than being
    /// dropped next to the checkout — where it would show up as untracked in
    /// the very tree being staged.
    #[tokio::test]
    async fn snapshot_branch_works_from_a_linked_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo).await;
        let wt = add_worktree(&repo, "feature", None).await.unwrap();
        std::fs::write(wt.join("work.txt"), "done\n").unwrap();

        let hash = snapshot_branch(&wt, "task/feature/1", "captured")
            .await
            .unwrap()
            .expect("the worktree has a new file");
        assert!(!hash.is_empty());
        let shown = git(&wt, &["show", "task/feature/1:work.txt"])
            .await
            .unwrap();
        assert_eq!(shown, "done\n");
        assert_eq!(
            git(&wt, &["status", "--porcelain"]).await.unwrap().trim(),
            "?? work.txt",
            "the file is still untracked in the worktree itself"
        );
    }

    #[tokio::test]
    async fn remove_worktree_survives_manual_rm_rf() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo).await;
        let wt = add_worktree(&repo, "feature", None).await.unwrap();

        // Simulate the user deleting the checkout by hand.
        std::fs::remove_dir_all(&wt).unwrap();

        remove_worktree(&repo, &wt, false).await.unwrap();
        // The stale registration should be pruned from git's list too.
        let entries = list_worktrees(&repo).await.unwrap();
        assert!(entries.iter().all(|e| e.path != wt));
    }

    #[tokio::test]
    async fn remove_worktree_ok_when_already_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo).await;
        let wt = add_worktree(&repo, "feature", None).await.unwrap();
        std::fs::remove_dir_all(&wt).unwrap();
        git(&repo, &["worktree", "prune"]).await.unwrap();

        // Path gone AND git no longer knows it — still not an error.
        remove_worktree(&repo, &wt, false).await.unwrap();
    }

    #[tokio::test]
    async fn remove_worktree_unlocks_session_locked_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo).await;
        let wt = add_worktree(&repo, "feature", None).await.unwrap();
        let wt_str = wt.to_string_lossy().into_owned();
        git(
            &repo,
            &[
                "worktree",
                "lock",
                "--reason",
                "claude session menu-enable-level",
                &wt_str,
            ],
        )
        .await
        .unwrap();

        remove_worktree(&repo, &wt, false).await.unwrap();
        let entries = list_worktrees(&repo).await.unwrap();
        assert!(entries.iter().all(|e| e.path != wt));
    }

    #[tokio::test]
    async fn remove_worktree_still_fails_on_dirty_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        init_repo(&repo).await;
        let wt = add_worktree(&repo, "feature", None).await.unwrap();
        std::fs::write(wt.join("untracked.txt"), "dirty").unwrap();

        assert!(remove_worktree(&repo, &wt, false).await.is_err());
        remove_worktree(&repo, &wt, true).await.unwrap();
    }
}
