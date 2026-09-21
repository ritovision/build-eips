/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Ownership of generated Git repositories, not of developer source worktrees.
//! Check the handle's effective paths before any fetch, checkout or index write.
//! An ordinary .git/config can redirect core.worktree without a gitdir pointer.
use std::{fs, io, path::Path};

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub(crate) fn check_metadata_path(root: &Path) -> io::Result<()> {
    let metadata = root.join(".git");
    crate::generated_fs::check_path(root, &metadata, false)?;
    match fs::symlink_metadata(&metadata) {
        Ok(meta) if !meta.is_dir() => {
            return Err(invalid(
                "generated repository must own a real .git directory, not a gitdir pointer",
            ))
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    // Refs/logs/objects/config must not use link ancestors or leaf links to
    // redirect writes. Git-created object hardlinks are not mutated in place.
    for entry in walkdir::WalkDir::new(&metadata).follow_links(false) {
        let entry = entry.map_err(io::Error::other)?;
        let meta = fs::symlink_metadata(entry.path())?;
        if crate::generated_fs::is_link(&meta) {
            return Err(invalid(format!(
                "generated Git metadata contains a link: {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

pub(crate) fn validate(repo: &git2::Repository, expected: &Path) -> io::Result<()> {
    check_metadata_path(expected)?;
    let root = expected.canonicalize()?;
    let metadata = root.join(".git").canonicalize()?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| invalid("generated repository has no working directory"))?;
    if repo.is_bare() || workdir.canonicalize()? != root {
        return Err(invalid(format!(
            "generated Git effective workdir `{}` differs from owned root `{}`",
            workdir.display(),
            root.display()
        )));
    }
    if repo.path().canonicalize()? != metadata || repo.commondir().canonicalize()? != metadata {
        return Err(invalid(
            "generated Git metadata/common directory is outside its owned .git directory",
        ));
    }
    let index = repo.index().map_err(io::Error::other)?;
    let index_path = index
        .path()
        .ok_or_else(|| invalid("generated repository has an in-memory index"))?;
    if crate::generated_fs::prospective_path(index_path)? != metadata.join("index") {
        return Err(invalid(
            "generated Git index is outside its owned metadata directory",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normal_generated_repository_is_owned_but_redirected_workdir_is_not() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), "KEEP").unwrap();
        let repo = git2::Repository::init(&root).unwrap();
        validate(&repo, &root).unwrap();
        repo.config()
            .unwrap()
            .set_str("core.worktree", outside.to_str().unwrap())
            .unwrap();
        drop(repo);
        let repo = git2::Repository::open(&root).unwrap();
        assert!(validate(&repo, &root).is_err());
        assert_eq!(
            fs::read_to_string(outside.join("sentinel")).unwrap(),
            "KEEP"
        );
    }
    #[cfg(unix)]
    #[test]
    fn generated_metadata_links_are_rejected_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let repo = git2::Repository::init(&root).unwrap();
        let external = temp.path().join("external-refs");
        fs::create_dir(&external).unwrap();
        fs::remove_dir_all(root.join(".git/refs")).unwrap();
        std::os::unix::fs::symlink(&external, root.join(".git/refs")).unwrap();
        assert!(validate(&repo, &root).is_err());
        assert_eq!(fs::read_dir(external).unwrap().count(), 0);
    }
}
