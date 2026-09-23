/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::{
    collections::BTreeSet,
    ffi::OsStr,
    io::ErrorKind,
    path::{absolute, Path, PathBuf},
};

use crate::config::RepositoryUse;
use crate::{
    layout::{BUILD_DIR, CONTENT_DIR},
    progress::{Git, ProgressIteratorExt},
};
use git2::{
    build::{CheckoutBuilder, TreeUpdateBuilder},
    Commit, FetchOptions, FileMode, ObjectType, Oid, RepositoryOpenFlags, Signature, Status,
    StatusOptions, Tree, TreeEntry, TreeWalkResult,
};
use log::{debug, info};
use snafu::{ensure, Backtrace, IntoError, OptionExt, ResultExt, Snafu};
use unicode_normalization::UnicodeNormalization;
use url::Url;

const DIRTY_PATH_DISPLAY_LIMIT: usize = 10;
const CONTENT_INDEX_PATH: &str = "content/_index.md";

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("cannot convert path into URL (`{}`)", path.to_string_lossy()))]
    PathUrl { path: PathBuf, backtrace: Backtrace },
    #[snafu(display("i/o error accessing `{}`", path.to_string_lossy()))]
    Io {
        path: PathBuf,
        source: std::io::Error,
        backtrace: Backtrace,
    },
    #[snafu(display("unable to {what}"))]
    Git {
        what: &'static str,
        source: git2::Error,
        backtrace: Backtrace,
    },
    #[snafu(display(
        "Windows Git source path length limit exceeded; move the source checkout to a shorter directory. For generated output paths, select a shorter --build-root. No global Git or Windows registry change is required"
    ))]
    NativeSourcePathTooLong {
        source: git2::Error,
        backtrace: Backtrace,
    },
    #[snafu(display("{message}"))]
    Dirty {
        message: String,
        backtrace: Backtrace,
    },
    #[snafu(display(
        "dirty mode cannot materialize conflicted path `{}`; resolve the conflict and try again",
        path.to_string_lossy()
    ))]
    DirtyConflict { path: PathBuf, backtrace: Backtrace },
    #[snafu(display(
        "dirty mode cannot materialize `{}` because it is not a tracked file or symlink in the working tree",
        path.to_string_lossy()
    ))]
    DirtyUnsupportedPath { path: PathBuf, backtrace: Backtrace },
    #[snafu(display("unable to update tree ({msg})"))]
    UpdateTree { msg: String, backtrace: Backtrace },
    #[snafu(display(
        "workspace path `{}` already exists but is not a usable git repository",
        path.to_string_lossy()
    ))]
    ExistingWorkspacePath { path: PathBuf, backtrace: Backtrace },
    #[snafu(display(
        "workspace repository `{}` is unusable: {reason}",
        path.to_string_lossy()
    ))]
    UnusableWorkspaceRepository {
        path: PathBuf,
        reason: &'static str,
        backtrace: Backtrace,
    },
    #[snafu(display(
        "fresh workspace repository `{}` was missing before checkout",
        path.to_string_lossy()
    ))]
    MissingFreshWorkspaceRepository { path: PathBuf, backtrace: Backtrace },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceMaterialization {
    Clean,
    Dirty,
}

/// Open a non-bare repository with a resolvable HEAD without modifying it.
pub fn open_usable_repository(path: &Path) -> Result<git2::Repository, Error> {
    let repository =
        git2::Repository::open_ext(path, RepositoryOpenFlags::NO_SEARCH, &[] as &[&OsStr])
            .context(GitSnafu {
                what: "open workspace repository",
            })?;
    ensure_usable_workspace_repository(&repository, path)?;
    Ok(repository)
}

/// Validate every present manifest used by the local sibling contract. Missing
/// manifests remain a supported transitional state; contradictory ones do not.
/// The committed snapshot is checked again after fetch, before it is merged.
pub(crate) fn validate_sibling_identity(
    path: &Path,
    expected: &str,
) -> Result<bool, snafu::Whatever> {
    let repo = open_usable_repository(path).whatever_context("unable to open sibling checkout")?;
    let actual_root = repo
        .workdir()
        .whatever_context("sibling has no worktree")?
        .canonicalize()
        .whatever_context("unable to resolve sibling worktree")?;
    if actual_root
        != path
            .canonicalize()
            .whatever_context("unable to resolve sibling path")?
    {
        snafu::whatever!("sibling `{expected}` is not rooted at `{}`", path.display());
    }
    let manifest_path = path.join(crate::config::MANIFEST_FILE);
    let working_present = match crate::config::Manifest::load(&manifest_path) {
        Ok(manifest) => {
            validate_sibling_name(&manifest, expected, "working tree")?;
            true
        }
        Err(crate::config::Error::Io { source, .. })
            if matches!(
                source.kind(),
                ErrorKind::NotFound | ErrorKind::NotADirectory
            ) =>
        {
            false
        }
        Err(error) => {
            return Err(error).whatever_context("unable to load sibling working-tree Build.toml")
        }
    };
    let head = repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .whatever_context("unable to read sibling HEAD")?;
    Ok(validate_sibling_commit(&repo, &head, expected)? && working_present)
}

fn validate_sibling_name(
    manifest: &crate::config::Manifest,
    expected: &str,
    snapshot: &str,
) -> Result<(), snafu::Whatever> {
    if manifest.name != *expected {
        snafu::whatever!(
            "sibling `{expected}` {snapshot} Build.toml declares name `{}`",
            manifest.name
        );
    }
    Ok(())
}

fn validate_sibling_commit(
    repo: &git2::Repository,
    commit: &Commit<'_>,
    expected: &str,
) -> Result<bool, snafu::Whatever> {
    let tree = commit
        .tree()
        .whatever_context("unable to read sibling commit tree")?;
    let entry = match tree.get_path(Path::new(crate::config::MANIFEST_FILE)) {
        Ok(entry) => entry,
        Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).whatever_context("unable to read committed sibling Build.toml")
        }
    };
    if !matches!(entry.filemode(), 0o100644 | 0o100755) {
        snafu::whatever!("sibling `{expected}` committed Build.toml must be a regular file");
    }
    let blob = repo
        .find_blob(entry.id())
        .whatever_context("unable to read sibling manifest blob")?;
    let text =
        std::str::from_utf8(blob.content()).whatever_context("sibling manifest is not UTF-8")?;
    let manifest = crate::config::Manifest::from_contents(
        PathBuf::from(format!("{expected}@{}:Build.toml", commit.id())),
        text,
    )
    .whatever_context("unable to parse committed sibling Build.toml")?;
    validate_sibling_name(&manifest, expected, "committed")?;
    Ok(true)
}

/// Return the commit currently checked out by a repository.
pub fn head_commit_id(repository: &git2::Repository) -> Result<Oid, Error> {
    let head = repository.head().context(GitSnafu {
        what: "read repository HEAD",
    })?;
    let commit = head.peel_to_commit().context(GitSnafu {
        what: "resolve repository HEAD commit",
    })?;
    Ok(commit.id())
}

/// Resolve a local commit-ish to its commit object ID without fetching.
pub fn resolve_commit_id(repository: &git2::Repository, commit: &str) -> Result<Oid, Error> {
    let object = repository.revparse_single(commit).context(GitSnafu {
        what: "resolve local commit-ish",
    })?;
    let commit = object.peel_to_commit().context(GitSnafu {
        what: "resolve local commit object",
    })?;
    Ok(commit.id())
}

/// Return configured remote URLs without fetching or otherwise modifying a repository.
pub fn remote_urls(repository: &git2::Repository) -> Result<Vec<String>, Error> {
    let names = repository.remotes().context(GitSnafu {
        what: "list repository remotes",
    })?;

    names
        .iter()
        .flatten()
        .map(|name| {
            let remote = repository.find_remote(name).context(GitSnafu {
                what: "read repository remote",
            })?;
            Ok(remote.url().map(ToOwned::to_owned))
        })
        .collect::<Result<Vec<_>, Error>>()
        .map(|urls| urls.into_iter().flatten().collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneOutcome {
    Fresh,
    Existing,
}

fn ensure_usable_workspace_repository(
    repository: &git2::Repository,
    path: &Path,
) -> Result<(), Error> {
    if repository.workdir().is_none() {
        return UnusableWorkspaceRepositorySnafu {
            path: path.to_path_buf(),
            reason: "it is bare",
        }
        .fail();
    }

    if repository.head().is_err() {
        return UnusableWorkspaceRepositorySnafu {
            path: path.to_path_buf(),
            reason: "it has no HEAD commit",
        }
        .fail();
    }

    Ok(())
}

fn open_existing_workspace_repository(
    destination: &Path,
) -> Result<Option<git2::Repository>, Error> {
    match git2::Repository::open_ext(
        destination,
        RepositoryOpenFlags::NO_SEARCH,
        &[] as &[&OsStr],
    ) {
        Ok(repository) => {
            ensure_usable_workspace_repository(&repository, destination)?;
            Ok(Some(repository))
        }
        Err(error) if error.code() == git2::ErrorCode::NotFound => {
            match std::fs::symlink_metadata(destination) {
                Ok(_) => ExistingWorkspacePathSnafu {
                    path: destination.to_path_buf(),
                }
                .fail(),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
                Err(error) => Err(IoSnafu {
                    path: destination.to_path_buf(),
                }
                .into_error(error)),
            }
        }
        Err(error) => match std::fs::symlink_metadata(destination) {
            Ok(_) => ExistingWorkspacePathSnafu {
                path: destination.to_path_buf(),
            }
            .fail(),
            Err(metadata_error) if metadata_error.kind() == ErrorKind::NotFound => Err(GitSnafu {
                what: "open existing workspace repository",
            }
            .into_error(error)),
            Err(metadata_error) => Err(IoSnafu {
                path: destination.to_path_buf(),
            }
            .into_error(metadata_error)),
        },
    }
}

pub fn clone_missing_repo(url: &str, destination: &Path) -> Result<CloneOutcome, Error> {
    clone_missing_repo_with_pin(url, destination, None)
}

/// Publish a new checkout only after both cloning and optional initial pinning
/// succeed. Existing usable developer checkouts are never reset.
pub fn clone_missing_repo_at_commit(
    url: &str,
    destination: &Path,
    commit: &str,
) -> Result<CloneOutcome, Error> {
    clone_missing_repo_with_pin(url, destination, Some(commit))
}

fn clone_missing_repo_with_pin(
    url: &str,
    destination: &Path,
    pin: Option<&str>,
) -> Result<CloneOutcome, Error> {
    let parent = destination.parent().ok_or_else(|| {
        IoSnafu {
            path: destination.to_path_buf(),
        }
        .into_error(std::io::Error::new(
            ErrorKind::InvalidInput,
            "workspace checkout has no parent",
        ))
    })?;
    std::fs::create_dir_all(parent).with_context(|_| IoSnafu {
        path: parent.to_path_buf(),
    })?;
    // Serialize cooperating initializers. The lock survives a killed initializer;
    // the operating-system lock does not, so retry does not require stale-lock removal.
    let lock_path = parent.join(".build-eips-init.lock");
    let mut lock = fslock::LockFile::open(&lock_path).with_context(|_| IoSnafu {
        path: lock_path.clone(),
    })?;
    lock.lock().with_context(|_| IoSnafu {
        path: lock_path.clone(),
    })?;
    if open_existing_workspace_repository(destination)?.is_some() {
        info!("using existing workspace repo `{}`", destination.display());
        return Ok(CloneOutcome::Existing);
    }

    let staging = tempfile::Builder::new()
        .prefix(".build-eips-init-")
        .tempdir_in(parent)
        .with_context(|_| IoSnafu {
            path: parent.to_path_buf(),
        })?;
    let staged_repo = staging.path().join("checkout");
    info!("cloning `{url}` for `{}`", destination.display());
    {
        let repository = git2::Repository::clone(url, &staged_repo).context(GitSnafu {
            what: "clone staged workspace repository",
        })?;
        ensure_usable_workspace_repository(&repository, &staged_repo)?;
        if pin.is_none() {
            let head = repository
                .head()
                .and_then(|head| head.peel_to_commit())
                .context(GitSnafu {
                    what: "read staged workspace repository HEAD",
                })?;
            let tree = head.tree().context(GitSnafu {
                what: "read staged workspace repository tree",
            })?;
            validate_portable_tree(&tree)?;
        }
    }
    if let Some(commit) = pin {
        checkout_fresh_clone_at_commit(&staged_repo, url, commit).map_err(|error| {
            UpdateTreeSnafu {
                msg: format!(
                    "unable to fetch or check out active Build.toml theme commit `{commit}`: {}",
                    snafu::Report::from_error(error)
                ),
            }
            .build()
        })?;
    }
    // Never replace another checkout that appeared while external tools ran.
    if std::fs::symlink_metadata(destination).is_ok() {
        return ExistingWorkspacePathSnafu {
            path: destination.to_path_buf(),
        }
        .fail();
    }
    std::fs::rename(&staged_repo, destination).with_context(|_| IoSnafu {
        path: destination.to_path_buf(),
    })?;
    Ok(CloneOutcome::Fresh)
}

pub fn checkout_fresh_clone_at_commit(
    destination: &Path,
    repository_url: &str,
    commit: &str,
) -> Result<(), Error> {
    let Some(repository) = open_existing_workspace_repository(destination)? else {
        return MissingFreshWorkspaceRepositorySnafu {
            path: destination.to_path_buf(),
        }
        .fail();
    };
    let commit = match repository
        .revparse_single(commit)
        .and_then(|object| object.peel_to_commit())
    {
        Ok(commit) => commit,
        Err(error)
            if matches!(
                error.code(),
                git2::ErrorCode::NotFound | git2::ErrorCode::InvalidSpec
            ) =>
        {
            let refspec = format!("+{commit}:refs/build-eips/theme-pin");
            fetch(&repository, repository_url, &refspec)?
        }
        Err(error) => {
            return Err(GitSnafu {
                what: "resolve manifest theme commit",
            }
            .into_error(error));
        }
    };

    let tree = commit.tree().context(GitSnafu {
        what: "read manifest theme tree",
    })?;
    validate_portable_tree(&tree)?;

    repository
        .set_head_detached(commit.id())
        .context(GitSnafu {
            what: "detach manifest theme commit",
        })?;
    repository
        .checkout_head(Some(CheckoutBuilder::default().force()))
        .context(GitSnafu {
            what: "checkout manifest theme commit",
        })?;

    Ok(())
}

fn is_generated_path(path: &Path) -> bool {
    path.components()
        .next()
        .map(|component| component.as_os_str() == OsStr::new(BUILD_DIR))
        .unwrap_or(false)
}

/// Stable repository-wide namespace used to reject tracked leaves that cannot
/// be represented uniquely on common case-insensitive filesystems. Normalize
/// canonical Unicode equivalents and case, then reject equal normalized leaf
/// paths and file-vs-directory aliases. Case-only directory spellings are
/// allowed when their descendant leaves remain distinct.
fn portable_component_key(component: &OsStr) -> Result<String, Error> {
    let text = component.to_str().ok_or_else(|| {
        UpdateTreeSnafu {
            msg: String::from("tracked paths must be UTF-8 for cross-platform portability"),
        }
        .build()
    })?;
    let lowered: String = text.nfc().flat_map(|c| c.to_lowercase()).collect();
    Ok(lowered.nfc().collect())
}

pub(crate) fn validate_portable_paths<'a>(
    paths: impl IntoIterator<Item = &'a PathBuf>,
) -> Result<(), Error> {
    let mut keyed = Vec::new();
    for path in paths {
        let mut key = Vec::new();
        for component in path.components() {
            let std::path::Component::Normal(name) = component else {
                return UpdateTreeSnafu {
                    msg: format!(
                        "tracked path `{}` contains a non-portable component",
                        path.display()
                    ),
                }
                .fail();
            };
            key.push(portable_component_key(name)?);
        }
        keyed.push((key, path.clone()));
    }
    keyed.sort_by(|left, right| left.0.cmp(&right.0));

    for pair in keyed.windows(2) {
        let (left_key, left_path) = &pair[0];
        let (right_key, right_path) = &pair[1];
        if right_key == left_key || right_key.starts_with(left_key) {
            return UpdateTreeSnafu {
                msg: format!(
                    "tracked paths `{}` and `{}` collide on a case-insensitive filesystem",
                    left_path.display(),
                    right_path.display()
                ),
            }
            .fail();
        }
    }
    Ok(())
}

pub(crate) fn validate_portable_tree(tree: &Tree<'_>) -> Result<(), Error> {
    let mut paths = BTreeSet::new();
    let mut walk_error = None;
    let walk_result = tree.walk(git2::TreeWalkMode::PreOrder, |prefix, entry| {
        let Some(name) = entry.name() else {
            walk_error = Some(
                UpdateTreeSnafu {
                    msg: format!("non-UTF-8 tracked path below `{prefix}`"),
                }
                .build(),
            );
            return TreeWalkResult::Abort;
        };
        if entry.kind() != Some(ObjectType::Tree) {
            paths.insert(PathBuf::from(format!("{prefix}{name}")));
        }
        TreeWalkResult::Ok
    });
    if let Some(error) = walk_error {
        return Err(error);
    }
    walk_result.context(GitSnafu {
        what: "inventory tree for cross-platform path collisions",
    })?;
    validate_portable_paths(paths.iter())
}

/// Effective tracked working-tree membership after applying the same dirty
/// overlay semantics used by materialization. Start from HEAD, remove every
/// changed path (and descendants replaced by that path), then add back only
/// changed leaves that physically exist as regular files or symlinks. This is
/// intentionally not equivalent to the index alone: an unstaged rename can be
/// materialized by dirty/live mode even though its destination is not in the
/// index yet.
fn effective_tracked_worktree_paths(root: &Path) -> Result<BTreeSet<PathBuf>, Error> {
    let mut paths = tracked_paths(root, |_| true)?;
    let (dirty, _) = collect_dirty_paths(root, |_| true)?;

    for changed in &dirty {
        paths.retain(|path| path != changed && !path.starts_with(changed));
    }
    for changed in &dirty {
        match crate::tree_sync::inspect(root, changed).with_context(|_| IoSnafu {
            path: root.join(changed),
        })? {
            Some(meta) if meta.is_file() || crate::generated_fs::is_link(&meta) => {
                paths.insert(changed.clone());
            }
            Some(meta) if meta.is_dir() => {}
            Some(_) => {
                return DirtyUnsupportedPathSnafu {
                    path: changed.clone(),
                }
                .fail()
            }
            None => {}
        }
    }
    #[cfg(windows)]
    discard_replaced_case_aliases(root, &mut paths)?;
    Ok(paths)
}

#[cfg(windows)]
fn discard_replaced_case_aliases(root: &Path, paths: &mut BTreeSet<PathBuf>) -> Result<(), Error> {
    let repo = git2::Repository::open(root).context(GitSnafu {
        what: "inspect case-only source rename",
    })?;
    let index = repo.index().context(GitSnafu {
        what: "read case-only rename index",
    })?;
    let indexed: BTreeSet<PathBuf> = index
        .iter()
        .map(|entry| {
            std::str::from_utf8(&entry.path)
                .map(PathBuf::from)
                .map_err(|_| {
                    UpdateTreeSnafu {
                        msg: "tracked paths must be UTF-8".to_owned(),
                    }
                    .build()
                })
        })
        .collect::<Result<_, _>>()?;
    let key = |path: &Path| {
        path.components()
            .map(|c| portable_component_key(c.as_os_str()))
            .collect::<Result<Vec<_>, _>>()
    };
    let mut replacements = std::collections::BTreeMap::new();
    for path in paths.iter().filter(|path| indexed.contains(*path)) {
        let full = root.join(path);
        let canonical = full
            .canonicalize()
            .with_context(|_| IoSnafu { path: &full })?;
        replacements
            .entry(key(path)?)
            .or_insert_with(Vec::new)
            .push(canonical);
    }
    let mut obsolete = Vec::new();
    for path in paths.iter().filter(|path| !indexed.contains(*path)) {
        if let Some(targets) = replacements.get(&key(path)?) {
            let full = root.join(path);
            let canonical = full
                .canonicalize()
                .with_context(|_| IoSnafu { path: &full })?;
            if targets.contains(&canonical) {
                obsolete.push(path.clone());
            }
        }
    }
    for path in obsolete {
        paths.remove(&path);
    }
    Ok(())
}

/// Validate the exact tracked membership that dirty/live materialization would
/// publish, rather than merely HEAD or the index.
pub(crate) fn validate_portable_worktree(root: &Path) -> Result<(), Error> {
    let paths = effective_tracked_worktree_paths(root)?;
    validate_portable_paths(paths.iter())
}

fn dirty_statuses(repo: &git2::Repository) -> Result<git2::Statuses<'_>, Error> {
    let mut options = StatusOptions::default();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .renames_head_to_index(true)
        .renames_index_to_workdir(true);
    repo.statuses(Some(&mut options))
        .map_err(source_status_error)
}

fn source_status_error(source: git2::Error) -> Error {
    if cfg!(windows) && source.message().contains("path too long") {
        NativeSourcePathTooLongSnafu.into_error(source)
    } else {
        GitSnafu {
            what: "get root repository status",
        }
        .into_error(source)
    }
}

fn format_dirty_rejection(tracked_paths: &BTreeSet<PathBuf>, untracked_count: usize) -> String {
    let mut lines = vec![String::from(
        "working tree or index has uncommitted modifications; the selected clean source path requires a clean working tree:",
    )];

    for path in tracked_paths.iter().take(DIRTY_PATH_DISPLAY_LIMIT) {
        lines.push(format!("- {}", path.to_string_lossy()));
    }

    if tracked_paths.len() > DIRTY_PATH_DISPLAY_LIMIT {
        lines.push(format!(
            "- ... and {} more tracked path(s)",
            tracked_paths.len() - DIRTY_PATH_DISPLAY_LIMIT
        ));
    }

    if untracked_count > 0 {
        lines.push(format!(
            "- ... plus {} untracked file(s) not listed",
            untracked_count
        ));
    }

    lines.push(String::new());

    if untracked_count > 0 {
        lines.push(String::from(
            "For local build/serve/check commands, run without `--clean` to include tracked local changes. For `--clean` runs, commit or stash tracked changes first. Commit/stash/remove untracked files before retrying.",
        ));
    } else {
        lines.push(String::from(
            "For local build/serve/check commands, run without `--clean` to include tracked local changes. For `--clean` runs, commit or stash tracked changes first.",
        ));
    }

    lines.join("\n")
}

pub fn check_dirty(root_path: &Path) -> Result<(), Error> {
    let (tracked_paths, untracked_count) =
        collect_dirty_paths(root_path, |path| !is_generated_path(path))?;

    if tracked_paths.is_empty() && untracked_count == 0 {
        Ok(())
    } else {
        DirtySnafu {
            message: format_dirty_rejection(&tracked_paths, untracked_count),
        }
        .fail()
    }
}

fn entry_path(entry: &git2::StatusEntry<'_>) -> Option<PathBuf> {
    entry
        .head_to_index()
        .and_then(|delta| delta.new_file().path().or_else(|| delta.old_file().path()))
        .or_else(|| {
            entry
                .index_to_workdir()
                .and_then(|delta| delta.new_file().path().or_else(|| delta.old_file().path()))
        })
        .or_else(|| entry.path().map(Path::new))
        .map(Path::to_path_buf)
}

fn collect_dirty_paths(
    root_path: &Path,
    include_path: impl Fn(&Path) -> bool,
) -> Result<(BTreeSet<PathBuf>, usize), Error> {
    let repo = git2::Repository::open(root_path).context(GitSnafu {
        what: "open root repository",
    })?;
    let statuses = dirty_statuses(&repo)?;
    let mut paths = BTreeSet::new();
    let mut untracked_count = 0;

    for entry in statuses.iter() {
        let status = entry.status();
        let path = entry_path(&entry).unwrap_or_else(|| PathBuf::from("<unknown>"));

        if status.contains(Status::CONFLICTED) {
            return DirtyConflictSnafu { path }.fail();
        }

        if status == Status::CURRENT || status == Status::IGNORED {
            continue;
        }

        if status == Status::WT_NEW {
            if include_path(&path) {
                untracked_count += 1;
            }
            continue;
        }

        if let Some(delta) = entry.head_to_index() {
            if let Some(old_path) = delta.old_file().path().filter(|path| include_path(path)) {
                paths.insert(old_path.to_path_buf());
            }
            if let Some(new_path) = delta.new_file().path().filter(|path| include_path(path)) {
                paths.insert(new_path.to_path_buf());
            }
        }

        if let Some(delta) = entry.index_to_workdir() {
            if let Some(old_path) = delta.old_file().path().filter(|path| include_path(path)) {
                paths.insert(old_path.to_path_buf());
            }
            if let Some(new_path) = delta.new_file().path().filter(|path| include_path(path)) {
                paths.insert(new_path.to_path_buf());
            }
        }

        if include_path(&path) {
            paths.insert(path);
        }
    }

    Ok((paths, untracked_count))
}

pub fn working_tree_paths(root_path: &Path) -> Result<Vec<PathBuf>, Error> {
    let (paths, _) = collect_dirty_paths(root_path, |path| !is_generated_path(path))?;
    Ok(paths.into_iter().collect())
}

pub fn tracked_working_tree_paths(root_path: &Path) -> Result<Vec<PathBuf>, Error> {
    let (paths, _) = collect_dirty_paths(root_path, |_| true)?;
    Ok(paths.into_iter().collect())
}

pub fn materialize_working_tree(source_root: &Path, destination_root: &Path) -> Result<(), Error> {
    validate_portable_worktree(source_root)?;
    remove_existing_path(destination_root).with_context(|_| IoSnafu {
        path: destination_root.to_path_buf(),
    })?;
    std::fs::create_dir_all(destination_root).with_context(|_| IoSnafu {
        path: destination_root.to_path_buf(),
    })?;

    let mut paths = tracked_paths(source_root, |_| true)?;
    paths.extend(tracked_working_tree_paths(source_root)?);
    sync_working_tree_paths(source_root, destination_root, &paths)
}

pub fn sync_working_tree_paths(
    source_root: &Path,
    destination_root: &Path,
    relative_paths: &BTreeSet<PathBuf>,
) -> Result<(), Error> {
    let plan = crate::tree_sync::Plan::new(source_root, destination_root, relative_paths)
        .with_context(|_| IoSnafu {
            path: destination_root,
        })?;
    plan.prepare_destination(destination_root)
        .with_context(|_| IoSnafu {
            path: destination_root,
        })?;
    for path in &plan.files {
        sync_working_tree_path(source_root, destination_root, path)?;
    }

    Ok(())
}

pub fn index_path(root_path: &Path) -> Result<PathBuf, Error> {
    let repo = git2::Repository::open(root_path).context(GitSnafu {
        what: "open root repository",
    })?;
    let index = repo.index().context(GitSnafu {
        what: "open root repository index",
    })?;
    index
        .path()
        .map(Path::to_path_buf)
        .with_context(|| UpdateTreeSnafu::<String> {
            msg: "repository index is in-memory".into(),
        })
}

pub fn sync_materialized_paths(
    source_root: &Path,
    build_repo_path: &Path,
    relative_paths: &BTreeSet<PathBuf>,
) -> Result<(), Error> {
    if relative_paths.is_empty() {
        return Ok(());
    }

    let working_repo = open_generated_repository(build_repo_path)?;
    let working_root = working_repo
        .workdir()
        .with_context(|| UpdateTreeSnafu::<String> {
            msg: "build repository workdir is unavailable".into(),
        })?;
    let mut index = working_repo.index().context(GitSnafu {
        what: "open build repository index",
    })?;

    sync_dirty_paths(source_root, working_root, &mut index, relative_paths)?;

    index.write().context(GitSnafu {
        what: "write build repository index",
    })?;

    Ok(())
}

fn remove_existing_path(path: &Path) -> Result<(), std::io::Error> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path)
        }
        Ok(_) => std::fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn tracked_paths(
    root_path: &Path,
    include_path: impl Fn(&Path) -> bool,
) -> Result<BTreeSet<PathBuf>, Error> {
    let repo = git2::Repository::open(root_path).context(GitSnafu {
        what: "open root repository",
    })?;
    let head = repo.head().context(GitSnafu { what: "head" })?;
    let commit = head.peel_to_commit().context(GitSnafu {
        what: "peel head to commit",
    })?;
    let tree = commit.tree().context(GitSnafu { what: "head tree" })?;
    validate_portable_tree(&tree)?;
    let mut paths = BTreeSet::new();
    let mut walk_error = None;

    let walk_result = tree.walk(git2::TreeWalkMode::PreOrder, |prefix, entry| {
        let Some(name) = entry.name() else {
            walk_error = Some(
                UpdateTreeSnafu {
                    msg: format!("tree entry without name in `{prefix}`"),
                }
                .build(),
            );
            return TreeWalkResult::Abort;
        };

        match entry.kind() {
            Some(ObjectType::Blob) => (),
            Some(ObjectType::Tree) => return TreeWalkResult::Ok,
            kind => {
                walk_error = Some(
                    UpdateTreeSnafu {
                        msg: format!("unknown blob type `{kind:?}` for `{}{name}`", prefix),
                    }
                    .build(),
                );
                return TreeWalkResult::Abort;
            }
        }

        let path = PathBuf::from(format!("{prefix}{name}"));
        if include_path(&path) {
            paths.insert(path);
        }

        TreeWalkResult::Ok
    });

    if let Some(error) = walk_error {
        return Err(error);
    }

    walk_result.context(GitSnafu {
        what: "traverse tree",
    })?;

    Ok(paths)
}

fn remove_index_path(index: &mut git2::Index, path: &Path) -> Result<(), Error> {
    match index.remove_path(path) {
        Ok(()) => Ok(()),
        Err(error) if error.code() == git2::ErrorCode::NotFound => match index.remove_dir(path, -1)
        {
            Ok(()) => Ok(()),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(()),
            Err(error) => Err(GitSnafu {
                what: "remove dirty path from index",
            }
            .into_error(error)),
        },
        Err(error) => Err(GitSnafu {
            what: "remove dirty path from index",
        }
        .into_error(error)),
    }
}

#[cfg(target_family = "unix")]
pub(crate) fn copy_symlink(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    let target = std::fs::read_link(source)?;
    std::os::unix::fs::symlink(target, destination)
}

#[cfg(target_family = "windows")]
pub(crate) fn copy_symlink(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    let target = std::fs::read_link(source)?;
    let resolved_target = source
        .parent()
        .map(|parent| parent.join(&target))
        .unwrap_or_else(|| target.clone());

    if std::fs::metadata(&resolved_target)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
    {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    }
}

#[cfg(not(any(target_family = "unix", target_family = "windows")))]
pub(crate) fn copy_symlink(_source: &Path, _destination: &Path) -> Result<(), std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no symlink implementation available",
    ))
}

pub(crate) fn validate_attribute_inputs(
    root: &Path,
    paths: &BTreeSet<PathBuf>,
) -> Result<(), Error> {
    for path in paths
        .iter()
        .filter(|p| p.file_name().is_some_and(|n| n == ".gitattributes"))
    {
        let metadata = crate::tree_sync::inspect(root, path).with_context(|_| IoSnafu {
            path: root.join(path),
        })?;
        ensure!(metadata.is_none_or(|m| m.is_file() && !crate::generated_fs::is_link(&m)), UpdateTreeSnafu {
            msg: format!("Git attribute configuration `{}` must be an ordinary file, not a link or directory", path.display()),
        });
    }
    Ok(())
}

fn sync_dirty_paths(
    source_root: &Path,
    working_root: &Path,
    index: &mut git2::Index,
    relative_paths: &BTreeSet<PathBuf>,
) -> Result<(), Error> {
    validate_attribute_inputs(source_root, relative_paths)?;
    let plan = crate::tree_sync::Plan::new(source_root, working_root, relative_paths)
        .with_context(|_| IoSnafu { path: working_root })?;
    plan.prepare_destination(working_root)
        .with_context(|_| IoSnafu { path: working_root })?;
    // Remove old tree entries before inserting replacements. A directory/file
    // exchange is one transition, not contradictory sequential path operations.
    for path in relative_paths {
        remove_index_path(index, path)?;
    }
    for path in &plan.files {
        sync_working_tree_path(source_root, working_root, path)?;
        index.add_path(path).context(GitSnafu {
            what: "add materialized path to index",
        })?;
    }
    Ok(())
}

fn sync_working_tree_path(
    source_root: &Path,
    destination_root: &Path,
    relative_path: &Path,
) -> Result<(), Error> {
    crate::generated_fs::check_path(source_root, &source_root.join(relative_path), true)
        .with_context(|_| IoSnafu {
            path: source_root.join(relative_path),
        })?;
    crate::generated_fs::check_path(
        destination_root,
        &destination_root.join(relative_path),
        true,
    )
    .with_context(|_| IoSnafu {
        path: destination_root.join(relative_path),
    })?;
    let source_path = source_root.join(relative_path);
    let destination_path = destination_root.join(relative_path);

    match std::fs::symlink_metadata(&source_path) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            remove_existing_path(&destination_path).with_context(|_| IoSnafu {
                path: destination_path.clone(),
            })
        }
        Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            if let Some(parent) = destination_path.parent() {
                std::fs::create_dir_all(parent).with_context(|_| IoSnafu {
                    path: parent.to_path_buf(),
                })?;
            }

            remove_existing_path(&destination_path).with_context(|_| IoSnafu {
                path: destination_path.clone(),
            })?;

            if metadata.file_type().is_symlink() {
                copy_symlink(&source_path, &destination_path).with_context(|_| IoSnafu {
                    path: destination_path.clone(),
                })?;
            } else {
                std::fs::copy(&source_path, &destination_path).with_context(|_| IoSnafu {
                    path: source_path.clone(),
                })?;
            }

            Ok(())
        }
        Ok(_) => DirtyUnsupportedPathSnafu {
            path: relative_path.to_path_buf(),
        }
        .fail(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            remove_existing_path(&destination_path).with_context(|_| IoSnafu {
                path: destination_path,
            })
        }
        Err(error) => Err(IoSnafu { path: source_path }.into_error(error)),
    }
}

fn materialize_dirty_tree(
    source_root: &Path,
    working_repo: &git2::Repository,
    local_head: Oid,
) -> Result<Oid, Error> {
    let (dirty_paths, untracked_count) =
        collect_dirty_paths(source_root, |path| !is_generated_path(path))?;
    if untracked_count > 0 {
        info!("dirty mode ignores untracked files in the active content repo");
    }

    if dirty_paths.is_empty() {
        return Ok(local_head);
    }

    let working_root = working_repo
        .workdir()
        .with_context(|| UpdateTreeSnafu::<String> {
            msg: "build repository workdir is unavailable".into(),
        })?;
    let mut index = working_repo.index().context(GitSnafu {
        what: "open build repository index",
    })?;

    sync_dirty_paths(source_root, working_root, &mut index, &dirty_paths)?;

    index.write().context(GitSnafu {
        what: "write build repository index",
    })?;
    let tree_id = index.write_tree().context(GitSnafu {
        what: "write dirty materialization tree",
    })?;
    let tree = working_repo.find_tree(tree_id).context(GitSnafu {
        what: "find dirty materialization tree",
    })?;

    let sig = Signature::now("eips-build", "eips-build@eips-build.invalid").context(GitSnafu {
        what: "dirty commit signature",
    })?;
    let parent = working_repo.find_commit(local_head).context(GitSnafu {
        what: "find clean local head commit",
    })?;
    let dirty_head = working_repo
        .commit(
            Some("HEAD"),
            &sig,
            &sig,
            "Dirty working tree materialization",
            &tree,
            &[&parent],
        )
        .context(GitSnafu {
            what: "commit dirty working tree materialization",
        })?;

    info!(
        "materialized tracked dirty changes from the active content repo into `{}`",
        working_root.to_string_lossy()
    );

    Ok(dirty_head)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EntryIdentity {
    pub(crate) id: Oid,
    pub(crate) mode: i32,
    pub(crate) kind: Option<ObjectType>,
}

pub(crate) fn check_entry_conflict(
    path: &Path,
    left: EntryIdentity,
    right: EntryIdentity,
) -> Result<(), Error> {
    for (equal, field) in [
        (left.mode == right.mode, "filemode"),
        (left.kind == right.kind, "kind"),
        (left.id == right.id, "id"),
    ] {
        ensure!(
            equal,
            UpdateTreeSnafu {
                msg: format!("conflicting path `{}` ({field})", path.display())
            }
        );
    }
    Ok(())
}

fn filemode(metadata: &std::fs::Metadata, previous: Option<u32>, trust_mode: bool) -> i32 {
    if metadata.file_type().is_symlink() {
        return 0o120000;
    }
    if metadata.is_dir() {
        return 0o040000;
    }
    if !trust_mode {
        return if previous == Some(0o100755) {
            0o100755
        } else {
            0o100644
        };
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o100 != 0 {
            return 0o100755;
        }
    }
    0o100644
}

pub(crate) fn working_entry_identity(
    repo: &git2::Repository,
    source: &Path,
    relative: &Path,
) -> Result<Option<EntryIdentity>, Error> {
    let path = source.join(relative);
    crate::generated_fs::check_path(source, &path, true)
        .with_context(|_| IoSnafu { path: &path })?;
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Ok(None)
        }
        Err(e) => return Err(IoSnafu { path }.into_error(e)),
    };
    let previous = repo
        .index()
        .context(GitSnafu {
            what: "read mode normalization index",
        })?
        .get_path(relative, 0)
        .map(|e| e.mode);
    let trust_mode = repo
        .config()
        .context(GitSnafu {
            what: "read mode normalization policy",
        })?
        .get_bool("core.filemode")
        .unwrap_or(cfg!(unix));
    let mode = filemode(&meta, previous, trust_mode);
    if meta.is_dir() && !meta.file_type().is_symlink() {
        return Ok(Some(EntryIdentity {
            id: Oid::zero(),
            mode,
            kind: Some(ObjectType::Tree),
        }));
    }
    let bytes = if meta.file_type().is_symlink() {
        std::fs::read_link(&path)
            .with_context(|_| IoSnafu { path: &path })?
            .as_os_str()
            .as_encoded_bytes()
            .to_vec()
    } else if meta.is_file() {
        std::fs::read(&path).with_context(|_| IoSnafu { path: &path })?
    } else {
        return DirtyUnsupportedPathSnafu {
            path: relative.to_path_buf(),
        }
        .fail();
    };
    let id = if meta.file_type().is_symlink() {
        // Git stores link text verbatim; never filter or dereference its target.
        Oid::hash_object(ObjectType::Blob, &bytes).context(GitSnafu {
            what: "hash source link",
        })?
    } else {
        // Match Index::add_path's clean filters, not the physical worktree bytes.
        // This writes only an immutable blob in the private generated repository.
        let mut writer = repo.blob_writer(Some(relative)).context(GitSnafu {
            what: "create filtered source identity",
        })?;
        std::io::Write::write_all(&mut writer, &bytes).with_context(|_| IoSnafu { path: &path })?;
        writer.commit().context(GitSnafu {
            what: "resolve filtered source identity",
        })?
    };
    Ok(Some(EntryIdentity {
        id,
        mode,
        kind: Some(ObjectType::Blob),
    }))
}

fn check_conflict(master_tree: &Tree, path: &Path, entry: &TreeEntry) -> Result<(), Error> {
    let original = match master_tree.get_path(path) {
        Err(_) => {
            for parent in path
                .ancestors()
                .skip(1)
                .filter(|p| !p.as_os_str().is_empty())
            {
                if let Ok(original) = master_tree.get_path(parent) {
                    if original.kind() != Some(ObjectType::Tree) {
                        check_entry_conflict(
                            parent,
                            EntryIdentity {
                                id: original.id(),
                                mode: original.filemode(),
                                kind: original.kind(),
                            },
                            EntryIdentity {
                                id: Oid::zero(),
                                mode: 0o040000,
                                kind: Some(ObjectType::Tree),
                            },
                        )?;
                    }
                }
            }
            return Ok(());
        }
        Ok(o) => o,
    };
    check_entry_conflict(
        path,
        EntryIdentity {
            id: original.id(),
            mode: original.filemode(),
            kind: original.kind(),
        },
        EntryIdentity {
            id: entry.id(),
            mode: entry.filemode(),
            kind: entry.kind(),
        },
    )
}

pub struct Fresh {
    src_repo_use: RepositoryUse,
    src_repo_path: PathBuf,
    src_repo_url: Url,
    source_materialization: SourceMaterialization,

    working_repo: git2::Repository,
}

impl Fresh {
    pub fn new(
        root_path: &Path,
        repo_path: &Path,
        src_repo_use: RepositoryUse,
        source_materialization: SourceMaterialization,
    ) -> Result<Self, Error> {
        let root_path = absolute(root_path).with_context(|_| IoSnafu { path: root_path })?;
        validate_portable_worktree(&root_path)?;
        if source_materialization == SourceMaterialization::Clean {
            check_dirty(&root_path)?;
        }
        let src_repo_url =
            Url::from_directory_path(&root_path)
                .ok()
                .with_context(|| PathUrlSnafu {
                    path: root_path.clone(),
                })?;

        debug!("source repository at `{src_repo_url}`");

        crate::generated_fs::check_source_destination(&root_path, repo_path).with_context(
            |_| IoSnafu {
                path: repo_path.to_path_buf(),
            },
        )?;
        let working_repo = open_or_init(repo_path)?;

        Ok(Self {
            working_repo,
            src_repo_path: root_path,
            src_repo_url,
            src_repo_use,
            source_materialization,
        })
    }

    pub fn clone_src(self) -> Result<SourceOnly, Error> {
        info!("cloning local repository");
        let master = fetch(
            &self.working_repo,
            self.src_repo_url.as_str(),
            "+HEAD:refs/build-eips/source-head",
        )?;
        {
            let master_tree = master.tree().context(GitSnafu {
                what: "read active source tree",
            })?;
            validate_portable_tree(&master_tree)?;
        }

        self.working_repo
            .set_head_detached(master.id())
            .context(GitSnafu { what: "detach" })?;
        let branch = self
            .working_repo
            .branch("master", &master, true)
            .context(GitSnafu {
                what: "branch master",
            })?;
        self.working_repo
            .set_head("refs/heads/master")
            .context(GitSnafu { what: "set head" })?;
        assert!(branch.is_head());
        self.working_repo
            .checkout_head(Some(
                CheckoutBuilder::default()
                    .remove_ignored(true)
                    .remove_untracked(true)
                    .force(),
            ))
            .context(GitSnafu {
                what: "checkout local",
            })?;

        if !self.working_repo.submodules().unwrap().is_empty() {
            panic!("submodules not supported yet");
        }

        let source_head = master.id();
        let mut local_head = source_head;
        drop(master);
        drop(branch);

        if self.source_materialization == SourceMaterialization::Dirty {
            local_head =
                materialize_dirty_tree(&self.src_repo_path, &self.working_repo, local_head)?;
        }

        Ok(SourceOnly {
            source_head,
            local_head,
            src_repo_use: self.src_repo_use,
            working_repo: self.working_repo,
        })
    }
}

/// Exact source commit plus its optional tracked-working-tree overlay commit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SourceGeneration {
    pub(crate) source_head: Oid,
    pub(crate) materialized_head: Oid,
}

pub struct SourceOnly {
    source_head: Oid,
    src_repo_use: RepositoryUse,

    working_repo: git2::Repository,
    local_head: Oid,
}

impl SourceOnly {
    pub(crate) fn source_generation(&self) -> SourceGeneration {
        SourceGeneration {
            source_head: self.source_head,
            materialized_head: self.local_head,
        }
    }

    pub fn merge(&self) -> Result<(), Error> {
        merge_sibling_repositories(&self.working_repo, &self.src_repo_use, self.local_head)
    }

    pub fn fetch_upstream(self) -> Result<SourceWithUpstream, Error> {
        info!("fetching latest {} repository", self.src_repo_use.title);
        let latest_master = fetch(
            &self.working_repo,
            self.src_repo_use.location.repository.as_str(),
            "+master:refs/build-eips/upstream-head",
        )?;
        let upstream_head = latest_master.id();
        drop(latest_master);
        Ok(SourceWithUpstream {
            upstream_head,
            local_head: self.local_head,
            src_repo_use: self.src_repo_use,
            working_repo: self.working_repo,
        })
    }
}

pub struct SourceWithUpstream {
    src_repo_use: RepositoryUse,

    working_repo: git2::Repository,
    local_head: Oid,
    upstream_head: Oid,
}

impl SourceWithUpstream {
    fn local_head_tree(&self) -> Result<Tree<'_>, Error> {
        let commit = self
            .working_repo
            .find_commit(self.local_head)
            .context(GitSnafu {
                what: "local commit from id",
            })?;
        let master_tree = commit.tree().context(GitSnafu {
            what: "getting master tree",
        })?;

        Ok(master_tree)
    }

    pub fn changed_files(&self) -> Result<Vec<PathBuf>, Error> {
        let merge_base = self
            .working_repo
            .merge_base(self.local_head, self.upstream_head)
            .context(GitSnafu { what: "merge base" })?;
        debug!(
            "merge base of `{}` (local) and `{}` (latest) is `{}`",
            self.local_head, self.upstream_head, merge_base
        );

        let merge_base_tree = self
            .working_repo
            .find_commit(merge_base)
            .context(GitSnafu {
                what: "getting merge base commit",
            })?
            .tree()
            .context(GitSnafu {
                what: "getting merge base tree",
            })?;

        let master_tree = self.local_head_tree()?;
        let diff = self
            .working_repo
            .diff_tree_to_tree(Some(&merge_base_tree), Some(&master_tree), None)
            .context(GitSnafu {
                what: "comparing merge base to master",
            })?;

        let changed_files = diff
            .deltas()
            .filter_map(|d| d.new_file().path())
            .map(Path::to_path_buf)
            .collect();

        Ok(changed_files)
    }

    pub fn merge(&self) -> Result<(), Error> {
        merge_sibling_repositories(&self.working_repo, &self.src_repo_use, self.local_head)
    }
}

pub(crate) fn check_ignored(working_repo: &git2::Repository, tree: &Tree) -> Result<(), Error> {
    let mut walk_error = None;
    let walk_result = tree.walk(git2::TreeWalkMode::PreOrder, |a, b| {
        if b.kind() != Some(ObjectType::Blob) {
            return TreeWalkResult::Ok;
        }

        let path = match b.name() {
            None => a.to_owned(),
            Some(p) => format!("{a}{p}"),
        };

        debug!("checking if `{path}` is ignored");

        match working_repo.is_path_ignored(&path) {
            Ok(false) => TreeWalkResult::Ok,
            Ok(true) => {
                walk_error = Some(
                    UpdateTreeSnafu {
                        msg: format!("contains ignored path `{path}`"),
                    }
                    .build(),
                );
                TreeWalkResult::Abort
            }
            Err(e) => {
                walk_error = Some(
                    GitSnafu {
                        what: "check ignored",
                    }
                    .into_error(e),
                );
                TreeWalkResult::Abort
            }
        }
    });

    if let Some(error) = walk_error {
        return Err(error);
    }

    walk_result.context(GitSnafu {
        what: "traverse tree",
    })?;

    Ok(())
}

fn merge_sibling_repositories(
    working_repo: &git2::Repository,
    repo_use: &RepositoryUse,
    mut local_head: Oid,
) -> Result<(), Error> {
    for (index, (other_kind, other_repo)) in repo_use
        .other_repos
        .iter()
        .progress_ext("Merge Repos")
        .enumerate()
    {
        let local_commit = working_repo.find_commit(local_head).context(GitSnafu {
            what: "find local head commit",
        })?;
        let local_tree = local_commit.tree().context(GitSnafu {
            what: "getting local head tree",
        })?;
        info!("fetching {other_kind} repository");
        // Local sibling overrides should follow the checked-out repo HEAD instead of assuming `master`.
        let other_ref = format!("refs/build-eips/other-head-{index}");
        let other_refspec = if other_repo.scheme() == "file" {
            format!("+HEAD:{other_ref}")
        } else {
            format!("+master:{other_ref}")
        };
        let master_other = fetch(working_repo, other_repo.as_str(), &other_refspec)?;
        if other_repo.scheme() == "file" {
            validate_sibling_commit(working_repo, &master_other, other_kind).map_err(|error| {
                UpdateTreeSnafu {
                    msg: snafu::Report::from_error(error).to_string(),
                }
                .build()
            })?;
        }
        let other_tree = master_other.tree().context(GitSnafu {
            what: "getting other tree",
        })?;

        let mut tree_builder = TreeUpdateBuilder::new();
        let prefix = format!("{}/", CONTENT_DIR);
        let mut walk_error: Option<Error> = None;
        let walk_result = other_tree.walk(git2::TreeWalkMode::PreOrder, |a, b| {
            if !a.starts_with(&prefix) && (!a.is_empty() || b.name() != Some(CONTENT_DIR)) {
                return TreeWalkResult::Skip;
            }

            let name = match b.name() {
                Some(n) => n,
                None => {
                    walk_error = Some(
                        UpdateTreeSnafu {
                            msg: format!("tree entry without name in `{a}`"),
                        }
                        .build(),
                    );
                    return TreeWalkResult::Abort;
                }
            };

            let path = format!("{}{}", a, name);
            match b.kind() {
                Some(ObjectType::Blob) => (),
                Some(ObjectType::Tree) => return TreeWalkResult::Ok,
                kind => {
                    walk_error = Some(
                        UpdateTreeSnafu {
                            msg: format!("unknown blob type `{kind:?}` for `{path}`"),
                        }
                        .build(),
                    );
                    return TreeWalkResult::Abort;
                }
            }

            if path == CONTENT_INDEX_PATH {
                debug!("skip sibling homepage `{path}`");
                return TreeWalkResult::Ok;
            }

            if let Err(e) = check_conflict(&local_tree, Path::new(&path), b) {
                walk_error = Some(e);
                return TreeWalkResult::Abort;
            }

            debug!("upsert `{path}`");
            let mode = match b.filemode() {
                0o120000 => FileMode::Link,
                0o100755 => FileMode::BlobExecutable,
                _ => FileMode::Blob,
            };
            tree_builder.upsert(path, b.id(), mode);
            TreeWalkResult::Ok
        });

        if let Some(error) = walk_error {
            return Err(error);
        }

        walk_result.context(GitSnafu {
            what: "traverse tree",
        })?;

        let merged_tree_oid = tree_builder
            .create_updated(working_repo, &local_tree)
            .context(GitSnafu { what: "build tree" })?;
        let merged_tree = working_repo.find_tree(merged_tree_oid).unwrap();
        validate_portable_tree(&merged_tree)?;

        check_ignored(working_repo, &merged_tree)?;

        let sig =
            Signature::now("eips-build", "eips-build@eips-build.invalid").context(GitSnafu {
                what: "commit signature",
            })?;
        let msg = format!("Merge {other_repo}");
        local_head = working_repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                &msg,
                &merged_tree,
                &[&local_commit, &master_other],
            )
            .context(GitSnafu { what: "committing" })?;

        working_repo
            .checkout_head(Some(CheckoutBuilder::default().force()))
            .context(GitSnafu {
                what: "checkout merged",
            })?;

        drop(merged_tree);
        drop(other_tree);
        drop(master_other);
        drop(local_tree);
        drop(local_commit);
        match working_repo.find_reference(&other_ref) {
            Ok(mut reference) => {
                if let Err(error) = reference.delete() {
                    debug!("unable to delete temporary sibling ref `{other_ref}`: {error}");
                }
            }
            Err(error) => {
                debug!("temporary sibling ref `{other_ref}` was not deleted: {error}");
            }
        }
    }

    Ok(())
}

fn fetch<'a>(
    repo: &'a git2::Repository,
    url: &'_ str,
    refspec: &'_ str,
) -> Result<Commit<'a>, Error> {
    debug!("fetching repository at `{url}`");
    let remote_name = "__build_eips_fetch";
    match repo.remote_delete(remote_name) {
        Ok(()) => (),
        Err(error) if error.code() == git2::ErrorCode::NotFound => (),
        Err(error) => {
            return Err(GitSnafu {
                what: "deleting temporary remote",
            }
            .into_error(error))
        }
    }

    let mut remote = repo.remote(remote_name, url).context(GitSnafu {
        what: "creating remote",
    })?;
    {
        let git_progress = Git::new();
        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(git_progress.remote_callbacks());
        remote
            .fetch(&[refspec], Some(&mut fetch_options), None)
            .context(GitSnafu {
                what: "fetching repo",
            })?;
    }
    drop(remote);
    repo.remote_delete(remote_name).context(GitSnafu {
        what: "deleting temporary remote",
    })?;

    let fetched_ref = refspec
        .split_once(':')
        .map(|(_, destination)| destination)
        .filter(|destination| !destination.is_empty())
        .unwrap_or("FETCH_HEAD");
    let commit = repo
        .revparse_single(fetched_ref)
        .context(GitSnafu {
            what: "revparse fetched ref",
        })?
        .peel_to_commit()
        .context(GitSnafu {
            what: "peel fetched ref",
        })?;
    Ok(commit)
}

pub(crate) fn open_generated_repository(dir: &Path) -> Result<git2::Repository, Error> {
    crate::generated_git::check_metadata_path(dir).with_context(|_| IoSnafu { path: dir })?;
    let repo = git2::Repository::open_ext(dir, RepositoryOpenFlags::NO_SEARCH, &[] as &[&OsStr])
        .context(GitSnafu {
            what: "open owned generated repository",
        })?;
    crate::generated_git::validate(&repo, dir).with_context(|_| IoSnafu { path: dir })?;
    Ok(repo)
}

fn open_or_init(dir: &Path) -> Result<git2::Repository, Error> {
    crate::generated_git::check_metadata_path(dir).with_context(|_| IoSnafu { path: dir })?;
    let repo =
        match git2::Repository::open_ext(dir, RepositoryOpenFlags::NO_SEARCH, &[] as &[&OsStr]) {
            Ok(r) => r,
            Err(e) if e.code() == git2::ErrorCode::NotFound => {
                git2::Repository::init(dir).context(GitSnafu { what: "init repo" })?
            }
            Err(e) => return Err(GitSnafu { what: "open repo" }.into_error(e)),
        };
    crate::generated_git::validate(&repo, dir).with_context(|_| IoSnafu { path: dir })?;
    #[cfg(windows)]
    repo.config().and_then(|mut config| config.set_bool("core.longpaths", true))
        .context(GitSnafu { what: "enable long paths in private generated checkout; alternatively use a shorter --build-root" })?;
    Ok(repo)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use git2::{IndexAddOption, Repository, Signature};
    use tempfile::TempDir;

    use super::{
        checkout_fresh_clone_at_commit, materialize_working_tree, sync_working_tree_paths,
        tracked_working_tree_paths, Fresh, SourceMaterialization,
    };
    use crate::config::{Location, RepositoryUse};

    #[cfg(windows)]
    #[test]
    fn private_checkout_handles_long_paths_without_global_configuration() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("internal-generated-checkout-".repeat(3));
        let repo = super::open_or_init(&root).unwrap();
        assert!(repo.config().unwrap().get_bool("core.longpaths").unwrap());
        let name = format!("{}.pdf", "existing-documentation-asset-".repeat(5));
        assert!(root.join(&name).as_os_str().len() > 260);
        let blob = repo.blob(b"fixture PDF bytes").unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert(&name, blob, 0o100644).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("fixture", "fixture@example.test").unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "long-path fixture",
            &tree,
            &[],
        )
        .unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        assert_eq!(
            std::fs::read(root.canonicalize().unwrap().join(name)).unwrap(),
            b"fixture PDF bytes"
        );
    }

    fn file_url(path: &Path) -> url::Url {
        url::Url::from_directory_path(path).unwrap()
    }

    fn write_file(root: &Path, relative: impl AsRef<Path>, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn commit_all(repo: &Repository, message: &str) {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let signature = Signature::now("build-eips test", "build-eips@example.test").unwrap();
        let parents = repo
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| repo.find_commit(oid).unwrap())
            .into_iter()
            .collect::<Vec<_>>();
        let parent_refs = parents.iter().collect::<Vec<_>>();

        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parent_refs,
        )
        .unwrap();
    }

    fn init_repo(path: &Path, files: &[(&str, &str)]) -> Repository {
        std::fs::create_dir_all(path).unwrap();
        let repo = Repository::init(path).unwrap();
        repo.set_head("refs/heads/master").unwrap();
        for (relative, contents) in files {
            write_file(path, relative, contents);
        }
        commit_all(&repo, "initial");
        repo
    }

    fn stage_path(repo: &Repository, relative: &str) {
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(relative)).unwrap();
        index.write().unwrap();
    }

    #[test]
    fn checkout_fresh_clone_missing_destination_returns_error() {
        let temp = TempDir::new().unwrap();
        let destination = temp.path().join("missing");
        let error =
            checkout_fresh_clone_at_commit(&destination, "https://example.test/theme.git", "main")
                .unwrap_err();

        assert!(matches!(
            error,
            super::Error::MissingFreshWorkspaceRepository { .. }
        ));
    }

    #[test]
    fn checkout_fresh_clone_fetches_missing_manifest_commit() {
        let temp = TempDir::new().unwrap();
        let source_path = temp.path().join("source");
        let source = init_repo(
            &source_path,
            &[(
                "README.md",
                "source
",
            )],
        );
        write_file(
            &source_path,
            "PINNED.md",
            "pinned
",
        );
        commit_all(&source, "pinned commit");
        let pinned_commit = source.head().unwrap().target().unwrap().to_string();
        let destination = temp.path().join("destination");
        init_repo(
            &destination,
            &[(
                "README.md",
                "destination
",
            )],
        );

        checkout_fresh_clone_at_commit(
            &destination,
            file_url(&source_path).as_str(),
            &pinned_commit,
        )
        .unwrap();

        let destination_repo = Repository::open(&destination).unwrap();
        assert_eq!(
            destination_repo
                .head()
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            pinned_commit
        );
        assert!(destination_repo
            .find_reference("refs/build-eips/theme-pin")
            .is_ok());
        assert_eq!(
            std::fs::read_to_string(destination.join("PINNED.md"))
                .unwrap()
                .replace("\r\n", "\n"),
            "pinned
"
        );
    }

    #[test]
    fn merge_skips_sibling_homepage_and_keeps_sibling_proposals() {
        let temp = TempDir::new().unwrap();
        let active = temp.path().join("active");
        let sibling = temp.path().join("sibling");
        let prepared = temp.path().join("prepared");

        init_repo(
            &active,
            &[
                ("content/_index.md", "active homepage\n"),
                ("content/00555.md", "# Active proposal\n"),
            ],
        );
        init_repo(
            &sibling,
            &[
                ("content/_index.md", "sibling homepage\n"),
                ("content/00678.md", "# Sibling proposal\n"),
            ],
        );

        let mut other_repos = std::collections::HashMap::new();
        other_repos.insert("ERCs".to_owned(), file_url(&sibling));
        let active_url = file_url(&active);
        let repository_use = RepositoryUse {
            title: "EIPs".to_owned(),
            location: Location {
                repository: active_url,
                base_url: "https://eips.example.test/".parse().unwrap(),
            },
            other_repos,
        };

        Fresh::new(
            &active,
            &prepared,
            repository_use,
            SourceMaterialization::Clean,
        )
        .unwrap()
        .clone_src()
        .unwrap()
        .fetch_upstream()
        .unwrap()
        .merge()
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(prepared.join("content/_index.md"))
                .unwrap()
                .replace("\r\n", "\n"),
            "active homepage\n"
        );
        assert!(prepared.join("content/00678.md").is_file());
    }

    #[test]
    fn materialize_working_tree_uses_tracked_theme_scope() {
        let temp = TempDir::new().unwrap();
        let theme = temp.path().join("theme");
        let mounted = temp.path().join("repo/themes/eips-theme");
        let repo = init_repo(
            &theme,
            &[
                ("config/zola.toml", "title = 'theme'\n"),
                ("build/generated.txt", "tracked build path\n"),
                ("delete.txt", "delete me\n"),
                ("staged.txt", "old staged\n"),
                ("tracked.txt", "old tracked\n"),
            ],
        );

        write_file(&theme, "tracked.txt", "unstaged tracked edit\n");
        write_file(&theme, "staged.txt", "staged tracked edit\n");
        stage_path(&repo, "staged.txt");
        write_file(&theme, "new-staged.txt", "new staged file\n");
        stage_path(&repo, "new-staged.txt");
        std::fs::remove_file(theme.join("delete.txt")).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("delete.txt")).unwrap();
        index.write().unwrap();
        write_file(
            &theme,
            "untracked.txt",
            "ignored by theme materialization\n",
        );

        materialize_working_tree(&theme, &mounted).unwrap();

        assert_eq!(
            std::fs::read_to_string(mounted.join("config/zola.toml")).unwrap(),
            "title = 'theme'\n"
        );
        assert_eq!(
            std::fs::read_to_string(mounted.join("build/generated.txt")).unwrap(),
            "tracked build path\n"
        );
        assert_eq!(
            std::fs::read_to_string(mounted.join("tracked.txt")).unwrap(),
            "unstaged tracked edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(mounted.join("staged.txt")).unwrap(),
            "staged tracked edit\n"
        );
        assert_eq!(
            std::fs::read_to_string(mounted.join("new-staged.txt")).unwrap(),
            "new staged file\n"
        );
        assert!(!mounted.join("delete.txt").exists());
        assert!(!mounted.join("untracked.txt").exists());
    }

    #[test]
    fn newly_staged_theme_file_syncs_after_git_index_rescan() {
        let temp = TempDir::new().unwrap();
        let theme = temp.path().join("theme");
        let mounted = temp.path().join("repo/themes/eips-theme");
        let repo = init_repo(&theme, &[("config/zola.toml", "title = 'theme'\n")]);
        materialize_working_tree(&theme, &mounted).unwrap();
        let mut previous_dirty_paths = tracked_working_tree_paths(&theme)
            .unwrap()
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();

        write_file(&theme, "templates/new.html", "new staged template\n");
        stage_path(&repo, "templates/new.html");
        let current_dirty_paths = tracked_working_tree_paths(&theme)
            .unwrap()
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let affected_paths = previous_dirty_paths
            .union(&current_dirty_paths)
            .cloned()
            .collect();

        sync_working_tree_paths(&theme, &mounted, &affected_paths).unwrap();
        previous_dirty_paths = current_dirty_paths;

        assert_eq!(
            std::fs::read_to_string(mounted.join("templates/new.html")).unwrap(),
            "new staged template\n"
        );
        assert!(previous_dirty_paths.contains(Path::new("templates/new.html")));
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn tracked_theme_symlinks_are_materialized_as_symlinks() {
        let temp = TempDir::new().unwrap();
        let theme = temp.path().join("theme");
        let mounted = temp.path().join("repo/themes/eips-theme");
        std::fs::create_dir_all(&theme).unwrap();
        let repo = Repository::init(&theme).unwrap();
        repo.set_head("refs/heads/master").unwrap();
        write_file(&theme, "target.txt", "target\n");
        std::os::unix::fs::symlink("target.txt", theme.join("linked.txt")).unwrap();
        commit_all(&repo, "initial");

        materialize_working_tree(&theme, &mounted).unwrap();

        assert!(std::fs::symlink_metadata(mounted.join("linked.txt"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(mounted.join("linked.txt")).unwrap(),
            PathBuf::from("target.txt")
        );
    }

    #[test]
    fn materialize_working_tree_requires_git_repository() {
        let temp = TempDir::new().unwrap();
        let theme = temp.path().join("theme");
        let mounted = temp.path().join("repo/themes/eips-theme");
        std::fs::create_dir_all(&theme).unwrap();

        let error = materialize_working_tree(&theme, &mounted).unwrap_err();

        assert!(error.to_string().contains("unable to open root repository"));
    }
    fn identity_manifest(name: &str) -> String {
        format!("name = \"{name}\"\n[locations.{name}]\nrepository=\"https://example.test/fork.git\"\nbase-url=\"https://example.test/\"\n[theme]\nrepository=\"https://example.test/theme.git\"\ncommit=\"test\"\n")
    }
    #[test]
    fn sibling_identity_checks_working_and_committed_snapshots() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("ERCs");
        let repo = init_repo(&path, &[("Build.toml", &identity_manifest("WrongRepo"))]);
        assert!(super::validate_sibling_identity(&path, "ERCs").is_err());
        write_file(&path, "Build.toml", &identity_manifest("ERCs"));
        assert!(
            super::validate_sibling_identity(&path, "ERCs").is_err(),
            "working edits cannot mask wrong committed source"
        );
        commit_all(&repo, "correct identity");
        assert!(super::validate_sibling_identity(&path, "ERCs").unwrap());
        write_file(&path, "Build.toml", &identity_manifest("WrongRepo"));
        assert!(super::validate_sibling_identity(&path, "ERCs").is_err());
    }
    #[test]
    fn missing_legacy_manifest_is_allowed_but_parent_repository_is_not() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("ERCs");
        init_repo(&path, &[("content/00002.md", "legacy")]);
        assert!(!super::validate_sibling_identity(&path, "ERCs").unwrap());
        let child = path.join("nested");
        std::fs::create_dir(&child).unwrap();
        assert!(super::validate_sibling_identity(&child, "ERCs").is_err());
        write_file(&path, "Build.toml", "malformed");
        assert!(super::validate_sibling_identity(&path, "ERCs").is_err());
    }

    #[test]
    fn portable_namespace_rejects_case_unicode_and_parent_collisions() {
        for paths in [
            vec![PathBuf::from("A.md"), PathBuf::from("a.md")],
            vec![PathBuf::from("Foo"), PathBuf::from("foo/b.md")],
            vec![PathBuf::from("Foo/A.md"), PathBuf::from("foo/a.md")],
            vec![PathBuf::from("Ä.md"), PathBuf::from("ä.md")],
            vec![PathBuf::from("é.md"), PathBuf::from("e\u{301}.md")],
        ] {
            assert!(
                super::validate_portable_paths(paths.iter()).is_err(),
                "expected collision for {paths:?}"
            );
        }
        for distinct in [
            vec![PathBuf::from("foo/a.md"), PathBuf::from("foo/b.md")],
            vec![PathBuf::from("Foo/a.md"), PathBuf::from("foo/b.md")],
        ] {
            super::validate_portable_paths(distinct.iter()).unwrap();
        }
    }

    #[test]
    fn tracked_case_collisions_fail_on_case_sensitive_hosts_but_case_rename_is_valid() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("source");
        let repo = init_repo(&root, &[("A.md", "upper\n")]);
        let case_sensitive = !root.join("a.md").exists();
        let base = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap();
        let mut builder = repo.treebuilder(Some(&base)).unwrap();
        builder
            .insert("a.md", repo.blob(b"lower\n").unwrap(), 0o100644)
            .unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        assert!(super::validate_portable_tree(&tree).is_err());
        if case_sensitive {
            write_file(&root, "a.md", "lower\n");
            commit_all(&repo, "colliding physical files");
            let error = super::validate_portable_worktree(&root).unwrap_err();
            assert!(error.to_string().contains("case-insensitive filesystem"));
        }
        drop(tree);

        let rename = temp.path().join("rename");
        let repo = init_repo(&rename, &[("A.md", "value\n")]);
        std::fs::rename(rename.join("A.md"), rename.join("a.md")).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("A.md")).unwrap();
        index.add_path(Path::new("a.md")).unwrap();
        index.write().unwrap();
        super::validate_portable_worktree(&rename).unwrap();

        // This physical two-file state cannot exist on a case-insensitive
        // volume. Its Git-tree equivalent is still tested above on every host.
        if case_sensitive {
            let collision = temp.path().join("unstaged-rename-collision");
            init_repo(&collision, &[("A.md", "upper\n"), ("B.md", "other\n")]);
            std::fs::rename(collision.join("B.md"), collision.join("a.md")).unwrap();
            let error = super::validate_portable_worktree(&collision).unwrap_err();
            assert!(
                error.to_string().contains("case-insensitive filesystem"),
                "{error}"
            );
        }
    }

    #[test]
    fn workspace_clone_validates_the_selected_tree_without_rejecting_a_safe_pin() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let repo = init_repo(&source, &[("A.md", "safe\n")]);
        let safe = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let base = parent.tree().unwrap();
        let mut builder = repo.treebuilder(Some(&base)).unwrap();
        builder
            .insert("a.md", repo.blob(b"collision\n").unwrap(), 0o100644)
            .unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("fixture", "fixture@example.test").unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "colliding default head",
            &tree,
            &[&parent],
        )
        .unwrap();

        let unpinned = temp.path().join("workspace/unpinned");
        let error = super::clone_missing_repo(file_url(&source).as_str(), &unpinned).unwrap_err();
        assert!(error.to_string().contains("case-insensitive filesystem"));
        assert!(!unpinned.exists());

        let pinned = temp.path().join("workspace/pinned");
        super::clone_missing_repo_at_commit(file_url(&source).as_str(), &pinned, &safe).unwrap();
        let checked_out = Repository::open(&pinned).unwrap();
        assert_eq!(
            checked_out
                .head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .id()
                .to_string(),
            safe
        );
        assert!(pinned.join("A.md").is_file());
        let names: Vec<_> = std::fs::read_dir(&pinned)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(names.iter().any(|name| name == "A.md"));
        assert!(!names.iter().any(|name| name == "a.md"));
        assert_eq!(
            std::fs::read_to_string(pinned.join("A.md"))
                .unwrap()
                .replace("\r\n", "\n"),
            "safe\n"
        );
    }

    #[test]
    fn failed_initial_pin_never_becomes_success_on_retry() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        init_repo(&source, &[("theme.toml", "name='theme'")]);
        let destination = temp.path().join("workspace/theme");
        for _ in 0..2 {
            assert!(super::clone_missing_repo_at_commit(
                file_url(&source).as_str(),
                &destination,
                "ffffffffffffffffffffffffffffffffffffffff"
            )
            .is_err());
            assert!(!destination.exists());
        }
        assert_eq!(
            std::fs::read_dir(destination.parent().unwrap())
                .unwrap()
                .count(),
            1,
            "only the persistent initializer lock should remain"
        );
    }

    #[test]
    fn complete_pin_is_published_and_existing_dirty_theme_is_preserved() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let repo = init_repo(&source, &[("theme.toml", "old")]);
        let pin = repo.head().unwrap().target().unwrap().to_string();
        write_file(&source, "theme.toml", "new");
        commit_all(&repo, "new");
        let destination = temp.path().join("workspace/theme");
        assert_eq!(
            super::clone_missing_repo_at_commit(file_url(&source).as_str(), &destination, &pin)
                .unwrap(),
            super::CloneOutcome::Fresh
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("theme.toml")).unwrap(),
            "old"
        );
        write_file(&destination, "theme.toml", "developer edits");
        assert_eq!(
            super::clone_missing_repo_at_commit(
                file_url(&source).as_str(),
                &destination,
                "bad-pin"
            )
            .unwrap(),
            super::CloneOutcome::Existing
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("theme.toml")).unwrap(),
            "developer edits"
        );
    }

    #[test]
    fn initialization_does_not_replace_a_preexisting_plain_directory() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        init_repo(&source, &[("theme.toml", "theme")]);
        let destination = temp.path().join("theme");
        std::fs::create_dir(&destination).unwrap();
        write_file(&destination, "keep", "sentinel");
        assert!(super::clone_missing_repo_at_commit(
            file_url(&source).as_str(),
            &destination,
            "HEAD"
        )
        .is_err());
        assert_eq!(
            std::fs::read_to_string(destination.join("keep")).unwrap(),
            "sentinel"
        );
    }
}

#[cfg(test)]
mod source_path_diagnostic_tests {
    use super::*;
    #[test]
    fn source_length_diagnostic_preserves_the_git_cause_and_names_the_workaround() {
        let error =
            source_status_error(git2::Error::from_str("path too long: source/proposal.pdf"));
        if cfg!(windows) {
            assert!(error
                .to_string()
                .contains("move the source checkout to a shorter directory"));
            assert!(error.to_string().contains("--build-root"));
        } else {
            assert_eq!(error.to_string(), "unable to get root repository status");
        }
        assert!(std::error::Error::source(&error)
            .unwrap()
            .to_string()
            .contains("source/proposal.pdf"));
        let unrelated = source_status_error(git2::Error::from_str("fixture status error"));
        assert_eq!(
            unrelated.to_string(),
            "unable to get root repository status"
        );
    }
}
