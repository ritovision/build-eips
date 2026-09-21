/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Shared tracked membership and source-currentness for content and theme.
//! HEAD/index/worktree play different roles; clean Git status is not a generation.
use crate::git;
use git2::{ObjectType, Oid};
use snafu::{ResultExt, Whatever};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::SystemTime,
};

pub(crate) struct Snapshot {
    pub(crate) stamp: (Oid, Oid),
    pub(crate) paths: BTreeSet<PathBuf>,
    pub(crate) dirty: BTreeSet<PathBuf>,
}
impl Snapshot {
    pub(crate) fn capture(root: &Path, active: bool) -> Result<Self, Whatever> {
        let stamp = source_stamp(root)?;
        let dirty: BTreeSet<_> = if active {
            git::working_tree_paths(root)
        } else {
            git::tracked_working_tree_paths(root)
        }
        .whatever_context("unable to read tracked source changes")?
        .into_iter()
        .collect();
        let mut paths = git::tracked_paths(root, |p| !active || !p.starts_with("build"))
            .whatever_context("unable to inventory tracked source")?;
        paths.extend(dirty.iter().cloned());
        if source_stamp(root)? != stamp {
            snafu::whatever!(
                "source HEAD/index changed during inventory; retry after source settles"
            );
        }
        Ok(Self {
            stamp,
            paths,
            dirty,
        })
    }
}

pub(crate) fn source_stamp(source: &Path) -> Result<(Oid, Oid), Whatever> {
    let repo =
        git::open_usable_repository(source).whatever_context("unable to open active source")?;
    let head = git::head_commit_id(&repo).whatever_context("unable to resolve active HEAD")?;
    let index = git::index_path(source).whatever_context("unable to resolve active index")?;
    let bytes = std::fs::read(&index).whatever_context("unable to read active index")?;
    Ok((
        head,
        Oid::hash_object(ObjectType::Blob, &bytes)
            .whatever_context("unable to fingerprint active index")?,
    ))
}

pub(crate) fn file_stamps(
    source: &Path,
    paths: &BTreeSet<PathBuf>,
) -> Result<Vec<Option<(u64, SystemTime)>>, Whatever> {
    paths
        .iter()
        .map(|path| match std::fs::symlink_metadata(source.join(path)) {
            Ok(meta) => Ok(Some((
                meta.len(),
                meta.modified()
                    .whatever_context("unable to timestamp source file")?,
            ))),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error).whatever_context("unable to inspect active source path"),
        })
        .collect()
}
