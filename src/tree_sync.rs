/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Plan tree-shape changes before touching the destination. Missing descendants
//! under a regular file are absent, not failed deletions. Never follow a link
//! ancestor to decide absence or to mutate a descendant.
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Component, Path, PathBuf},
};

fn invalid(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("unsafe tree transition `{}`: {why}", path.display()),
    )
}

pub(crate) fn inspect(root: &Path, relative: &Path) -> io::Result<Option<fs::Metadata>> {
    if relative
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(invalid(relative, "not a relative file path"));
    }
    crate::generated_fs::check_path(root, root, false)?;
    let mut current = root.to_path_buf();
    let count = relative.components().count();
    for (i, component) in relative.components().enumerate() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if i + 1 == count => return Ok(Some(meta)),
            Ok(meta) if crate::generated_fs::is_link(&meta) => {
                return Err(invalid(&current, "link ancestor"))
            }
            Ok(meta) if !meta.is_dir() => return Ok(None),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    Err(invalid(relative, "empty file path"))
}

pub(crate) struct Plan {
    removals: BTreeSet<PathBuf>,
    directories: BTreeSet<PathBuf>,
    pub(crate) files: BTreeSet<PathBuf>,
    pub(crate) shape_changed: bool,
}

impl Plan {
    pub(crate) fn new(
        source: &Path,
        destination: &Path,
        paths: &BTreeSet<PathBuf>,
    ) -> io::Result<Self> {
        let mut files = BTreeSet::new();
        let mut directories = BTreeSet::new();
        let mut absent = BTreeSet::new();
        // BTree order puts each ancestor before its descendants. A new leaf
        // supersedes obsolete descendants without attempting to inspect them.
        for path in paths {
            if files.iter().any(|p: &PathBuf| path.starts_with(p)) {
                continue;
            }
            match inspect(source, path)? {
                Some(meta) if meta.is_file() || crate::generated_fs::is_link(&meta) => {
                    files.insert(path.clone());
                }
                Some(meta) if meta.is_dir() => {
                    directories.insert(path.clone());
                }
                Some(_) => return Err(invalid(path, "unsupported source file type")),
                None => {
                    absent.insert(path.clone());
                }
            }
        }
        for file in &files {
            for parent in file
                .ancestors()
                .skip(1)
                .filter(|p| !p.as_os_str().is_empty())
            {
                directories.insert(parent.to_path_buf());
            }
        }
        let mut removals = BTreeSet::new();
        let mut shape_changed = false;
        // Remove obstructing leaves at directory positions before inspecting
        // their descendants. Replacing an old symlink unlinks it; never follows it.
        for dir in &directories {
            if removals.iter().any(|p: &PathBuf| dir.starts_with(p)) {
                continue;
            }
            if inspect(destination, dir)?
                .is_some_and(|m| !m.is_dir() || crate::generated_fs::is_link(&m))
            {
                shape_changed = true;
                removals.insert(dir.clone());
            }
        }
        for path in absent.iter().chain(files.iter()) {
            if removals.iter().any(|p| path.starts_with(p)) {
                continue;
            }
            let existing = inspect(destination, path)?;
            if absent.contains(path) && existing.is_some()
                || files.contains(path) && existing.as_ref().is_none_or(|m| m.is_dir())
            {
                shape_changed = true;
            }
            if existing.is_some() {
                removals.insert(path.clone());
            }
        }
        // A parent replacement makes deleting any children redundant. Do not
        // leave those children queued as permanently impossible future writes.
        let mut normalized = BTreeSet::new();
        for path in removals {
            if !normalized.iter().any(|p: &PathBuf| path.starts_with(p)) {
                normalized.insert(path);
            }
        }
        Ok(Self {
            removals: normalized,
            directories,
            files,
            shape_changed,
        })
    }

    pub(crate) fn source_leaf_supersedes(&self, path: &Path) -> bool {
        self.files
            .iter()
            .any(|leaf| leaf != path && path.starts_with(leaf))
    }

    pub(crate) fn removes_destination_ancestor(&self, path: &Path) -> bool {
        self.removals
            .iter()
            .any(|ancestor| ancestor != path && path.starts_with(ancestor))
    }

    pub(crate) fn remove_paths(destination: &Path, paths: &BTreeSet<PathBuf>) -> io::Result<bool> {
        let mut removals = BTreeSet::new();
        for path in paths {
            if removals
                .iter()
                .any(|ancestor: &PathBuf| path.starts_with(ancestor))
            {
                continue;
            }
            if inspect(destination, path)?.is_some() {
                removals.insert(path.clone());
            }
        }
        let changed = !removals.is_empty();
        Self {
            removals,
            directories: BTreeSet::new(),
            files: BTreeSet::new(),
            shape_changed: changed,
        }
        .prepare_destination(destination)?;
        Ok(changed)
    }

    pub(crate) fn prepare_destination(&self, destination: &Path) -> io::Result<()> {
        self.prepare_destination_inner(destination, false)
    }

    pub(crate) fn prepare_atomic_destination(&self, destination: &Path) -> io::Result<()> {
        self.prepare_destination_inner(destination, true)
    }

    fn prepare_destination_inner(&self, destination: &Path, atomic_leaves: bool) -> io::Result<()> {
        for path in &self.removals {
            // Atomic publishers replace a leaf by rename. Only deletion and
            // incompatible directory shapes need an explicit removal first.
            if atomic_leaves
                && self.files.contains(path)
                && inspect(destination, path)?
                    .is_some_and(|m| !m.is_dir() || crate::generated_fs::is_link(&m))
            {
                continue;
            }
            let path = destination.join(path);
            crate::generated_fs::check_path(destination, &path, true)?;
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() && !crate::generated_fs::is_link(&meta) => {
                    fs::remove_dir_all(&path)?
                }
                #[cfg(windows)]
                Ok(meta) if meta.is_dir() => fs::remove_dir(&path)?,
                Ok(_) => fs::remove_file(&path)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        for path in &self.directories {
            let path = destination.join(path);
            crate::generated_fs::check_path(destination, &path, false)?;
            fs::create_dir_all(path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(root: &Path, path: &str) {
        let p = root.join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, "test").unwrap();
    }
    #[test]
    fn replacements_normalize_descendants_and_work_in_both_directions() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("src");
        let dst = t.path().join("dst");
        write(&src, "static/entry");
        write(&dst, "static/entry/child");
        let paths = [
            PathBuf::from("static/entry"),
            PathBuf::from("static/entry/child"),
        ]
        .into_iter()
        .collect();
        let p = Plan::new(&src, &dst, &paths).unwrap();
        p.prepare_destination(&dst).unwrap();
        assert_eq!(p.files.len(), 1);
        fs::copy(src.join("static/entry"), dst.join("static/entry")).unwrap();
        fs::remove_file(src.join("static/entry")).unwrap();
        write(&src, "static/entry/child");
        let p = Plan::new(&src, &dst, &paths).unwrap();
        p.prepare_destination(&dst).unwrap();
        assert!(dst.join("static/entry").is_dir());
        assert_eq!(p.files.len(), 1);
    }
    #[test]
    fn stale_descendant_of_existing_file_is_a_noop_deletion() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("src");
        let dst = t.path().join("dst");
        write(&src, "static/entry");
        write(&dst, "static/entry");
        let paths = [PathBuf::from("static/entry/child")].into_iter().collect();
        Plan::new(&src, &dst, &paths)
            .unwrap()
            .prepare_destination(&dst)
            .unwrap();
        assert_eq!(fs::read(dst.join("static/entry")).unwrap(), b"test");
    }
    #[cfg(unix)]
    #[test]
    fn replacing_link_ancestor_never_mutates_its_target() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("src");
        let dst = t.path().join("dst");
        let out = t.path().join("out");
        write(&src, "entry/child");
        write(&out, "sentinel");
        fs::create_dir(&dst).unwrap();
        std::os::unix::fs::symlink(&out, dst.join("entry")).unwrap();
        let paths = [PathBuf::from("entry"), PathBuf::from("entry/child")]
            .into_iter()
            .collect();
        Plan::new(&src, &dst, &paths)
            .unwrap()
            .prepare_destination(&dst)
            .unwrap();
        assert_eq!(fs::read(out.join("sentinel")).unwrap(), b"test");
        assert!(dst.join("entry").is_dir());
    }
}
