/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Active-source generations for serve. The staging repository retains the full
//! raw merged input in its index/HEAD, including omitted proposals. Preprocessing
//! happens outside Zola's watched directory; only successful updates are copied
//! into the live tree. This is not an atomic multi-file Zola publication protocol.

use crate::{
    generation::{file_stamps, source_stamp, Snapshot},
    git,
    layout::CONTENT_DIR,
    markdown,
    network_upgrades::{self, NetworkUpgradeIndex},
    proposal::{proposal_number_from_content_markdown_path, OnlyRenderPlan, ProposalNumber},
};
use git2::{
    build::CheckoutBuilder, ObjectType, Oid, Repository, Signature, TreeWalkMode, TreeWalkResult,
};
use snafu::{OptionExt, ResultExt, Whatever};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

pub(crate) struct LiveSource {
    _scratch: tempfile::TempDir,
    source: PathBuf,
    live: PathBuf,
    stage: PathBuf,
    selection: Option<BTreeSet<ProposalNumber>>,
    initial_merged: Oid,
    last_good_tree: Oid,
    publication_uncertain: bool,
    configuration: crate::execution::ConfigurationSnapshot,
    network_upgrade_index: Option<NetworkUpgradeIndex>,
    sibling_files: BTreeMap<PathBuf, (Oid, i32)>,
    previous_head: Oid,
    previous_paths: BTreeSet<PathBuf>,
    previous_dirty: BTreeSet<PathBuf>,
    pending: BTreeSet<PathBuf>,
}

fn tree_files(repo: &Repository, commit: Oid) -> Result<BTreeMap<PathBuf, (Oid, i32)>, Whatever> {
    let tree = repo
        .find_commit(commit)
        .and_then(|c| c.tree())
        .whatever_context("unable to read source tree")?;
    let mut paths = BTreeMap::new();
    tree.walk(TreeWalkMode::PreOrder, |prefix, entry| {
        if entry.kind() == Some(ObjectType::Blob) {
            if let Some(name) = entry.name() {
                paths.insert(
                    PathBuf::from(format!("{prefix}{name}")),
                    (entry.id(), entry.filemode()),
                );
            }
        }
        TreeWalkResult::Ok
    })
    .whatever_context("unable to inventory source tree")?;
    Ok(paths)
}

fn is_hardfork_meta_eip_path(source_root: &Path, repo_relative_path: &Path) -> bool {
    let Ok(content_relative_path) = repo_relative_path.strip_prefix(CONTENT_DIR) else {
        return false;
    };
    let Some(proposal_number) = proposal_number_from_content_markdown_path(content_relative_path)
    else {
        return false;
    };

    if network_upgrades::is_registered_network_upgrade_source_number(proposal_number) {
        return true;
    }

    std::fs::read_to_string(source_root.join(repo_relative_path))
        .map(|contents| network_upgrades::markdown_has_network_upgrade_marker(&contents))
        .unwrap_or(false)
}

fn warn_for_changed_hardfork_meta_eips(source_root: &Path, affected_paths: &BTreeSet<PathBuf>) {
    for path in affected_paths
        .iter()
        .filter(|path| is_hardfork_meta_eip_path(source_root, path))
    {
        log::warn!(
            "edit detected in hardfork Meta EIP `{}`; restart `build-eips serve` to refresh /hardforks/ and member EIP rows",
            path.to_string_lossy()
        );
    }
}

impl LiveSource {
    pub(crate) fn new(
        source: &Path,
        live: &Path,
        plan: Option<&OnlyRenderPlan>,
        generation: git::SourceGeneration,
        configuration: crate::execution::ConfigurationSnapshot,
    ) -> Result<Self, Whatever> {
        let scratch = tempfile::Builder::new()
            .prefix(".serve-generation-")
            .tempdir_in(
                live.parent()
                    .whatever_context("live repository has no build root")?,
            )
            .whatever_context("unable to create private live staging directory")?;
        let stage = scratch.path().join("repo");
        let repo = Repository::clone(
            live.to_str()
                .whatever_context("non-UTF-8 live repository path")?,
            &stage,
        )
        .whatever_context("unable to clone prepared raw input for serving")?;
        crate::generated_git::validate(&repo, &stage)
            .whatever_context("unsafe generated live repository")?;
        let initial_merged =
            git::head_commit_id(&repo).whatever_context("unable to read prepared HEAD")?;
        let active_files = tree_files(&repo, generation.materialized_head)?;
        // Prepared merge commits record each sibling as their second parent.
        // Preserve that exact sibling snapshot for conflict checks during serve.
        let mut sibling_files = BTreeMap::new();
        let mut cursor = initial_merged;
        while cursor != generation.materialized_head {
            let commit = repo
                .find_commit(cursor)
                .whatever_context("unable to inspect prepared merge ancestry")?;
            if commit.parent_count() != 2 {
                snafu::whatever!(
                    "prepared input does not descend from its recorded active generation"
                );
            }
            for (path, file) in tree_files(&repo, commit.parent_id(1).unwrap())? {
                if path.starts_with("content") && path != Path::new("content/_index.md") {
                    sibling_files.insert(path, file);
                }
            }
            cursor = commit.parent_id(0).unwrap();
        }
        let mut state = Self {
            _scratch: scratch,
            source: source.into(),
            live: live.into(),
            stage,
            selection: plan.map(OnlyRenderPlan::selected_numbers),
            initial_merged,
            last_good_tree: repo
                .find_commit(initial_merged)
                .and_then(|c| c.tree())
                .whatever_context("unable to identify initial validated input")?
                .id(),
            publication_uncertain: false,
            configuration,
            network_upgrade_index: None,
            sibling_files,
            previous_head: generation.source_head,
            previous_paths: active_files
                .into_keys()
                .filter(|p| !p.starts_with("build"))
                .collect(),
            previous_dirty: BTreeSet::new(),
            pending: BTreeSet::new(),
        };
        drop(repo);
        // Watchers are installed before this call. Reconcile with the recorded
        // prepared generation rather than merely sampling status after a gap.
        state.synchronize_inner(true)?;
        Ok(state)
    }

    pub(crate) fn set_network_upgrade_index(&mut self, index: Option<NetworkUpgradeIndex>) {
        self.network_upgrade_index = index;
    }

    pub(crate) fn needs_publication_retry(&self) -> bool {
        self.publication_uncertain
    }

    pub(crate) fn synchronize(&mut self) -> Result<usize, Whatever> {
        self.synchronize_inner(false)
    }

    fn synchronize_inner(&mut self, force: bool) -> Result<usize, Whatever> {
        self.configuration.verify()?;
        let Snapshot {
            stamp,
            paths: current,
            dirty,
        } = Snapshot::capture(&self.source, true)?;
        let head_changed = stamp.0 != self.previous_head;
        let inventory_changed = current != self.previous_paths;
        let mut affected: BTreeSet<_> = self.previous_dirty.union(&dirty).cloned().collect();
        affected.extend(self.previous_paths.symmetric_difference(&current).cloned());
        affected.extend(self.pending.iter().cloned());
        if force || head_changed {
            affected.extend(self.previous_paths.iter().cloned());
            affected.extend(current.iter().cloned());
        }
        if affected.is_empty() {
            return Ok(0);
        }
        warn_for_changed_hardfork_meta_eips(&self.source, &affected);
        self.pending.extend(affected.iter().cloned());
        let files_before = file_stamps(&self.source, &affected)?;
        let repo = git::open_generated_repository(&self.stage)
            .whatever_context("unable to open serve staging repository")?;
        if repo.find_commit(stamp.0).is_err() {
            let url = url::Url::from_directory_path(&self.source)
                .map_err(|_| ())
                .ok()
                .whatever_context("unable to form local source URL")?;
            repo.remote_anonymous(url.as_str())
                .and_then(|mut remote| {
                    remote.fetch(&["+HEAD:refs/build-eips/live-source"], None, None)
                })
                .whatever_context("unable to import changed local HEAD for serving")?;
        }
        // Discard previous transformations, not the raw staged input. No full
        // re-clone or remote-sibling fetch is needed for an ordinary edit.
        let transformed = git::working_tree_paths(&self.stage)
            .whatever_context("unable to inspect staged transformations")?;
        if !transformed.is_empty() {
            let mut checkout = CheckoutBuilder::new();
            checkout.force().remove_untracked(true);
            for path in &transformed {
                checkout.path(path);
            }
            repo.checkout_index(None, Some(&mut checkout))
                .whatever_context("unable to restore raw staging input")?;
        }
        // Admission must use the same tracked attribute generation as normal
        // materialization. Apply it privately first, then reopen to avoid cached
        // attributes from a preceding accepted/rejected generation. An attribute
        // link must not become an indirect read outside the admitted source.
        let attribute_paths: BTreeSet<_> = affected
            .iter()
            .filter(|p| p.file_name().is_some_and(|name| name == ".gitattributes"))
            .cloned()
            .collect();
        let repo = if attribute_paths.is_empty() {
            repo
        } else {
            git::validate_attribute_inputs(&self.source, &attribute_paths).whatever_context(
                "unsafe tracked attribute configuration; last-good input retained",
            )?;
            let removed_attributes = attribute_paths.difference(&current).cloned().collect();
            let eligible_attributes = attribute_paths.intersection(&current).cloned().collect();
            git::sync_materialized_paths(
                &self._scratch.path().join("absent-source"),
                &self.stage,
                &removed_attributes,
            )
            .whatever_context("unable to remove obsolete source attributes")?;
            git::sync_materialized_paths(&self.source, &self.stage, &eligible_attributes)
                .whatever_context("unable to stage current source attributes")?;
            git::open_generated_repository(&self.stage)
                .whatever_context("unable to reopen current attribute context")?
        };
        let mut to_copy = affected.clone();
        for path in &affected {
            if let Some((expected_id, expected_mode)) = self.sibling_files.get(path) {
                if let Some(actual) = git::working_entry_identity(&repo, &self.source, path)
                    .whatever_context("unable to inspect source/sibling overlap")?
                {
                    git::check_entry_conflict(path, actual, git::EntryIdentity {
                        id: *expected_id, mode: *expected_mode, kind: Some(ObjectType::Blob),
                    }).whatever_context("active change conflicts with startup sibling snapshot; last-good input retained")?;
                }
                // An identical active copy, or its deletion, cannot remove the
                // authoritative sibling contribution from the merged snapshot.
                to_copy.remove(path);
            } else {
                // A newly introduced active descendant/ancestor must not replace
                // a sibling's file with a directory (or erase its directory).
                let overlapping = path
                    .ancestors()
                    .skip(1)
                    .any(|p| self.sibling_files.contains_key(p))
                    || self
                        .sibling_files
                        .range(path.clone()..)
                        .next()
                        .is_some_and(|(p, _)| p.starts_with(path));
                if overlapping {
                    if let Some(actual) = git::working_entry_identity(&repo, &self.source, path)
                        .whatever_context("unable to inspect source/sibling tree-shape overlap")?
                    {
                        if actual.kind == Some(ObjectType::Blob) {
                            git::check_entry_conflict(path, actual, git::EntryIdentity {
                                id: Oid::zero(), mode: 0o040000, kind: Some(ObjectType::Tree),
                            }).whatever_context("active change conflicts with startup sibling snapshot; last-good input retained")?;
                        }
                    }
                }
            }
        }
        // A formerly staged addition can become untracked again while its
        // physical file remains. It must disappear from the prepared input.
        let removed: BTreeSet<_> = to_copy.difference(&current).cloned().collect();
        let eligible: BTreeSet<_> = to_copy.intersection(&current).cloned().collect();
        // The private absent source provides deletion inputs to the same guarded
        // materializer; it is never populated or treated as a repository.
        git::sync_materialized_paths(
            &self._scratch.path().join("absent-source"),
            &self.stage,
            &removed,
        )
        .whatever_context("unable to remove paths that left the tracked source generation")?;
        git::sync_materialized_paths(&self.source, &self.stage, &eligible)
            .whatever_context("unable to stage current tracked source changes")?;
        crate::source_reads::validate_tree(&self.stage)
            .whatever_context("candidate source-read policy rejected; last-good input retained")?;
        let mut index = repo
            .index()
            .whatever_context("unable to open staging index")?;
        index
            .read(true)
            .whatever_context("unable to refresh staged raw index")?;
        let tree_id = index
            .write_tree()
            .whatever_context("unable to retain raw generation tree")?;
        let tree = repo
            .find_tree(tree_id)
            .whatever_context("unable to read raw generation tree")?;
        git::check_ignored(&repo, &tree).whatever_context(
            "candidate contains ignored tracked paths; last-good input retained",
        )?;
        // Compare with the last SUCCESSFUL publication, never a raw candidate
        // whose preprocessing or publication may have failed on a previous try.
        if self.last_good_tree == tree_id && !head_changed && !self.publication_uncertain {
            if source_stamp(&self.source)? != stamp
                || file_stamps(&self.source, &affected)? != files_before
            {
                snafu::whatever!(
                    "active source changed during staging; retry after source settles"
                );
            }
            self.previous_head = stamp.0;
            self.previous_paths = current;
            self.previous_dirty = dirty;
            self.pending.clear();
            return Ok(0);
        }
        let source_parent = repo
            .find_commit(stamp.0)
            .whatever_context("source HEAD moved during preparation; retry")?;
        let merged_parent = repo
            .find_commit(self.initial_merged)
            .whatever_context("unable to read initial merged source")?;
        let parents = if source_parent.id() == merged_parent.id() {
            vec![&source_parent]
        } else {
            vec![&source_parent, &merged_parent]
        };
        let signature = Signature::now("build-eips serve", "build-eips@example.invalid")
            .whatever_context("unable to create staging signature")?;
        let id = repo
            .commit(
                None,
                &signature,
                &signature,
                "Serve raw source generation",
                &tree,
                &parents,
            )
            .whatever_context("unable to retain staged raw source generation")?;
        repo.set_head_detached(id)
            .whatever_context("unable to select raw source generation")?;
        let plan = self
            .selection
            .clone()
            .map(|selected| OnlyRenderPlan::build_live(&self.stage.join("content"), selected))
            .transpose()
            .whatever_context("unable to refresh targeted inventory; last-good input retained")?;
        let mut publish = to_copy;
        // An inventory/HEAD change may change links in otherwise untouched pages.
        // In targeted mode this reprocesses only the selected/retained Markdown.
        if plan.is_some() || force || head_changed || inventory_changed {
            for path in git::tracked_paths(&self.stage, |p| {
                p.starts_with("content") && p.extension().is_some_and(|x| x == "md")
            })
            .whatever_context("unable to enumerate retained Markdown")?
            {
                publish.insert(path);
            }
        }
        if let Some(plan) = &plan {
            publish.retain(|path| {
                if plan.should_sync_dirty_path(path) {
                    return true;
                }
                // Selected proposal deletion/rename must still remove its old path.
                let Ok(relative) = path.strip_prefix("content") else {
                    return true;
                };
                crate::proposal::proposal_number_from_content_markdown_path(relative)
                    .is_some_and(|n| self.selection.as_ref().unwrap().contains(&n))
            });
        }
        markdown::preprocess_paths_with_network_upgrades(
            &self.stage.join("content"),
            &publish,
            plan.as_ref(),
            self.network_upgrade_index.as_ref(),
        )
        .whatever_context("unable to preprocess candidate generation; last-good input retained")?;
        if source_stamp(&self.source)? != stamp
            || file_stamps(&self.source, &affected)? != files_before
        {
            snafu::whatever!("active source changed during preprocessing; last-good input retained until the next update");
        }
        self.configuration.verify()?;
        crate::source_reads::validate_tree(&self.stage).whatever_context(
            "transformed source-read policy rejected; last-good input retained",
        )?;
        // This boundary is intentionally after all parsing and transformation.
        // Publication I/O can still fail midway; keep pending paths for retry.
        self.publication_uncertain = true;
        let output = crate::layout::output_path(self.live.parent().unwrap());
        let refresh = crate::zola::prepare_static_output(&self.stage, &output, &publish)
            .whatever_context("unable to invalidate stale static output; retry pending")?;
        git::sync_working_tree_paths(&self.stage, &self.live, &publish).whatever_context(
            "unable to publish validated generation; update may be partial, retry pending",
        )?;
        if refresh {
            crate::zola::request_full_rebuild(&self.live).whatever_context(
                "unable to refresh Zola after a static tree transition; retry pending",
            )?;
        }
        self.previous_head = stamp.0;
        self.previous_paths = current;
        self.previous_dirty = dirty;
        self.last_good_tree = tree_id;
        self.publication_uncertain = false;
        self.pending.clear();
        log::info!(
            "published validated serve generation: {} path(s)",
            publish.len()
        );
        Ok(publish.len())
    }
}
