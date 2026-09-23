/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Invocation-local batch of the existing `git log -1 -- <path>` semantics.
//!
//! A broad name-only log is not equivalent at merges: default history
//! simplification follows the first parent with an equal entry for THAT path.
//! Resolve all requested paths together in child-before-parent order, stopping
//! each path at its first change. No state survives a preprocessing invocation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use git2::{ErrorCode, Oid, Repository, Tree};
use snafu::{OptionExt, ResultExt, Whatever};

pub(crate) struct History {
    paths: Vec<PathBuf>,
    timestamps: Option<BTreeMap<PathBuf, Option<i64>>>,
    direct: Option<bool>,
}

impl History {
    pub(crate) fn new(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let paths = paths.into_iter().collect::<Vec<_>>();
        // Measured whole-history startup is more expensive than small direct
        // queries. Keep live edits/targeted preparation on the exact Git query.
        // The original basename argument is a Git pathspec. Keep Git in charge
        // of pattern/magic matching and explicitly case-insensitive pathspecs;
        // the tree-diff batch compares literal repository paths.
        let special_pathspecs = std::env::var_os("GIT_ICASE_PATHSPECS").is_some()
            || paths.iter().any(|path| {
                let name = path.file_name().unwrap_or_default().as_encoded_bytes();
                name.first() == Some(&b':')
                    || name
                        .iter()
                        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b'\\'))
            });
        let direct = (paths.len() < 128 || special_pathspecs).then_some(true);
        Self {
            paths,
            timestamps: None,
            direct,
        }
    }

    pub(crate) fn timestamp(&mut self, path: &Path) -> Result<Option<i64>, Whatever> {
        // Section indexes, already-generated pages, and missing live paths
        // never ask for a timestamp, so they do not require a Git repository.
        if self.direct.is_none() {
            self.direct = Some(configured_history(path)?);
        }
        if self.direct == Some(true) {
            return direct_timestamp(path);
        }
        if self.timestamps.is_none() {
            self.timestamps = Some(collect(path, &self.paths)?);
        }
        self.timestamps
            .as_ref()
            .unwrap()
            .get(path)
            .copied()
            .with_whatever_context(|| {
                format!(
                    "path was not admitted to history batch: `{}`",
                    path.display()
                )
            })
    }
}

// These Git settings change the old query's semantics/output, not just its
// cost. Leave such requests to Git itself rather than reimplementing rename
// following or signature verification. This is checked once per large batch.
fn configured_history(path: &Path) -> Result<bool, Whatever> {
    let mut command = Command::new("git");
    command.current_dir(path.parent().unwrap()).args([
        "config",
        "--null",
        "--type=bool",
        "--get-regexp",
        "^log\\.(follow|showsignature)$",
    ]);
    let output = command
        .output()
        .with_whatever_context(|e| format!("failed to execute {command:?}: {e}"))?;
    if output.status.code() == Some(1) {
        return Ok(false);
    }
    if !output.status.success() {
        snafu::whatever!(
            "command {command:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .any(|record| record.ends_with(b"\ntrue")))
}

fn direct_timestamp(path: &Path) -> Result<Option<i64>, Whatever> {
    let mut command = Command::new("git");
    command
        .current_dir(path.parent().unwrap())
        .args(["log", "-1", "--pretty=format:%ct", "--"])
        .arg(path.file_name().unwrap());
    let output = command
        .output()
        .with_whatever_context(|e| format!("failed to execute {command:?}: {e}"))?;
    if !output.status.success() {
        snafu::whatever!(
            "command {command:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let text = std::str::from_utf8(&output.stdout)
        .whatever_context("history output is not UTF-8")?
        .trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse()
        .map(Some)
        .whatever_context("unable to parse proposal history timestamp")
}

struct Commit {
    id: Oid,
    tree: Oid,
    timestamp: i64,
    parents: Vec<Oid>,
}

fn entry(tree: &Tree<'_>, path: &Path) -> Result<Option<(Oid, i32)>, Whatever> {
    match tree.get_path(path) {
        Ok(entry) => Ok(Some((entry.id(), entry.filemode_raw()))),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error).with_whatever_context(|_| {
            format!("unable to read history entry `{}`", path.display())
        }),
    }
}

fn collect(first: &Path, paths: &[PathBuf]) -> Result<BTreeMap<PathBuf, Option<i64>>, Whatever> {
    let parent = first
        .parent()
        .whatever_context("proposal history path has no parent")?;
    let repo = Repository::discover(parent)
        .whatever_context("unable to open proposal history repository")?;
    let workdir = repo
        .workdir()
        .whatever_context("proposal history repository has no working directory")?;
    let cwd =
        std::env::current_dir().whatever_context("unable to resolve proposal history paths")?;
    let relative = paths
        .iter()
        .map(|path| {
            let absolute = if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(path)
            };
            absolute
                .strip_prefix(workdir)
                .map(Path::to_path_buf)
                .with_whatever_context(|_| {
                    format!(
                        "proposal history path is outside repository: `{}`",
                        path.display()
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Keep the complete effective topology. Even full-history path pruning
    // can remove an empty first parent of a merge, changing an absent path's
    // no-history result into a deletion timestamp. Git supplies shallow and
    // replacement parents/trees; per-path simplification happens only below.
    let mut command = Command::new("git");
    command.current_dir(workdir).args([
        "log",
        "--topo-order",
        "--no-show-signature",
        "--no-follow",
        "--format=%H %T %ct %P",
    ]);
    let output = command
        .output()
        .with_whatever_context(|e| format!("failed to execute {command:?}: {e}"))?;
    if !output.status.success() {
        snafu::whatever!(
            "command {command:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let text =
        std::str::from_utf8(&output.stdout).whatever_context("history output is not UTF-8")?;
    let commits = text
        .lines()
        .map(|line| {
            let mut fields = line.split_whitespace();
            let id = fields
                .next()
                .whatever_context("history commit ID missing")?
                .parse::<Oid>()
                .whatever_context("invalid history commit ID")?;
            let tree = fields
                .next()
                .whatever_context("history tree ID missing")?
                .parse::<Oid>()
                .whatever_context("invalid history tree ID")?;
            let timestamp = fields
                .next()
                .whatever_context("history timestamp missing")?
                .parse::<i64>()
                .whatever_context("invalid history timestamp")?;
            let parents = fields
                .map(|id| {
                    id.parse::<Oid>()
                        .whatever_context("invalid history parent ID")
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Commit {
                id,
                tree,
                timestamp,
                parents,
            })
        })
        .collect::<Result<Vec<_>, Whatever>>()?;
    let mut result: BTreeMap<_, _> = paths.iter().cloned().map(|path| (path, None)).collect();
    let Some(head) = commits.first().map(|commit| commit.id) else {
        return Ok(result);
    };
    let trees: HashMap<_, _> = commits
        .iter()
        .map(|commit| (commit.id, commit.tree))
        .collect();
    let mut pending = HashMap::from([(head, (0..paths.len()).collect::<Vec<_>>())]);
    for commit in &commits {
        let Some(indices) = pending.remove(&commit.id) else {
            continue;
        };
        let tree = repo
            .find_tree(commit.tree)
            .whatever_context("unable to read history tree")?;
        // Compare each commit/parent pair once, not once per proposal. Tree
        // diffs do not read blob bodies and include mode/add/delete changes.
        // Rename detection is deliberately not requested: the old command
        // queried the current pathname without --follow.
        let parents = commit
            .parents
            .iter()
            .map(|parent| {
                let tree_id = trees
                    .get(parent)
                    .whatever_context("history parent missing from traversal")?;
                let parent_tree = repo
                    .find_tree(*tree_id)
                    .whatever_context("unable to read history parent tree")?;
                let diff = repo
                    .diff_tree_to_tree(Some(&parent_tree), Some(&tree), None)
                    .whatever_context("unable to compare proposal history trees")?;
                let mut changed = HashSet::new();
                for delta in diff.deltas() {
                    for path in [delta.old_file().path(), delta.new_file().path()]
                        .into_iter()
                        .flatten()
                    {
                        changed.insert(path.to_path_buf());
                    }
                }
                Ok((*parent, changed))
            })
            .collect::<Result<Vec<_>, Whatever>>()?;
        for index in indices {
            if let Some((parent, _)) = parents
                .iter()
                .find(|(_, changed)| !changed.contains(&relative[index]))
            {
                pending.entry(*parent).or_insert_with(Vec::new).push(index);
            } else if !parents.is_empty() || entry(&tree, &relative[index])?.is_some() {
                result.insert(paths[index].clone(), Some(commit.timestamp));
            }
            // A path absent at a root has no history; the caller retains the
            // old per-file filesystem fallback at the original processing time.
        }
        if pending.is_empty() {
            break;
        }
    }
    if !pending.is_empty() {
        snafu::whatever!("history traversal ended before all proposal paths were resolved");
    }
    Ok(result)
}

#[cfg(test)]
mod tests;
