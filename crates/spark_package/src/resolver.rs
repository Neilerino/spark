//! Transitive dependency resolver.
//!
//! Resolves all dependencies (path and git) from a manifest, including
//! transitive dependencies declared in each dependency's own `Spark.toml`.
//! Detects cycles and handles diamond dependencies (same dep reached via
//! multiple paths).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::git::fetch_git_dep;
use crate::lock::{load_lock_file, LockFile, LockedPackage};
use crate::manifest::{load_manifest, Dependency, LinkLibrary, Manifest};
use crate::registry;
use crate::resolve::{resolve_path_dep, ResolvedSource};

/// The fully resolved set of dependencies.
#[derive(Debug)]
pub struct ResolvedDeps {
    /// All source files from resolved dependencies.
    pub sources: Vec<ResolvedSource>,
    /// All link libraries collected transitively.
    pub link_libraries: Vec<LinkLibrary>,
    /// All link search paths collected transitively.
    pub link_paths: Vec<String>,
    /// The lock file to write (contains git dep commit hashes).
    pub lock: LockFile,
}

/// Resolves all dependencies from a manifest, including transitive deps.
///
/// # Algorithm
///
/// 1. Loads the existing `Spark.lock` if present
/// 2. For each dependency in the manifest:
///    - **Path dep**: resolves source files, then checks for a sub-`Spark.toml`
///    - **Git dep**: checks lock freshness, fetches if needed, collects sources
///    - Recurses into each dependency's `Spark.toml` if it exists
/// 3. Detects cycles via a DFS "currently resolving" set
/// 4. Handles diamond deps by skipping already-resolved packages
///
/// # Errors
///
/// Returns an error string if a dependency cannot be resolved (missing path,
/// git fetch failure, cycle detected).
pub fn resolve_all_deps(manifest: &Manifest, project_root: &Path) -> Result<ResolvedDeps, String> {
    let existing_lock = load_lock_file(project_root).transpose()?;

    let mut ctx = ResolveContext {
        sources: Vec::new(),
        link_libraries: Vec::new(),
        link_paths: Vec::new(),
        locked_packages: Vec::new(),
        resolved: HashSet::new(),
        resolving: HashSet::new(),
        existing_lock,
        index_path: None,
    };

    resolve_deps_recursive(manifest, project_root, &mut ctx)?;

    Ok(ResolvedDeps {
        sources: ctx.sources,
        link_libraries: ctx.link_libraries,
        link_paths: ctx.link_paths,
        lock: LockFile {
            packages: ctx.locked_packages,
        },
    })
}

/// Internal state for the recursive resolution.
struct ResolveContext {
    sources: Vec<ResolvedSource>,
    link_libraries: Vec<LinkLibrary>,
    link_paths: Vec<String>,
    locked_packages: Vec<LockedPackage>,
    /// Names of packages that have been fully resolved (prevents re-processing).
    resolved: HashSet<String>,
    /// Names of packages currently being resolved (cycle detection).
    resolving: HashSet<String>,
    /// Previously loaded lock file for freshness checking.
    existing_lock: Option<LockFile>,
    /// Lazily synced registry index path (cached on first registry dep).
    index_path: Option<PathBuf>,
}

/// Recursively resolves dependencies from a manifest.
#[allow(clippy::too_many_lines)]
fn resolve_deps_recursive(
    manifest: &Manifest,
    project_root: &Path,
    ctx: &mut ResolveContext,
) -> Result<(), String> {
    for (dep_name, dep) in &manifest.dependencies {
        // Diamond dep: already resolved, skip.
        if ctx.resolved.contains(dep_name) {
            continue;
        }

        // Cycle detection.
        if ctx.resolving.contains(dep_name) {
            return Err(format!(
                "dependency cycle detected: `{dep_name}` depends on itself (transitively)"
            ));
        }

        ctx.resolving.insert(dep_name.clone());

        let dep_root = match dep {
            Dependency::Path { path } => {
                let dep_sources = resolve_path_dep(dep_name, path, project_root)?;
                ctx.sources.extend(dep_sources);

                // Compute the canonical absolute path for sub-Spark.toml
                // loading and transitive dependency resolution.
                let raw = if Path::new(path).is_absolute() {
                    PathBuf::from(path)
                } else {
                    project_root.join(path)
                };
                raw.canonicalize().unwrap_or(raw)
            }
            Dependency::Git {
                git,
                tag,
                branch,
                rev,
            } => {
                // Check if we have a fresh lock entry for this dep.
                let locked_commit = ctx.existing_lock.as_ref().and_then(|lock| {
                    lock.packages
                        .iter()
                        .find(|p| p.name == *dep_name && p.url == *git)
                        .map(|p| p.commit.clone())
                });

                // Use locked commit if available, otherwise fetch.
                let fetched = if let Some(commit) = &locked_commit {
                    // Try to use the locked commit. If the dir doesn't exist,
                    // re-fetch.
                    let short = &commit[..commit.len().min(12)];
                    let cached_dir = project_root
                        .join(".spark")
                        .join("packages")
                        .join(format!("{dep_name}-{short}"));

                    if cached_dir.join(".git").exists() {
                        crate::git::FetchedGitDep {
                            path: cached_dir,
                            commit: commit.clone(),
                        }
                    } else {
                        fetch_git_dep(
                            dep_name,
                            git,
                            tag.as_deref(),
                            branch.as_deref(),
                            rev.as_deref(),
                            project_root,
                        )?
                    }
                } else {
                    fetch_git_dep(
                        dep_name,
                        git,
                        tag.as_deref(),
                        branch.as_deref(),
                        rev.as_deref(),
                        project_root,
                    )?
                };

                // Collect source files from the fetched package.
                let dep_sources = resolve_path_dep(
                    dep_name,
                    &fetched.path.display().to_string(),
                    &PathBuf::new(),
                )?;
                ctx.sources.extend(dep_sources);

                // Record in lock file.
                ctx.locked_packages.push(LockedPackage {
                    name: dep_name.clone(),
                    source: "git".to_string(),
                    url: git.clone(),
                    commit: fetched.commit.clone(),
                    version: None,
                });

                fetched.path
            }
            Dependency::Registry(constraint) => {
                // Lazily sync the registry index.
                if ctx.index_path.is_none() {
                    ctx.index_path = Some(registry::sync_index(project_root)?);
                }
                let index_path = ctx.index_path.as_ref().expect("index_path just set");

                // Look up the package in the index.
                let metadata = registry::lookup_package(index_path, dep_name)?;

                // Check if we have a locked version for this dep.
                let locked_version = ctx.existing_lock.as_ref().and_then(|lock| {
                    lock.packages
                        .iter()
                        .find(|p| p.name == *dep_name && p.source == "registry")
                        .and_then(|p| p.version.clone())
                });

                // If locked and the cached directory exists, reuse it.
                let fetched = if let Some(ref ver_str) = locked_version {
                    let cached_dir = project_root
                        .join(".spark")
                        .join("packages")
                        .join(format!("{dep_name}-{ver_str}"));

                    if cached_dir.exists() {
                        // Find the matching version entry for lock recording.
                        let version: semver::Version = ver_str
                            .parse()
                            .map_err(|e| format!("invalid locked version for `{dep_name}`: {e}"))?;
                        registry::FetchedRegistryDep {
                            path: cached_dir,
                            version,
                        }
                    } else {
                        // Locked but cache gone — resolve and fetch again.
                        let entry = registry::resolve_version(&metadata, constraint)?;
                        registry::fetch_registry_dep(dep_name, entry, project_root)?
                    }
                } else {
                    // Not locked — resolve via MVS and fetch.
                    let entry = registry::resolve_version(&metadata, constraint)?;
                    registry::fetch_registry_dep(dep_name, entry, project_root)?
                };

                // Collect source files from the fetched package.
                let dep_sources = resolve_path_dep(
                    dep_name,
                    &fetched.path.display().to_string(),
                    &PathBuf::new(),
                )?;
                ctx.sources.extend(dep_sources);

                // Record in lock file.
                ctx.locked_packages.push(LockedPackage {
                    name: dep_name.clone(),
                    source: "registry".to_string(),
                    url: String::new(),
                    commit: String::new(),
                    version: Some(fetched.version.to_string()),
                });

                fetched.path
            }
        };

        // Collect build section from dependency's Spark.toml.
        if let Some(sub_manifest_result) = load_manifest(&dep_root) {
            let sub_manifest = sub_manifest_result?;

            // Collect link info from the dependency's build section.
            for spec in &sub_manifest.build.links {
                let lib = LinkLibrary {
                    name: spec.name().to_string(),
                    kind: spec.kind(),
                };
                if !ctx.link_libraries.iter().any(|l| l.name == lib.name) {
                    ctx.link_libraries.push(lib);
                }
            }
            for path in &sub_manifest.build.link_paths {
                if !ctx.link_paths.contains(path) {
                    ctx.link_paths.push(path.clone());
                }
            }

            // Recurse into the dependency's own dependencies.
            if !sub_manifest.dependencies.is_empty() {
                resolve_deps_recursive(&sub_manifest, &dep_root, ctx)?;
            }
        }

        ctx.resolving.remove(dep_name);
        ctx.resolved.insert(dep_name.clone());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(dir: &Path, name: &str, deps: &str, build: &str) {
        let content =
            format!("[project]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n{deps}\n{build}");
        std::fs::write(dir.join("Spark.toml"), content).unwrap();
    }

    fn write_spark_file(dir: &Path, module: &str) {
        let mod_dir = dir.join(module);
        std::fs::create_dir_all(&mod_dir).unwrap();
        std::fs::write(
            mod_dir.join(format!("{module}.spark")),
            format!("module {module}\npub fn helper(): Int {{ return 1 }}\n"),
        )
        .unwrap();
    }

    #[test]
    fn path_only_transitive() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let lib_b = tmp.path().join("lib_b");
        let lib_c = tmp.path().join("lib_c");

        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&lib_b).unwrap();
        std::fs::create_dir_all(&lib_c).unwrap();

        // root depends on lib_b, lib_b depends on lib_c.
        write_manifest(
            &root,
            "root",
            &format!(
                "[dependencies]\nlib_b = {{ path = \"{}\" }}",
                lib_b.display()
            ),
            "",
        );
        write_manifest(
            &lib_b,
            "lib_b",
            &format!(
                "[dependencies]\nlib_c = {{ path = \"{}\" }}",
                lib_c.display()
            ),
            "",
        );
        write_manifest(&lib_c, "lib_c", "", "");

        write_spark_file(&lib_b, "lib_b");
        write_spark_file(&lib_c, "lib_c");

        let manifest = crate::manifest::load_manifest(&root).unwrap().unwrap();
        let resolved = resolve_all_deps(&manifest, &root).unwrap();

        // Should have sources from both lib_b and lib_c.
        let names: Vec<String> = resolved
            .sources
            .iter()
            .map(|s| s.module_path.display().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n.contains("lib_b")),
            "expected lib_b source, got {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("lib_c")),
            "expected lib_c source, got {names:?}"
        );
    }

    #[test]
    fn cycle_detection() {
        let tmp = tempfile::tempdir().unwrap();
        let lib_a = tmp.path().join("lib_a");
        let lib_b = tmp.path().join("lib_b");

        std::fs::create_dir_all(&lib_a).unwrap();
        std::fs::create_dir_all(&lib_b).unwrap();

        // lib_a depends on lib_b, lib_b depends on lib_a.
        write_manifest(
            &lib_a,
            "lib_a",
            &format!(
                "[dependencies]\nlib_b = {{ path = \"{}\" }}",
                lib_b.display()
            ),
            "",
        );
        write_manifest(
            &lib_b,
            "lib_b",
            &format!(
                "[dependencies]\nlib_a = {{ path = \"{}\" }}",
                lib_a.display()
            ),
            "",
        );

        write_spark_file(&lib_a, "lib_a");
        write_spark_file(&lib_b, "lib_b");

        let manifest = crate::manifest::load_manifest(&lib_a).unwrap().unwrap();
        let result = resolve_all_deps(&manifest, &lib_a);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cycle"));
    }

    #[test]
    fn diamond_deps() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let lib_a = tmp.path().join("lib_a");
        let lib_b = tmp.path().join("lib_b");
        let shared = tmp.path().join("shared");

        for dir in [&root, &lib_a, &lib_b, &shared] {
            std::fs::create_dir_all(dir).unwrap();
        }

        // root depends on lib_a and lib_b.
        // Both lib_a and lib_b depend on shared.
        write_manifest(
            &root,
            "root",
            &format!(
                "[dependencies]\nlib_a = {{ path = \"{}\" }}\nlib_b = {{ path = \"{}\" }}",
                lib_a.display(),
                lib_b.display()
            ),
            "",
        );
        write_manifest(
            &lib_a,
            "lib_a",
            &format!(
                "[dependencies]\nshared = {{ path = \"{}\" }}",
                shared.display()
            ),
            "",
        );
        write_manifest(
            &lib_b,
            "lib_b",
            &format!(
                "[dependencies]\nshared = {{ path = \"{}\" }}",
                shared.display()
            ),
            "",
        );
        write_manifest(&shared, "shared", "", "");

        write_spark_file(&lib_a, "lib_a");
        write_spark_file(&lib_b, "lib_b");
        write_spark_file(&shared, "shared");

        let manifest = crate::manifest::load_manifest(&root).unwrap().unwrap();
        let resolved = resolve_all_deps(&manifest, &root).unwrap();

        // Shared should appear exactly once despite being reached via both lib_a and lib_b.
        let shared_count = resolved
            .sources
            .iter()
            .filter(|s| s.module_path.display().to_string().contains("shared"))
            .count();
        assert_eq!(shared_count, 1, "shared should be resolved exactly once");
    }

    #[test]
    fn transitive_link_info() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let lib = tmp.path().join("lib");

        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&lib).unwrap();

        write_manifest(
            &root,
            "root",
            &format!("[dependencies]\nmylib = {{ path = \"{}\" }}", lib.display()),
            "",
        );
        write_manifest(
            &lib,
            "mylib",
            "",
            "[build]\nlinks = [\"sqlite3\"]\nlink-paths = [\"/usr/local/lib\"]",
        );

        write_spark_file(&lib, "mylib");

        let manifest = crate::manifest::load_manifest(&root).unwrap().unwrap();
        let resolved = resolve_all_deps(&manifest, &root).unwrap();

        assert_eq!(resolved.link_libraries.len(), 1);
        assert_eq!(resolved.link_libraries[0].name, "sqlite3");
        assert_eq!(
            resolved.link_libraries[0].kind,
            crate::manifest::LinkKind::Dynamic
        );
        assert_eq!(resolved.link_paths, vec!["/usr/local/lib"]);
    }

    #[test]
    fn transitive_link_kind_propagation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let lib = tmp.path().join("lib");

        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&lib).unwrap();

        write_manifest(
            &root,
            "root",
            &format!("[dependencies]\nmylib = {{ path = \"{}\" }}", lib.display()),
            "",
        );
        write_manifest(
            &lib,
            "mylib",
            "",
            "[build]\nlinks = [{ name = \"z\", kind = \"static\" }, \"curl\"]",
        );

        write_spark_file(&lib, "mylib");

        let manifest = crate::manifest::load_manifest(&root).unwrap().unwrap();
        let resolved = resolve_all_deps(&manifest, &root).unwrap();

        assert_eq!(resolved.link_libraries.len(), 2);

        assert_eq!(resolved.link_libraries[0].name, "z");
        assert_eq!(
            resolved.link_libraries[0].kind,
            crate::manifest::LinkKind::Static
        );

        assert_eq!(resolved.link_libraries[1].name, "curl");
        assert_eq!(
            resolved.link_libraries[1].kind,
            crate::manifest::LinkKind::Dynamic
        );
    }

    #[test]
    fn no_deps_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        write_manifest(tmp.path(), "test", "", "");

        let manifest = crate::manifest::load_manifest(tmp.path()).unwrap().unwrap();
        let resolved = resolve_all_deps(&manifest, tmp.path()).unwrap();

        assert!(resolved.sources.is_empty());
        assert!(resolved.link_libraries.is_empty());
        assert!(resolved.link_paths.is_empty());
        assert!(resolved.lock.packages.is_empty());
    }
}
