/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Repository-level Markdown aliases on Git for Windows with core.symlinks=false.
//! Only an index-recorded symlink with its exact Git blob is interpreted as a
//! stub. Ordinary text files never become links, and the resolved target must
//! remain in the content root. No symlink privilege or global Git setting needed.
use snafu::{OptionExt, ResultExt, Whatever};
use std::path::{Path, PathBuf};

pub(crate) fn content_alias(content: &Path, alias: &Path) -> Result<PathBuf, Whatever> {
    let content = content
        .canonicalize()
        .whatever_context("unable to resolve content root")?;
    let repository = content
        .parent()
        .whatever_context("content root has no repository parent")?;
    let alias = alias
        .canonicalize()
        .whatever_context("unable to resolve Markdown alias")?;
    let relative = alias
        .strip_prefix(repository)
        .whatever_context("Markdown alias is outside repository")?;
    if relative
        .components()
        .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git"))
    {
        snafu::whatever!("Markdown aliases must not refer to Git metadata");
    }
    crate::generated_fs::check_path(repository, &alias, false)
        .whatever_context("Markdown alias must be an ordinary checked-out stub")?;
    let repo = crate::git::open_generated_repository(repository)
        .whatever_context("unable to inspect repository-level Markdown alias")?;
    let index = repo
        .index()
        .whatever_context("unable to read Markdown alias index")?;
    let entry = index
        .get_path(relative, 0)
        .whatever_context("Markdown alias is not tracked")?;
    if entry.mode != 0o120000 {
        snafu::whatever!("Markdown target is outside content root and is not a tracked symlink");
    }
    let blob = repo
        .find_blob(entry.id)
        .whatever_context("unable to read Markdown alias Git blob")?;
    let bytes = std::fs::read(&alias).whatever_context("unable to inspect Markdown alias stub")?;
    if bytes != blob.content() {
        snafu::whatever!("Markdown alias differs from its prepared Git symlink");
    }
    let target =
        std::str::from_utf8(&bytes).whatever_context("Markdown alias target must be UTF-8")?;
    let target = alias.parent().unwrap().join(target);
    let resolved = target
        .canonicalize()
        .whatever_context("unable to resolve tracked Markdown alias target")?;
    if !resolved.starts_with(&content) {
        snafu::whatever!("tracked Markdown alias target is outside content root");
    }
    if resolved
        .strip_prefix(&content)
        .unwrap()
        .components()
        .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git"))
    {
        snafu::whatever!("tracked Markdown aliases must not expose Git metadata");
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(target: &str, mode: u32) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(t.path()).unwrap();
        std::fs::create_dir(t.path().join("content")).unwrap();
        std::fs::write(t.path().join("content/LICENSE.md"), "Public license").unwrap();
        std::fs::write(t.path().join("LICENSE.md"), target).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("LICENSE.md")).unwrap();
        let mut entry = index.get_path(Path::new("LICENSE.md"), 0).unwrap();
        entry.mode = mode;
        entry.id = repo.blob(target.as_bytes()).unwrap();
        index.add(&entry).unwrap();
        index.write().unwrap();
        t
    }
    #[test]
    fn verified_git_stub_resolves_to_content_without_source_mutation() {
        let t = fixture("content/LICENSE.md", 0o120000);
        assert_eq!(
            content_alias(&t.path().join("content"), &t.path().join("LICENSE.md")).unwrap(),
            t.path().join("content/LICENSE.md").canonicalize().unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(t.path().join("LICENSE.md")).unwrap(),
            "content/LICENSE.md"
        );
    }
    #[test]
    fn ordinary_text_changed_stubs_and_outside_targets_are_not_admitted() {
        let t = fixture("content/LICENSE.md", 0o100644);
        assert!(content_alias(&t.path().join("content"), &t.path().join("LICENSE.md")).is_err());
        let t = fixture("content/LICENSE.md", 0o120000);
        std::fs::write(t.path().join("LICENSE.md"), "content/OTHER.md").unwrap();
        assert!(content_alias(&t.path().join("content"), &t.path().join("LICENSE.md")).is_err());
        let t = fixture("outside.md", 0o120000);
        std::fs::write(t.path().join("outside.md"), "Not content").unwrap();
        assert!(content_alias(&t.path().join("content"), &t.path().join("LICENSE.md")).is_err());
    }
}
