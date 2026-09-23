/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Publication of an explicit external export. No delete-then-write window.
//! As with generated_fs, this is not a sandbox against another process swapping
//! parent directories under the same user's account between filesystem calls.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::Serialize;
use snafu::{OptionExt, ResultExt, Whatever};
use url::Url;

use super::{extract, format_corpus};
use crate::{
    config::SearchCorpusFormat,
    generated_fs,
    search::{normalized_base_path, CorpusSource, SearchCorpusSummary},
};

#[derive(Debug)]
pub(crate) struct ExportDestination {
    path: PathBuf,
    force: bool,
}

impl ExportDestination {
    pub(crate) fn new(path: &Path, force: bool) -> Result<Self, Whatever> {
        let raw = path.as_os_str().as_encoded_bytes();
        let directory_spelling = raw.is_empty()
            || raw == b"."
            || raw == b".."
            || raw.ends_with(b"/")
            || raw.ends_with(b"/.")
            || raw.ends_with(b"/..")
            || (cfg!(windows)
                && (raw.ends_with(b"\\") || raw.ends_with(b"\\.") || raw.ends_with(b"\\..")));
        if directory_spelling {
            snafu::whatever!("corpus output must name a regular file, not a directory path");
        }
        let absolute = std::path::absolute(path)
            .whatever_context("unable to resolve corpus output from invocation directory")?;
        let name = absolute
            .file_name()
            .whatever_context("corpus output must name a file")?;
        let parent = absolute
            .parent()
            .whatever_context("corpus output has no parent")?;
        // Inspect before canonicalizing: a symlink in the supplied path is not
        // made safe by resolving it. ParentDir works only through real dirs.
        validate_parent(parent)?;
        let parent = parent
            .canonicalize()
            .whatever_context("unable to resolve corpus output parent")?;
        let destination = Self {
            path: parent.join(name),
            force,
        };
        destination.check()?;
        Ok(destination)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn check(&self) -> Result<(), Whatever> {
        validate_parent(self.path.parent().unwrap())?;
        match fs::symlink_metadata(&self.path) {
            Ok(meta) if generated_fs::is_link(&meta) || !meta.is_file() => {
                snafu::whatever!("corpus destination `{}` is a symlink or non-regular file; refusing replacement", self.path.display());
            }
            Ok(_) if !self.force => {
                snafu::whatever!("corpus destination `{}` already exists; use --force to replace a regular export file", self.path.display());
            }
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).whatever_context("unable to inspect corpus destination"),
        }
    }

    fn write(&self, value: &impl Serialize) -> Result<(), Whatever> {
        self.check()?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".corpus-export-")
            .tempfile_in(self.path.parent().unwrap())
            .whatever_context("unable to create same-directory corpus temporary")?;
        serde_json::to_writer(&mut temporary, value)
            .whatever_context("unable to serialize corpus export")?;
        temporary
            .write_all(b"\n")
            .whatever_context("unable to finish corpus export")?;
        temporary
            .as_file()
            .sync_all()
            .whatever_context("unable to flush corpus export")?;
        self.check()?;
        if self.force {
            temporary.persist(&self.path).whatever_context(
                "unable to replace corpus export; previous destination retained",
            )?;
        } else {
            // Never clobber a file created by somebody else during extraction
            // or serialization, including the interval after the final check.
            temporary
                .persist_noclobber(&self.path)
                .whatever_context("corpus destination became occupied; refusing replacement")?;
        }
        Ok(())
    }
}

fn validate_parent(parent: &Path) -> Result<(), Whatever> {
    for ancestor in parent.ancestors() {
        let meta = fs::symlink_metadata(ancestor).with_whatever_context(|_| {
            format!(
                "corpus output parent must exist and be accessible: `{}`",
                ancestor.display()
            )
        })?;
        if generated_fs::is_link(&meta) || !meta.is_dir() {
            snafu::whatever!(
                "corpus output parent must contain only real directories, not symlinks: `{}`",
                ancestor.display()
            );
        }
    }
    Ok(())
}

pub(crate) fn export_corpus(
    rendered: &Path,
    base_url: &Url,
    source: CorpusSource,
    format: SearchCorpusFormat,
    destination: &ExportDestination,
) -> Result<SearchCorpusSummary, Whatever> {
    let mut extracted = extract::extract_corpus(rendered, &normalized_base_path(base_url))?;
    // Extraction supplies deployment-root-relative URLs. Joining with the
    // effective origin resolves them once, without adding the mount twice.
    for document in &mut extracted.documents {
        document.url = base_url
            .join(&document.url)
            .whatever_context("unable to resolve exported document URL")?
            .to_string();
    }
    for chunk in &mut extracted.chunks {
        chunk.url = base_url
            .join(&chunk.url)
            .whatever_context("unable to resolve exported chunk URL")?
            .to_string();
    }
    let corpus = format_corpus(extracted, format, source);
    destination.write(&corpus)?;
    Ok(SearchCorpusSummary {
        documents: corpus.documents.len(),
        chunks: corpus.chunks.len(),
        output_path: destination.path.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn export_refuses_overwrite_without_force_and_replaces_only_when_forced() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("corpus.json");
        ExportDestination::new(&file, false)
            .unwrap()
            .write(&json!({"first":true}))
            .unwrap();
        assert!(ExportDestination::new(&file, false).is_err());
        ExportDestination::new(&file, true)
            .unwrap()
            .write(&json!({"second":true}))
            .unwrap();
        assert_eq!(fs::read_to_string(file).unwrap(), "{\"second\":true}\n");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn export_requires_existing_directory_parent_and_regular_leaf() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ExportDestination::new(&dir.path().join("missing/out"), false).is_err());
        assert!(ExportDestination::new(dir.path(), true).is_err());
        assert!(ExportDestination::new(Path::new(""), false).is_err());
        for relative in ["new/", "new/.", "new/.."] {
            assert!(ExportDestination::new(&dir.path().join(relative), true).is_err());
        }
        fs::write(dir.path().join("file"), "KEEP").unwrap();
        assert!(ExportDestination::new(&dir.path().join("file/out"), true).is_err());
        assert!(ExportDestination::new(&dir.path().join("./file"), false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn export_refuses_symlink_leaf_parent_and_dangling_link_even_with_force() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("keep"), "KEEP").unwrap();
        fs::create_dir(dir.path().join("real")).unwrap();
        symlink(dir.path().join("keep"), dir.path().join("leaf")).unwrap();
        symlink(dir.path().join("absent"), dir.path().join("dangling")).unwrap();
        symlink(dir.path().join("real"), dir.path().join("alias")).unwrap();
        for relative in ["leaf", "dangling", "alias/output", "alias/../output"] {
            assert!(
                ExportDestination::new(&dir.path().join(relative), true).is_err(),
                "{relative}"
            );
        }
        let destination = ExportDestination::new(&dir.path().join("late"), true).unwrap();
        symlink(dir.path().join("keep"), destination.path()).unwrap();
        assert!(destination.write(&json!({"new":true})).is_err());
        assert_eq!(fs::read_to_string(dir.path().join("keep")).unwrap(), "KEEP");
    }

    #[test]
    fn new_occupant_and_serialization_error_preserve_destination() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("corpus.json");
        let destination = ExportDestination::new(&file, false).unwrap();
        fs::write(&file, "KEEP").unwrap();
        assert!(destination.write(&json!({"new":true})).is_err());
        struct Broken;
        impl Serialize for Broken {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("intentional serializer failure"))
            }
        }
        assert!(ExportDestination::new(&file, true)
            .unwrap()
            .write(&Broken)
            .is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "KEEP");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn extraction_failure_preserves_previous_export() {
        let dir = tempfile::tempdir().unwrap();
        let rendered = dir.path().join("rendered");
        fs::create_dir(&rendered).unwrap();
        fs::write(rendered.join("bad.html"), [0xff]).unwrap();
        let file = dir.path().join("corpus.json");
        fs::write(&file, "KEEP").unwrap();
        let dest = ExportDestination::new(&file, true).unwrap();
        assert!(export_corpus(
            &rendered,
            &Url::parse("https://example.test/EIPs/").unwrap(),
            super::super::tests::fixture_source(),
            SearchCorpusFormat::DocumentsAndChunks,
            &dest
        )
        .is_err());
        assert_eq!(fs::read_to_string(file).unwrap(), "KEEP");
    }

    #[test]
    fn export_encodes_filesystem_segments_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let rendered = dir.path().join("rendered");
        let special = rendered.join("reference/café 100% #/");
        fs::create_dir_all(&special).unwrap();
        fs::write(
            special.join("index.html"),
            "<main data-pagefind-body><h2 id='section% café'>Section</h2><p>Body.</p></main>",
        )
        .unwrap();
        let dest = ExportDestination::new(&dir.path().join("out.json"), false).unwrap();
        export_corpus(
            &rendered,
            &Url::parse("https://example.test/wg/EIPs/").unwrap(),
            super::super::tests::fixture_source(),
            SearchCorpusFormat::DocumentsAndChunks,
            &dest,
        )
        .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(dest.path()).unwrap()).unwrap();
        assert_eq!(
            value["documents"][0]["url"],
            "https://example.test/wg/EIPs/reference/caf%C3%A9%20100%25%20%23/"
        );
        assert_eq!(
            value["chunks"][0]["url"],
            "https://example.test/wg/EIPs/reference/caf%C3%A9%20100%25%20%23/#section%25%20caf%C3%A9"
        );
    }

    #[test]
    fn all_formats_resolve_absolute_urls_and_preserve_heading_and_source_context() {
        let dir = tempfile::tempdir().unwrap();
        let rendered = dir.path().join("rendered");
        fs::create_dir_all(rendered.join("1559")).unwrap();
        fs::write(rendered.join("1559/index.html"),
            "<main data-pagefind-body><span data-pagefind-meta=\"proposal:EIP-1559\"></span><h2 id=\"abstract\">Abstract</h2><p>Body.</p></main>").unwrap();
        for base in [
            "https://example.test/",
            "https://example.test/wg/review/EIPs/",
        ] {
            for format in [
                SearchCorpusFormat::Documents,
                SearchCorpusFormat::Chunks,
                SearchCorpusFormat::DocumentsAndChunks,
            ] {
                let mut source = super::super::tests::fixture_source();
                source.base_url = base.to_owned();
                source.only = Some(vec![1559]);
                let dest = ExportDestination::new(&dir.path().join("export.json"), true).unwrap();
                export_corpus(&rendered, &Url::parse(base).unwrap(), source, format, &dest)
                    .unwrap();
                let value: serde_json::Value =
                    serde_json::from_slice(&fs::read(dest.path()).unwrap()).unwrap();
                assert_eq!(value["source"]["base_url"], base);
                assert_eq!(value["source"]["only"], json!([1559]));
                assert_eq!(
                    value["documents"].as_array().unwrap().len(),
                    usize::from(format != SearchCorpusFormat::Chunks)
                );
                assert_eq!(
                    value["chunks"].as_array().unwrap().len(),
                    usize::from(format != SearchCorpusFormat::Documents)
                );
                if format != SearchCorpusFormat::Chunks {
                    assert_eq!(value["documents"][0]["url"], format!("{base}1559/"));
                }
                if format != SearchCorpusFormat::Documents {
                    assert_eq!(value["chunks"][0]["url"], format!("{base}1559/#abstract"));
                }
            }
        }
    }
}
