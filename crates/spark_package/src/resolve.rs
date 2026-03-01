//! Path dependency resolution.
//!
//! Given a dependency's path (relative to the project root), discovers all
//! `.spark` source files in that directory.

use std::path::{Path, PathBuf};

/// Discovers all `.spark` source files in a path dependency directory.
///
/// Returns `(relative_path, source_text)` pairs for each file found.
/// Paths are relative to the dependency root, prefixed with the package name
/// so that module resolution can distinguish them.
///
/// # Errors
///
/// Returns an error string if the dependency path does not exist or cannot be read.
pub fn resolve_path_dep(
    dep_name: &str,
    dep_path: &str,
    project_root: &Path,
) -> Result<Vec<(PathBuf, String)>, String> {
    let abs_dep_path = if Path::new(dep_path).is_absolute() {
        PathBuf::from(dep_path)
    } else {
        project_root.join(dep_path)
    };

    if !abs_dep_path.exists() {
        return Err(format!(
            "dependency `{dep_name}` path does not exist: {}",
            abs_dep_path.display()
        ));
    }

    if !abs_dep_path.is_dir() {
        return Err(format!(
            "dependency `{dep_name}` path is not a directory: {}",
            abs_dep_path.display()
        ));
    }

    let mut sources = Vec::new();
    collect_spark_files(&abs_dep_path, &abs_dep_path, dep_name, &mut sources)?;
    Ok(sources)
}

/// Recursively collects `.spark` files from a directory.
fn collect_spark_files(
    dir: &Path,
    dep_root: &Path,
    dep_name: &str,
    out: &mut Vec<(PathBuf, String)>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        format!("could not read directory `{}`: {e}", dir.display())
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| {
            format!("could not read directory entry: {e}")
        })?;
        let path = entry.path();

        if path.is_dir() {
            collect_spark_files(&path, dep_root, dep_name, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("spark") {
            let relative = path.strip_prefix(dep_root).unwrap_or(&path);
            // Prefix with the dependency name so module paths are namespaced.
            let prefixed = PathBuf::from(dep_name).join(relative);
            let source = std::fs::read_to_string(&path).map_err(|e| {
                format!("could not read `{}`: {e}", path.display())
            })?;
            out.push((prefixed, source));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_nonexistent_dep() {
        let result = resolve_path_dep("missing", "/nonexistent/path", Path::new("/tmp"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("does not exist"));
    }
}
