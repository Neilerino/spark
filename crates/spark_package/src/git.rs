//! Git dependency fetching.
//!
//! Clones git repositories into `.spark/packages/<name>-<commit[..12]>/` and
//! returns the local path and resolved commit hash. Fetches are idempotent —
//! if the target directory already exists, the clone is skipped.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Result of fetching a git dependency.
#[derive(Debug)]
pub struct FetchedGitDep {
    /// Local path to the cloned repository.
    pub path: PathBuf,
    /// Resolved full commit hash.
    pub commit: String,
}

/// Fetches a git dependency, cloning it into `.spark/packages/` under the project root.
///
/// If a `tag` or `branch` is specified, performs a shallow clone (`--depth 1`).
/// If a `rev` (exact commit) is specified, performs a full clone and checks out
/// the commit (shallow clones cannot target arbitrary SHAs).
/// If none are specified, clones the default branch at `--depth 1`.
///
/// # Errors
///
/// Returns an error string if git is not installed, the URL is invalid,
/// the specified ref does not exist, or any git operation fails.
pub fn fetch_git_dep(
    dep_name: &str,
    url: &str,
    tag: Option<&str>,
    branch: Option<&str>,
    rev: Option<&str>,
    project_root: &Path,
) -> Result<FetchedGitDep, String> {
    // Resolve the commit hash for the target ref.
    let commit = resolve_commit(dep_name, url, tag, branch, rev)?;
    let short_commit = &commit[..commit.len().min(12)];

    let packages_dir = project_root.join(".spark").join("packages");
    let target_dir = packages_dir.join(format!("{dep_name}-{short_commit}"));

    // Idempotent: if the target already exists with a .git dir, skip clone.
    if target_dir.join(".git").exists() {
        return Ok(FetchedGitDep {
            path: target_dir,
            commit,
        });
    }

    // Ensure the packages directory exists.
    std::fs::create_dir_all(&packages_dir).map_err(|e| {
        format!("could not create packages directory `{}`: {e}", packages_dir.display())
    })?;

    // Clone the repository.
    if let Some(exact_rev) = rev {
        // Full clone + checkout for exact commits.
        run_git(&[
            "clone",
            "--quiet",
            url,
            &target_dir.display().to_string(),
        ], dep_name)?;
        run_git_in(
            &["checkout", "--quiet", exact_rev],
            &target_dir,
            dep_name,
        )?;
    } else if let Some(ref_name) = tag.or(branch) {
        // Shallow clone with specific branch/tag.
        run_git(&[
            "clone",
            "--depth",
            "1",
            "--branch",
            ref_name,
            "--quiet",
            url,
            &target_dir.display().to_string(),
        ], dep_name)?;
    } else {
        // Shallow clone of default branch.
        run_git(&[
            "clone",
            "--depth",
            "1",
            "--quiet",
            url,
            &target_dir.display().to_string(),
        ], dep_name)?;
    }

    Ok(FetchedGitDep {
        path: target_dir,
        commit,
    })
}

/// Resolves a git ref to a full commit hash using `git ls-remote`.
///
/// For tags, looks for `refs/tags/<tag>`. For branches, looks for
/// `refs/heads/<branch>`. For exact revisions, returns the rev as-is.
/// For bare (no ref), resolves `HEAD`.
fn resolve_commit(
    dep_name: &str,
    url: &str,
    tag: Option<&str>,
    branch: Option<&str>,
    rev: Option<&str>,
) -> Result<String, String> {
    // Exact rev: use as-is (will be validated at checkout time).
    if let Some(r) = rev {
        return Ok(r.to_string());
    }

    let ref_pattern = tag.map_or_else(
        || {
            branch.map_or_else(
                || "HEAD".to_string(),
                |b| format!("refs/heads/{b}"),
            )
        },
        |t| format!("refs/tags/{t}"),
    );

    let output = run_git(&["ls-remote", url, &ref_pattern], dep_name)?;

    // Parse the first line: "<commit>\t<ref>"
    let line = output
        .lines()
        .next()
        .ok_or_else(|| format!("dependency `{dep_name}`: ref `{ref_pattern}` not found in {url}"))?;

    let commit = line
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("dependency `{dep_name}`: unexpected ls-remote output"))?;

    Ok(commit.to_string())
}

/// Runs a git command and returns its stdout on success.
fn run_git(args: &[&str], dep_name: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("dependency `{dep_name}`: could not run git: {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "dependency `{dep_name}`: git {} failed: {stderr}",
            args.first().unwrap_or(&"")
        ))
    }
}

/// Runs a git command in a specific directory.
fn run_git_in(args: &[&str], dir: &Path, dep_name: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("dependency `{dep_name}`: could not run git: {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "dependency `{dep_name}`: git {} failed: {stderr}",
            args.first().unwrap_or(&"")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Creates a temporary bare git repository with a single file for testing.
    /// Returns the path to the bare repo.
    fn create_test_repo(dir: &Path) -> PathBuf {
        let work_dir = dir.join("work");
        let bare_dir = dir.join("repo.git");

        // Create a working repo, add a file, commit, then create a bare clone.
        std::fs::create_dir_all(&work_dir).unwrap();

        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&work_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&work_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&work_dir)
            .output()
            .unwrap();

        // Create a Spark.toml and a .spark source file.
        std::fs::write(
            work_dir.join("Spark.toml"),
            "[project]\nname = \"testlib\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(work_dir.join("testlib")).unwrap();
        std::fs::write(
            work_dir.join("testlib").join("testlib.spark"),
            "module testlib\npub fn helper(): Int { return 42 }\n",
        )
        .unwrap();

        Command::new("git")
            .args(["add", "."])
            .current_dir(&work_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--quiet", "-m", "initial"])
            .current_dir(&work_dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["tag", "v1.0.0"])
            .current_dir(&work_dir)
            .output()
            .unwrap();

        // Create bare clone.
        Command::new("git")
            .args([
                "clone",
                "--bare",
                "--quiet",
                &work_dir.display().to_string(),
                &bare_dir.display().to_string(),
            ])
            .output()
            .unwrap();

        bare_dir
    }

    #[test]
    fn fetch_git_dep_with_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_repo = create_test_repo(tmp.path());
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let url = format!("file://{}", bare_repo.display());
        let result = fetch_git_dep("testlib", &url, Some("v1.0.0"), None, None, &project_root);
        let fetched = result.unwrap();

        assert!(!fetched.commit.is_empty());
        assert!(fetched.path.exists());
        assert!(fetched.path.join("Spark.toml").exists());
    }

    #[test]
    fn fetch_git_dep_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_repo = create_test_repo(tmp.path());
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let url = format!("file://{}", bare_repo.display());

        let first = fetch_git_dep("testlib", &url, Some("v1.0.0"), None, None, &project_root).unwrap();
        let second = fetch_git_dep("testlib", &url, Some("v1.0.0"), None, None, &project_root).unwrap();

        assert_eq!(first.commit, second.commit);
        assert_eq!(first.path, second.path);
    }

    #[test]
    fn fetch_git_dep_bad_url() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let result = fetch_git_dep(
            "badpkg",
            "file:///nonexistent/repo.git",
            None,
            None,
            None,
            &project_root,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("badpkg"));
    }

    #[test]
    fn fetch_git_dep_bad_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let bare_repo = create_test_repo(tmp.path());
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let url = format!("file://{}", bare_repo.display());
        let result = fetch_git_dep("testlib", &url, Some("v999.0.0"), None, None, &project_root);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }
}
