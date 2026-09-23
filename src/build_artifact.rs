/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Self-contained completed build output. A receipt travels with its HTML;
//! failed rendering/indexing never changes the reusable output. This is not a
//! versioned-site server or a crash-proof filesystem transaction.
use serde::{Deserialize, Serialize};
use snafu::{OptionExt, ResultExt, Whatever};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use url::Url;

pub(crate) const RECEIPT: &str = ".build-eips-build.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BuildInfo {
    pub version: u32,
    pub complete: bool,
    pub generation: String,
    pub base_url: String,
    pub mount_path: String,
    pub tool_version: String,
    pub tool_binary_git_blob: String,
    pub zola_version: String,
    pub product: String,
    pub source_head: String,
    pub materialized_head: String,
    pub merged_source_tree: String,
}

impl BuildInfo {
    pub(crate) fn new(
        base_url: &Url,
        product: &str,
        source: crate::git::SourceGeneration,
        repo: &Path,
    ) -> Result<Self, Whatever> {
        let executable =
            std::env::current_exe().whatever_context("unable to identify build tool")?;
        let binary = git2::Oid::hash_file(git2::ObjectType::Blob, &executable)
            .whatever_context("unable to fingerprint build tool")?;
        let raw = crate::git::open_generated_repository(repo)
            .whatever_context("unable to identify merged source")?;
        let tree = raw
            .head()
            .and_then(|h| h.peel_to_tree())
            .whatever_context("unable to identify merged source tree")?
            .id();
        Ok(Self {
            version: 1,
            complete: true,
            generation: format!(
                "{}-{}",
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
                std::process::id()
            ),
            base_url: base_url.to_string(),
            mount_path: crate::search::normalized_base_path(base_url),
            tool_version: env!("CARGO_PKG_VERSION").into(),
            tool_binary_git_blob: binary.to_string(),
            zola_version: crate::zola::probe_zola(std::ffi::OsStr::new("zola"))
                .whatever_context("unable to identify Zola")?
                .to_string(),
            product: product.into(),
            source_head: source.source_head.to_string(),
            materialized_head: source.materialized_head.to_string(),
            merged_source_tree: tree.to_string(),
        })
    }

    pub(crate) fn read(output: &Path) -> Result<Self, Whatever> {
        let receipt = output.join(RECEIPT);
        crate::generated_fs::check_path(output, &receipt, false)
            .whatever_context("unsafe completed-build receipt")?;
        let metadata = fs::metadata(&receipt)
            .whatever_context("missing completed-build receipt; run `build-eips build` first")?;
        if !metadata.is_file() || metadata.len() > 65536 {
            snafu::whatever!("invalid completed-build receipt; rebuild this profile");
        }
        let info: Self = serde_json::from_slice(
            &fs::read(&receipt).whatever_context("unable to read completed-build receipt")?,
        )
        .whatever_context("invalid completed-build receipt; rebuild this profile")?;
        let url =
            Url::parse(&info.base_url).whatever_context("invalid completed-build base URL")?;
        if info.version != 1
            || !info.complete
            || info.generation.is_empty()
            || !matches!(url.scheme(), "http" | "https")
            || info.mount_path != crate::search::normalized_base_path(&url)
        {
            snafu::whatever!(
                "incomplete or unsupported completed-build receipt; rebuild this profile"
            );
        }
        crate::generated_fs::check_path(output, &output.join("index.html"), false)
            .whatever_context("unsafe completed-build index")?;
        if !output.join("index.html").is_file() {
            snafu::whatever!("completed-build output is missing index.html; rebuild this profile");
        }
        Ok(info)
    }

    pub(crate) fn write(&self, output: &Path) -> Result<(), Whatever> {
        let path = output.join(RECEIPT);
        crate::generated_fs::check_path(output, &path, false)
            .whatever_context("unsafe completed-build receipt destination")?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .whatever_context(
                "completed-build receipt path is reserved; refusing to overwrite rendered content",
            )?;
        serde_json::to_writer_pretty(&mut file, self)
            .whatever_context("unable to serialize completed-build receipt")?;
        file.write_all(b"\n")
            .whatever_context("unable to finish completed-build receipt")?;
        file.sync_all()
            .whatever_context("unable to flush completed-build receipt")?;
        Ok(())
    }
}

/// Keep the ordinary output path stable for consumers. The record is inside the
/// directory so promotion cannot pair old metadata with a new HTML tree. A crash
/// in the two-rename window leaves no current output: preview clearly refuses it.
pub(crate) fn publish(candidate: &Path, output: &Path) -> Result<(), Whatever> {
    BuildInfo::read(candidate)?;
    let parent = output
        .parent()
        .whatever_context("completed output has no parent")?;
    crate::generated_fs::check_path(parent, output, false)
        .whatever_context("unsafe completed output")?;
    let backup = tempfile::Builder::new()
        .prefix(".previous-build-")
        .tempdir_in(parent)
        .whatever_context("unable to reserve previous-build backup")?;
    let previous = backup.path().join("output");
    let had_output = match fs::symlink_metadata(output) {
        Ok(m) if m.is_dir() => true,
        Ok(_) => snafu::whatever!("completed output is not a directory; refusing replacement"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).whatever_context("unable to inspect previous output"),
    };
    if had_output {
        fs::rename(output, &previous)
            .whatever_context("unable to retain previous completed build")?;
    }
    if let Err(error) = fs::rename(candidate, output) {
        if had_output {
            if let Err(rollback) = fs::rename(&previous, output) {
                let recovery = backup.keep();
                snafu::whatever!("build promotion failed: {error}; rollback failed: {rollback}; previous build retained at `{}`; preview must not use incomplete output", recovery.display());
            }
        }
        return Err(error)
            .whatever_context("unable to promote candidate; previous completed build retained");
    }
    log::info!("completed build published to `{}`", output.display());
    Ok(())
}

pub(crate) fn candidate_output(
    build_root: &Path,
) -> Result<(tempfile::TempDir, PathBuf), Whatever> {
    let scratch = tempfile::Builder::new()
        .prefix(".build-candidate-")
        .tempdir_in(build_root)
        .whatever_context("unable to create private build candidate")?;
    let output = scratch.path().join("output");
    Ok((scratch, output))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(root: &Path, generation: &str) -> BuildInfo {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("index.html"), generation).unwrap();
        let info = BuildInfo {
            version: 1,
            complete: true,
            generation: generation.into(),
            base_url: "https://example.test/EIPs/".into(),
            mount_path: "/EIPs/".into(),
            tool_version: "fixture".into(),
            tool_binary_git_blob: "fixture".into(),
            zola_version: "0.22.1".into(),
            product: "EIPs".into(),
            source_head: "fixture".into(),
            materialized_head: "fixture".into(),
            merged_source_tree: "fixture".into(),
        };
        info.write(root).unwrap();
        info
    }
    #[test]
    fn only_completed_candidates_replace_previous_output() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        let first = fixture(&output, "first");
        let (_scratch, candidate) = candidate_output(temp.path()).unwrap();
        fs::create_dir(&candidate).unwrap();
        fs::write(candidate.join("index.html"), "incomplete").unwrap();
        assert!(publish(&candidate, &output).is_err());
        assert_eq!(BuildInfo::read(&output).unwrap(), first);
        let second = fixture(&candidate, "second");
        publish(&candidate, &output).unwrap();
        assert_eq!(BuildInfo::read(&output).unwrap(), second);
        assert_eq!(
            fs::read_to_string(output.join("index.html")).unwrap(),
            "second"
        );
    }
    #[test]
    fn incomplete_and_mismatched_records_are_not_previewable() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        assert!(BuildInfo::read(root).is_err());
        let mut info = fixture(root, "ok");
        info.complete = false;
        fs::write(root.join(RECEIPT), serde_json::to_vec(&info).unwrap()).unwrap();
        assert!(BuildInfo::read(root).is_err());
        info.complete = true;
        info.mount_path = "/ERCs/".into();
        fs::write(root.join(RECEIPT), serde_json::to_vec(&info).unwrap()).unwrap();
        assert!(BuildInfo::read(root).is_err());
    }
}
