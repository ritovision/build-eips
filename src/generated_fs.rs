/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Checks for paths we mutate in a disposable tree. Repository-controlled links
//! may be copied as leaves, but must never act as directories for our writes.
//! This is not a sandbox against another process replacing directories under the
//! same user's account between a check and an operation.

use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

fn invalid(path: &Path, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("unsafe generated path `{}`: {reason}", path.display()),
    )
}

pub(crate) fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Includes junctions and other reparse points, not just symbolic links.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// `path` must be under `root`; all existing ancestors must be real directories.
/// A leaf link can be allowed when the operation unlinks/replaces that leaf.
pub(crate) fn check_path(root: &Path, path: &Path, allow_leaf_link: bool) -> io::Result<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| invalid(path, "outside owned root"))?;
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(invalid(path, "non-relative path component"));
    }
    let mut current = root.to_path_buf();
    let parts: Vec<_> = relative
        .components()
        .filter(|c| *c != Component::CurDir)
        .collect();
    for index in 0..=parts.len() {
        let leaf = index == parts.len();
        match fs::symlink_metadata(&current) {
            Ok(meta) if is_link(&meta) => {
                if !(leaf && allow_leaf_link && current != root) {
                    return Err(invalid(
                        &current,
                        "symlink or reparse point at mutation boundary",
                    ));
                }
            }
            Ok(meta) if !leaf && !meta.is_dir() => {
                return Err(invalid(&current, "ancestor is not a directory"))
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if let Some(part) = parts.get(index) {
            current.push(part.as_os_str());
        }
    }
    Ok(())
}

/// Canonicalize existing prefixes as well as destinations that do not exist yet.
pub(crate) fn prospective_path(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let absolute = std::path::absolute(path)?;
            let parent = absolute
                .parent()
                .ok_or_else(|| invalid(path, "no parent"))?;
            let name = absolute
                .file_name()
                .ok_or_else(|| invalid(path, "no file name"))?;
            Ok(prospective_path(parent)?.join(name))
        }
        Err(e) => Err(e),
    }
}

pub(crate) fn check_disjoint(left: &Path, right: &Path) -> io::Result<()> {
    let a = prospective_path(left)?;
    let b = prospective_path(right)?;
    if a.starts_with(&b) || b.starts_with(&a) {
        return Err(invalid(
            left,
            &format!("overlaps source `{}`", right.display()),
        ));
    }
    Ok(())
}

/// Preserve the supported <active>/build layout, but never let generated state
/// contain a checkout, or overlap tracked files, content, or Git metadata.
pub(crate) fn check_source_destination(source: &Path, destination: &Path) -> io::Result<()> {
    let source = prospective_path(source)?;
    let destination = prospective_path(destination)?;
    if source.starts_with(&destination) {
        return Err(invalid(&destination, "contains the source checkout"));
    }
    for reserved in ["content", ".git", "Build.toml"] {
        check_disjoint(&destination, &source.join(reserved))?;
    }
    if let Ok(relative) = destination.strip_prefix(&source) {
        let repo = git2::Repository::open_ext(
            &source,
            git2::RepositoryOpenFlags::NO_SEARCH,
            &[] as &[&std::ffi::OsStr],
        )
        .map_err(io::Error::other)?;
        let index = repo.index().map_err(io::Error::other)?;
        for entry in index.iter() {
            let name = std::str::from_utf8(&entry.path).map_err(io::Error::other)?;
            let tracked = Path::new(name);
            if tracked.starts_with(relative) || relative.starts_with(tracked) {
                return Err(invalid(&destination, "overlaps a tracked source path"));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_build_root(
    resolved: &crate::execution::ResolvedExecution,
) -> io::Result<()> {
    let build = &resolved.build_path;
    check_source_destination(&resolved.root_path, build)?;
    if let Some(theme) = &resolved.theme_path {
        check_disjoint(build, theme)?;
    }
    for url in resolved.repository_use.other_repos.values() {
        if let Ok(sibling) = url.to_file_path() {
            check_disjoint(build, &sibling)?;
        }
    }
    for child in ["repo", "output", ".lock"] {
        check_path(build, &build.join(child), false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_escape_and_overlapping_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("generated");
        assert!(check_path(&root, &root.join("../victim"), false).is_err());
        assert!(check_disjoint(&root, temp.path()).is_err());
        assert!(check_disjoint(&root, &temp.path().join("source")).is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn ancestor_links_rejected_even_when_leaf_does_not_exist() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("generated");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("themes")).unwrap();
        assert!(check_path(&root, &root.join("themes/eips-theme"), true).is_err());
        assert!(check_path(&root, &root.join("themes"), true).is_ok());
        assert!(check_path(&root, &root.join("themes"), false).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn prospective_paths_detect_symlinked_source_alias() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&source, &alias).unwrap();
        assert!(check_disjoint(&alias.join("generated"), &source).is_err());
    }
}
