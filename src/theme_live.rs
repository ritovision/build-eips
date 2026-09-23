/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Theme generations use the same tracked membership and tree transition rules
//! as active content, without proposal inventory, Markdown or sibling merging.
use crate::{
    generation::{file_stamps, source_stamp, Snapshot},
    git,
    serve::LocalThemeServeSync,
};
use snafu::{OptionExt, ResultExt, Whatever};
use std::{collections::BTreeSet, path::PathBuf};

pub(crate) struct LiveTheme {
    scratch: tempfile::TempDir,
    stage: PathBuf,
    config: LocalThemeServeSync,
    configuration: crate::execution::ConfigurationSnapshot,
    previous: Option<Snapshot>,
    previous_paths: BTreeSet<PathBuf>,
    pending: BTreeSet<PathBuf>,
    publication_uncertain: bool,
    render_pending: bool,
}

impl LiveTheme {
    pub(crate) fn new(
        config: LocalThemeServeSync,
        configuration: crate::execution::ConfigurationSnapshot,
    ) -> Result<Self, Whatever> {
        crate::generated_fs::check_path(&config.owned_root, &config.mounted_theme_dir, false)
            .whatever_context("unsafe theme mount")?;
        let scratch = tempfile::Builder::new()
            .prefix(".serve-theme-generation-")
            .tempdir_in(
                config
                    .owned_root
                    .parent()
                    .whatever_context("theme mount has no build root")?,
            )
            .whatever_context("unable to create private theme generation")?;
        let stage = scratch.path().join("theme");
        std::fs::create_dir(&stage).whatever_context("unable to initialize theme staging")?;
        let mut previous_paths = BTreeSet::new();
        if config.mounted_theme_dir.exists() {
            for entry in walkdir::WalkDir::new(&config.mounted_theme_dir).follow_links(false) {
                let entry = entry.whatever_context("unable to inventory prepared theme")?;
                if !entry.file_type().is_dir() {
                    previous_paths.insert(
                        entry
                            .path()
                            .strip_prefix(&config.mounted_theme_dir)
                            .unwrap()
                            .to_path_buf(),
                    );
                }
            }
        }
        let mut state = Self {
            scratch,
            stage,
            config,
            configuration,
            previous: None,
            previous_paths,
            pending: BTreeSet::new(),
            publication_uncertain: false,
            render_pending: false,
        };
        state.synchronize()?;
        Ok(state)
    }

    pub(crate) fn needs_publication_retry(&self) -> bool {
        self.publication_uncertain
    }

    pub(crate) fn synchronize(&mut self) -> Result<usize, Whatever> {
        self.configuration.verify()?;
        let source = &self.config.theme_source_root;
        let snapshot = Snapshot::capture(source, false)?;
        let mut affected = self.pending.clone();
        affected.extend(snapshot.dirty.iter().cloned());
        affected.extend(
            self.previous_paths
                .symmetric_difference(&snapshot.paths)
                .cloned(),
        );
        if let Some(previous) = &self.previous {
            affected.extend(previous.dirty.iter().cloned());
        }
        if self
            .previous
            .as_ref()
            .is_none_or(|p| p.stamp.0 != snapshot.stamp.0)
        {
            affected.extend(self.previous_paths.iter().cloned());
            affected.extend(snapshot.paths.iter().cloned());
        }
        if affected.is_empty() {
            return Ok(0);
        }
        self.pending.extend(affected.iter().cloned());
        let before = file_stamps(source, &affected)?;
        let removed = affected.difference(&snapshot.paths).cloned().collect();
        let eligible = affected.intersection(&snapshot.paths).cloned().collect();
        git::sync_working_tree_paths(&self.scratch.path().join("absent"), &self.stage, &removed)
            .whatever_context("unable to remove obsolete theme generation paths")?;
        git::sync_working_tree_paths(source, &self.stage, &eligible)
            .whatever_context("unable to stage current theme generation")?;
        crate::source_reads::validate_tree(&self.stage).whatever_context(
            "candidate theme source-read policy rejected; last-good theme retained",
        )?;
        crate::reserved::clear_empty_staged_reservations(&self.stage)?;
        crate::reserved::validate_inputs(&self.stage)
            .whatever_context("candidate theme occupies generated paths; last-good theme retained")?;
        if source_stamp(source)? != snapshot.stamp || file_stamps(source, &affected)? != before {
            snafu::whatever!("theme changed during staging; last-good theme retained until retry");
        }
        crate::generated_fs::check_path(
            &self.config.owned_root,
            &self.config.mounted_theme_dir,
            false,
        )
        .whatever_context("unsafe theme mount")?;
        self.configuration.verify()?;
        self.publication_uncertain = true;
        let output = crate::layout::serve_output_path(self.config.owned_root.parent().unwrap());
        let changed = crate::publication::changed_paths(&self.stage, &self.config.mounted_theme_dir, &affected)
            .whatever_context("unable to compare theme publication; retry pending")?;
        let refresh = crate::zola::prepare_static_output(&self.stage, &output, &changed)
            .whatever_context("unable to invalidate theme static output; retry pending")?;
        self.render_pending |= refresh;
        let published = crate::publication::publish(&self.stage, &self.config.mounted_theme_dir, &changed)
            .whatever_context(
                "unable to publish theme generation; update may be partial, retry pending",
            )?;
        if self.render_pending {
            crate::zola::request_full_rebuild(&self.config.owned_root)
                .whatever_context("unable to refresh theme output; retry pending")?;
            self.render_pending = false;
        }
        self.previous_paths = snapshot.paths.clone();
        self.previous = Some(snapshot);
        self.pending.clear();
        self.publication_uncertain = false;
        log::info!("published theme generation: {} path(s)", published);
        Ok(published)
    }
}
