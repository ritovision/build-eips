/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Standalone export orchestration. A unique preparation/render root is never
//! shared with build, serve or preview; only the final export file is published.

use std::path::Path;

use snafu::{ResultExt, Whatever};

use crate::{
    config::SearchCorpusFormat, execution::ResolvedExecution, generated_fs, pipeline::Prepared,
    search::ExportDestination,
};

pub(crate) fn run(
    mut resolved: ResolvedExecution,
    output: &Path,
    format: SearchCorpusFormat,
    force: bool,
) -> Result<(), Whatever> {
    // Resolve against invocation CWD, not -C, and reject destinations before
    // doing any materialization or expensive rendering.
    let destination = ExportDestination::new(output, force)?;
    validate_destination(&resolved, destination.path())?;
    generated_fs::validate_build_root(&resolved)
        .whatever_context("unsafe private corpus build root")?;
    let base_url = resolved
        .base_url_override
        .as_ref()
        .unwrap_or(&resolved.repository_use.location.base_url);
    if !matches!(base_url.scheme(), "http" | "https")
        || base_url.host_str().is_none()
        || !base_url.username().is_empty()
        || base_url.password().is_some()
        || base_url.query().is_some()
        || base_url.fragment().is_some()
    {
        snafu::whatever!("corpus base URL must be an absolute HTTP(S) URL without credentials, query or fragment");
    }

    std::fs::create_dir_all(&resolved.build_path)
        .whatever_context("unable to create corpus scratch parent")?;
    let private = tempfile::Builder::new()
        .prefix(".corpus-")
        .tempdir_in(&resolved.build_path)
        .whatever_context("unable to create private corpus preparation")?;
    resolved.build_path = private.path().to_path_buf();
    // The export's own private directory needs no shared-build lock. The guard
    // lives across preparation, render, extraction and final publication and is
    // removed on both success and ordinary error unwinding.
    Prepared::prepare(resolved)?.corpus(&destination, format)
}

fn validate_destination(resolved: &ResolvedExecution, destination: &Path) -> Result<(), Whatever> {
    // --force is permission to replace an export, not to corrupt input or an
    // existing reusable/live build. Untracked exports at the active root work.
    generated_fs::check_disjoint(destination, &resolved.build_path)
        .whatever_context("corpus destination must be outside the build root")?;
    generated_fs::check_source_destination(&resolved.root_path, destination)
        .whatever_context("corpus destination overlaps active source")?;
    protect_git_metadata(&resolved.root_path, destination)?;
    if let Some(theme) = &resolved.theme_path {
        generated_fs::check_disjoint(destination, theme)
            .whatever_context("corpus destination overlaps theme source")?;
        protect_git_metadata(theme, destination)?;
    }
    for url in resolved.repository_use.other_repos.values() {
        if let Ok(sibling) = url.to_file_path() {
            generated_fs::check_disjoint(destination, &sibling)
                .whatever_context("corpus destination overlaps sibling source")?;
            protect_git_metadata(&sibling, destination)?;
        }
    }
    for (path, _) in &resolved.configuration.0 {
        generated_fs::check_disjoint(destination, path)
            .whatever_context("corpus destination overlaps workspace configuration")?;
    }
    Ok(())
}

fn protect_git_metadata(source: &Path, destination: &Path) -> Result<(), Whatever> {
    // A linked worktree's administrative and common object/ref directories can
    // be outside the checkout. Protect those actual paths, not only `.git`.
    // Declared file:// remotes can be bare even though active/workspace
    // checkouts cannot. This guard identifies metadata; it must not impose the
    // local-worktree usability policy on the existing remote-fetch contract.
    let repo = git2::Repository::open_ext(
        source,
        git2::RepositoryOpenFlags::NO_SEARCH,
        &[] as &[&std::ffi::OsStr],
    )
    .whatever_context("unable to identify protected source Git metadata")?;
    for metadata in [repo.path(), repo.commondir()] {
        generated_fs::check_disjoint(destination, metadata)
            .whatever_context("corpus destination overlaps source Git metadata")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bare_file_remote_metadata_is_protected_without_requiring_a_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        let repo = git2::Repository::init_bare(&remote).unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let author = git2::Signature::now("Test", "test@example.test").unwrap();
        repo.commit(Some("HEAD"), &author, &author, "initial", &tree, &[])
            .unwrap();
        assert!(repo.is_bare());
        assert!(protect_git_metadata(&remote, &temp.path().join("export.json")).is_ok());
        assert!(protect_git_metadata(&remote, &remote.join("HEAD")).is_err());
    }

    #[test]
    fn linked_worktree_actual_git_metadata_is_protected() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let linked = temp.path().join("linked");
        let repo = git2::Repository::init(&original).unwrap();
        std::fs::write(original.join("file"), "source").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("file")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let author = git2::Signature::now("Test", "test@example.test").unwrap();
        repo.commit(Some("HEAD"), &author, &author, "initial", &tree, &[])
            .unwrap();
        repo.worktree("linked", &linked, None).unwrap();
        let worktree = crate::git::open_usable_repository(&linked).unwrap();
        assert!(protect_git_metadata(&linked, &worktree.path().join("HEAD")).is_err());
        assert!(protect_git_metadata(&linked, &worktree.commondir().join("config")).is_err());
        assert!(protect_git_metadata(&linked, &temp.path().join("export.json")).is_ok());
    }
}
