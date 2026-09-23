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
    proposal_catalog::{self, ProposalCatalog},
    proposal_metadata,
    proposal::{OnlyRenderPlan, ProposalNumber},
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
    last_good_commit: Oid,
    last_good_tree: Oid,
    publication_uncertain: bool,
    render_pending: bool,
    configuration: crate::execution::ConfigurationSnapshot,
    repository_title: String,
    accepted_catalog: ProposalCatalog,
    accepted_index: NetworkUpgradeIndex,
    accepted_projections: BTreeMap<PathBuf, Oid>,
    generated_in_stage: bool,
    pending_publication: BTreeSet<PathBuf>,
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

// These two files are generated only after raw admission succeeds. They are
// removed from PRIVATE staging before the next raw generation is admitted.
// This is a finite generator list, not authority to delete authored output.
const DERIVED_PATHS: &[&str] = &[
    "static/assets/data/proposals.json",
    "content/hardforks/_index.md",
];

fn raw_changes(
    repo: &Repository,
    previous: &git2::Tree<'_>,
    candidate: &git2::Tree<'_>,
) -> Result<BTreeSet<PathBuf>, Whatever> {
    let diff = repo.diff_tree_to_tree(Some(previous), Some(candidate), None)
        .whatever_context("unable to compare accepted raw generation")?;
    Ok(diff.deltas().flat_map(|delta| {
        [delta.old_file().path().map(Path::to_path_buf),
         delta.new_file().path().map(Path::to_path_buf)]
    }).flatten().collect())
}

fn projection_hash(path: &Path) -> Result<Oid, Whatever> {
    let bytes = std::fs::read(path).whatever_context("unable to read prepared proposal projection")?;
    Oid::hash_object(ObjectType::Blob, &bytes)
        .whatever_context("unable to compare prepared proposal projection")
}

fn proposal_paths(catalog: &ProposalCatalog, stage: &Path, plan: Option<&OnlyRenderPlan>)
    -> BTreeSet<PathBuf>
{
    catalog.source_documents().map(|document| {
        document.source_path().strip_prefix(stage).expect("catalog belongs to raw staging").to_path_buf()
    }).filter(|path| plan.is_none_or(|plan| plan.should_sync_dirty_path(path))).collect()
}

impl LiveSource {
    pub(crate) fn new(
        source: &Path,
        live: &Path,
        plan: Option<&OnlyRenderPlan>,
        generation: git::SourceGeneration,
        configuration: crate::execution::ConfigurationSnapshot,
        repository_title: &str,
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
        // The clone is the exact admitted raw generation used by preparation,
        // not the current source after a startup gap. Install ALL accepted
        // semantics/projections before forced reconciliation observes edits.
        let accepted_catalog = proposal_catalog::collect_proposal_catalog(&stage.join(CONTENT_DIR), plan)
            .whatever_context("unable to initialize accepted proposal catalog")?;
        let accepted_index = network_upgrades::collect_network_upgrades(&accepted_catalog)
            .whatever_context("unable to initialize accepted network upgrades")?;
        let accepted_projections = proposal_paths(&accepted_catalog, &stage, plan).into_iter()
            .map(|path| projection_hash(&live.join(&path)).map(|hash| (path, hash)))
            .collect::<Result<_, _>>()?;
        let mut state = Self {
            _scratch: scratch,
            source: source.into(),
            live: live.into(),
            stage,
            selection: plan.map(OnlyRenderPlan::selected_numbers),
            last_good_commit: initial_merged,
            last_good_tree: repo
                .find_commit(initial_merged)
                .and_then(|c| c.tree())
                .whatever_context("unable to identify initial validated input")?
                .id(),
            publication_uncertain: false,
            render_pending: false,
            configuration,
            repository_title: repository_title.to_owned(),
            accepted_catalog,
            accepted_index,
            accepted_projections,
            generated_in_stage: false,
            pending_publication: BTreeSet::new(),
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
        self.pending.extend(affected.iter().cloned());
        let files_before = file_stamps(&self.source, &affected)?;
        let repo = git::open_generated_repository(&self.stage)
            .whatever_context("unable to open serve staging repository")?;
        if self.generated_in_stage {
            for relative in DERIVED_PATHS {
                let path = self.stage.join(relative);
                crate::generated_fs::check_path(&self.stage, &path, false)
                    .whatever_context("unsafe private derived path")?;
                match std::fs::remove_file(path) {
                    Ok(()) => {},
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                    Err(error) => return Err(error).whatever_context("unable to clear private derived input"),
                }
            }
            crate::reserved::clear_empty_staged_reservations(&self.stage)?;
            self.generated_in_stage = false;
        }
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
        crate::reserved::clear_empty_staged_reservations(&self.stage)?;
        crate::reserved::validate_inputs(&self.stage)
            .whatever_context("candidate occupies reserved generated paths; last-good input retained")?;
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
        let previous_tree = repo.find_tree(self.last_good_tree)
            .whatever_context("unable to inspect accepted raw input")?;
        let changed = raw_changes(&repo, &previous_tree, &tree)?;
        let source_parent = repo
            .find_commit(stamp.0)
            .whatever_context("source HEAD moved during preparation; retry")?;
        let merged_parent = repo
            .find_commit(self.last_good_commit)
            .whatever_context("unable to read accepted raw source")?;
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
        // Every candidate derives the complete catalog and aggregate model.
        // Accepted state is used for comparison, never incremental mutation.
        let candidate_catalog = proposal_catalog::collect_proposal_catalog(&self.stage.join(CONTENT_DIR), plan.as_ref())
            .whatever_context("candidate proposal semantics rejected; last-good input retained")?;
        let candidate_index = network_upgrades::collect_network_upgrades(&candidate_catalog)
            .whatever_context("candidate network upgrades rejected; last-good input retained")?;
        let mut global_semantics_changed = candidate_catalog.records() != self.accepted_catalog.records()
            || candidate_index != self.accepted_index;
        let candidate_paths = proposal_paths(&candidate_catalog, &self.stage, plan.as_ref());
        let old_paths: BTreeSet<_> = self.accepted_projections.keys().cloned().collect();
        global_semantics_changed |= candidate_paths != old_paths;
        let mut preprocess = changed.clone();
        // A failed publication may have installed projections not represented by
        // the accepted state. Recompute those paths even when the source reverts.
        preprocess.extend(self.pending_publication.iter().cloned());
        for document in candidate_catalog.source_documents() {
            let number = document.number();
            if candidate_index.memberships_by_proposal.get(&number)
                != self.accepted_index.memberships_by_proposal.get(&number)
            {
                preprocess.insert(document.source_path().strip_prefix(&self.stage).unwrap().to_path_buf());
            }
        }
        // Inventory, destination identity, or asset route changes can alter links
        // in otherwise unchanged retained Markdown. Reconsider those projections,
        // but byte-diff them: this MUST NOT republish the entire proposal tree.
        let inventory_changed = {
            let diff = repo.diff_tree_to_tree(Some(&previous_tree), Some(&tree), None)
                .whatever_context("unable to compare raw inventory")?;
            diff.deltas().any(|delta| delta.status() != git2::Delta::Modified)
        };
        let structural_content_change = inventory_changed && changed.iter().any(|path| path.starts_with(CONTENT_DIR));
        let destinations_changed = candidate_catalog.records().iter().any(|(key, record)| {
            self.accepted_catalog.records().get(key).is_none_or(|old| old.destination != record.destination)
        }) || candidate_catalog.records().len() != self.accepted_catalog.records().len();
        let all_proposal_paths = proposal_paths(&candidate_catalog, &self.stage, None);
        let old_proposal_paths = proposal_paths(&self.accepted_catalog, &self.stage, None);
        let native_route_changed = changed.iter().any(|path| {
            path.starts_with(CONTENT_DIR) && path.extension().is_some_and(|x| x == "md")
                && !all_proposal_paths.contains(path) && !old_proposal_paths.contains(path)
        });
        if head_changed || inventory_changed || destinations_changed || native_route_changed {
            preprocess.extend(git::tracked_paths(&self.stage, |path| {
                path.starts_with(CONTENT_DIR) && path.extension().is_some_and(|x| x == "md")
            }).whatever_context("unable to enumerate retained Markdown")?);
        }
        let retained = |path: &PathBuf| plan.as_ref().is_none_or(|plan| {
            plan.should_sync_dirty_path(path) || old_paths.contains(path)
        });
        preprocess.retain(retained);
        // Generated hardfork Markdown is NOT source Markdown to preprocess.
        for relative in DERIVED_PATHS { preprocess.remove(Path::new(relative)); }
        markdown::preprocess_paths_with_network_upgrades(
            &self.stage.join(CONTENT_DIR), &preprocess, plan.as_ref(), Some(&candidate_index),
        ).whatever_context("unable to preprocess candidate generation; last-good input retained")?;
        let candidate_projections = candidate_paths.iter().map(|path| {
            if preprocess.contains(path) || !self.accepted_projections.contains_key(path) {
                projection_hash(&self.stage.join(path)).map(|hash| (path.clone(), hash))
            } else {
                Ok((path.clone(), self.accepted_projections[path]))
            }
        }).collect::<Result<BTreeMap<_, _>, Whatever>>()?;
        let mut publish: BTreeSet<_> = changed.into_iter().filter(retained).collect();
        publish.extend(self.pending_publication.iter().cloned());
        publish.extend(preprocess.iter().filter(|path| !candidate_paths.contains(*path)).cloned());
        for path in old_paths.union(&candidate_paths) {
            if self.accepted_projections.get(path) != candidate_projections.get(path) {
                publish.insert(path.clone());
            }
        }
        self.generated_in_stage = true;
        proposal_metadata::write_proposal_metadata_json_from_catalog(&self.stage, &self.repository_title, &candidate_catalog)
            .whatever_context("unable to derive candidate proposal metadata; last-good input retained")?;
        network_upgrades::write_hardforks_index(&self.stage.join(CONTENT_DIR), &candidate_index)
            .whatever_context("unable to derive candidate hardfork index; last-good input retained")?;
        publish.extend(DERIVED_PATHS.iter().map(PathBuf::from));
        if source_stamp(&self.source)? != stamp
            || file_stamps(&self.source, &affected)? != files_before
        {
            snafu::whatever!("active source changed during preprocessing; last-good input retained until the next update");
        }
        self.configuration.verify()?;
        crate::source_reads::validate_tree(&self.stage).whatever_context(
            "transformed source-read policy rejected; last-good input retained",
        )?;
        crate::reserved::validate_zola_facing(&self.stage).whatever_context(
            "transformed routes rejected; last-good input retained",
        )?;
        // This boundary is intentionally after all parsing and transformation.
        // Publication I/O can still fail midway; keep pending paths for retry.
        self.pending_publication.extend(publish.iter().cloned());
        self.publication_uncertain = true;
        let changed_publication = crate::publication::changed_paths(&self.stage, &self.live, &publish)
            .whatever_context("unable to compare publication; retry pending")?;
        let output = crate::layout::serve_output_path(self.live.parent().unwrap());
        let static_refresh = crate::zola::prepare_static_output(&self.stage, &output, &changed_publication)
            .whatever_context("unable to invalidate stale static output; retry pending")?;
        // Zola already performs a full render when a non-Markdown content
        // asset is written. An asset-only add/rename needs no second trigger.
        // Deletions can be ignored by its watcher, so retain explicit escalation
        // when the structural update has no such native content write.
        let native_asset_write = changed_publication.iter().any(|path| {
            path.starts_with(CONTENT_DIR)
                && path.extension().is_none_or(|extension| extension != "md")
                && self.stage.join(path).is_file()
        });
        self.render_pending |= global_semantics_changed || static_refresh
            || (structural_content_change && !native_asset_write);
        let published = crate::publication::publish(&self.stage, &self.live, &changed_publication)
            .whatever_context("unable to publish validated generation; update may be partial, retry pending")?;
        if self.render_pending {
            crate::zola::request_full_rebuild(&self.live)
                .whatever_context("unable to request coalesced Zola render; retry pending")?;
            self.render_pending = false;
        }
        self.accepted_catalog = candidate_catalog;
        self.accepted_index = candidate_index;
        self.accepted_projections = candidate_projections;
        self.last_good_commit = id;
        self.pending_publication.clear();
        self.previous_head = stamp.0;
        self.previous_paths = current;
        self.previous_dirty = dirty;
        self.last_good_tree = tree_id;
        self.publication_uncertain = false;
        self.pending.clear();
        log::info!(
            "published validated serve generation: {} path(s)",
            published
        );
        Ok(published)
    }
}


#[cfg(test)]
pub(crate) mod fixture {
    use super::*;

    /// Minimal complete WG semantics for lifecycle unit fixtures. No production
    /// feature flag, empty-registry mode or relaxed proposal parser is involved.
    pub(crate) fn complete_sources(root: &Path) {
        use network_upgrades::NetworkUpgradeParserMode;
        let content = root.join(CONTENT_DIR);
        std::fs::create_dir_all(&content).unwrap();
        let write_missing = |number: u32, kind: &str, body: &str| {
            let path = content.join(format!("{number:05}.md"));
            if !path.exists() {
                std::fs::write(path, format!("---\neip: {number}\ntitle: Fixture {number}\nstatus: Draft\ntype: {kind}\ncreated: 2020-01-01\n---\n\n{body}\n")).unwrap();
            }
        };
        write_missing(90099, "Standards Track", "Historical fixture member.");
        for entry in network_upgrades::transitional_modern_registry().into_iter().chain(network_upgrades::permanent_registry()) {
            let body = match entry.parser_mode {
                NetworkUpgradeParserMode::LegacyIncludedList => "### Included EIPs\n\n* [EIP-90099](./90099.md)\n",
                NetworkUpgradeParserMode::ModernStages => "### Included EIPs\n",
                _ => "",
            };
            write_missing(entry.source_meta_eip, "Meta", body);
            if let NetworkUpgradeParserMode::ExplicitIncludedMembers(members) = entry.parser_mode {
                for number in members { write_missing(*number, "Standards Track", "Curated fixture member."); }
            }
        }
    }

    pub(crate) fn prepare(root: &Path, plan: Option<&OnlyRenderPlan>) {
        let catalog = proposal_catalog::collect_proposal_catalog(&root.join(CONTENT_DIR), plan).unwrap();
        let index = network_upgrades::collect_network_upgrades(&catalog).unwrap();
        proposal_metadata::write_proposal_metadata_json_from_catalog(root, "EIPs", &catalog).unwrap();
        markdown::preprocess_with_network_upgrades(&root.join(CONTENT_DIR), plan, Some(&index)).unwrap();
        network_upgrades::write_hardforks_index(&root.join(CONTENT_DIR), &index).unwrap();
        if let Some(plan) = plan { plan.prune_content(&root.join(CONTENT_DIR)).unwrap(); }
    }
}

#[cfg(test)]
#[path = "live_semantics_tests.rs"]
mod semantic_tests;
