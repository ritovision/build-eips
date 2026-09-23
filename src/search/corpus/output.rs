/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One normal-site destination, written only into a fresh rendered candidate.
use super::schema::CorpusOutput;
use crate::search::SITE_CORPUS_PATH;
use snafu::{ResultExt, Whatever};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub(super) fn write_corpus_file(
    output_path: &Path,
    corpus: &CorpusOutput,
) -> Result<PathBuf, Whatever> {
    crate::reserved::ensure_absent(output_path, SITE_CORPUS_PATH)?;
    let output_file = output_path.join(SITE_CORPUS_PATH);
    let parent = output_file.parent().unwrap();
    crate::generated_fs::check_path(output_path, &output_file, false)
        .whatever_context("unsafe site corpus destination")?;
    fs::create_dir_all(parent).whatever_context("unable to create site corpus directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .whatever_context("unable to create temporary site corpus")?;
    serde_json::to_writer(&mut temporary, corpus)
        .whatever_context("unable to encode site corpus")?;
    temporary
        .write_all(b"\n")
        .whatever_context("unable to finish site corpus")?;
    temporary
        .as_file()
        .sync_all()
        .whatever_context("unable to flush site corpus")?;
    temporary
        .persist_noclobber(&output_file)
        .whatever_context("site corpus destination is occupied; refusing replacement")?;
    Ok(output_file)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn empty() -> CorpusOutput {
        CorpusOutput {
            version: 1,
            source: super::super::tests::fixture_source(),
            documents: Vec::new(),
            chunks: Vec::new(),
        }
    }
    #[test]
    fn site_corpus_never_overwrites_authored_files_or_directories() {
        for directory in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let dest = temp.path().join(SITE_CORPUS_PATH);
            fs::create_dir_all(dest.parent().unwrap()).unwrap();
            if directory {
                fs::create_dir(&dest).unwrap();
            } else {
                fs::write(&dest, "KEEP").unwrap();
            }
            assert!(write_corpus_file(temp.path(), &empty()).is_err());
            if directory {
                assert!(dest.is_dir());
            } else {
                assert_eq!(fs::read_to_string(dest).unwrap(), "KEEP");
            }
        }
    }
    #[cfg(unix)]
    #[test]
    fn corpus_refuses_leaf_and_ancestor_links_without_changing_targets() {
        use std::os::unix::fs::symlink;
        for leaf in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("output");
            fs::create_dir_all(&root).unwrap();
            let outside = temp.path().join("outside");
            fs::create_dir_all(&outside).unwrap();
            fs::write(outside.join("keep.json"), "KEEP").unwrap();
            if leaf {
                let dest = root.join(SITE_CORPUS_PATH);
                fs::create_dir_all(dest.parent().unwrap()).unwrap();
                symlink(outside.join("keep.json"), dest).unwrap();
            } else {
                symlink(&outside, root.join("assets")).unwrap();
            }
            assert!(write_corpus_file(&root, &empty()).is_err());
            assert_eq!(
                fs::read_to_string(outside.join("keep.json")).unwrap(),
                "KEEP"
            );
        }
    }
}
