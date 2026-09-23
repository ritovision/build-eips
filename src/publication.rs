/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Diffed publication into a disposable live tree. Regular files and link leaves
//! are replaced by rename, never truncated or unlinked first. A directory/file
//! transition and a batch of files are NOT an atomic website transaction.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

pub(crate) fn changed_paths(
    source: &Path,
    destination: &Path,
    paths: &BTreeSet<PathBuf>,
) -> io::Result<BTreeSet<PathBuf>> {
    // Comparison must use the same transition plan as publication: inspecting
    // an obsolete child through a source leaf or an installed destination link
    // would reject a valid planned replacement (or follow the wrong tree).
    let plan = crate::tree_sync::Plan::new(source, destination, paths)?;
    let mut changed = BTreeSet::new();
    for path in paths {
        if plan.source_leaf_supersedes(path) {
            continue;
        }
        if plan.removes_destination_ancestor(path) {
            // The ancestor removal handles obsolete descendants. New source
            // descendants are compared against absence without walking the
            // currently installed link/file ancestor in the destination.
            if crate::tree_sync::inspect(source, path)?.is_some() {
                changed.insert(path.clone());
            }
        } else if !same_file(source, destination, path)? {
            changed.insert(path.clone());
        }
    }
    Ok(changed)
}

fn same_file(source: &Path, destination: &Path, path: &Path) -> io::Result<bool> {
    let source_meta = crate::tree_sync::inspect(source, path)?;
    let destination_meta = crate::tree_sync::inspect(destination, path)?;
    let (Some(left), Some(right)) = (source_meta.as_ref(), destination_meta.as_ref()) else {
        return Ok(source_meta.is_none() && destination_meta.is_none());
    };
    if crate::generated_fs::is_link(left) || crate::generated_fs::is_link(right) {
        return Ok(crate::generated_fs::is_link(left)
            && crate::generated_fs::is_link(right)
            && fs::read_link(source.join(path))? == fs::read_link(destination.join(path))?);
    }
    if left.is_dir() || right.is_dir() {
        return Ok(left.is_dir() && right.is_dir());
    }
    if !left.is_file() || !right.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported publication file type",
        ));
    }
    Ok(left.len() == right.len()
        && left.permissions() == right.permissions()
        && fs::read(source.join(path))? == fs::read(destination.join(path))?)
}

pub(crate) fn publish(
    source: &Path,
    destination: &Path,
    paths: &BTreeSet<PathBuf>,
) -> io::Result<usize> {
    let changed = changed_paths(source, destination, paths)?;
    if changed.is_empty() {
        return Ok(0);
    }
    let plan = crate::tree_sync::Plan::new(source, destination, &changed)?;
    // Keep temporary files outside the watched tree: Zola must not copy a
    // temporary static file or try to render partially written Markdown.
    let parent = source
        .parent()
        .ok_or_else(|| io::Error::other("private staging has no parent"))?;
    let scratch = tempfile::Builder::new()
        .prefix(".serve-publication-")
        .tempdir_in(parent)?;
    plan.prepare_atomic_destination(destination)?;
    for path in &plan.files {
        let source_path = source.join(path);
        let destination_path = destination.join(path);
        crate::generated_fs::check_path(source, &source_path, true)?;
        crate::generated_fs::check_path(destination, &destination_path, true)?;
        let metadata = fs::symlink_metadata(&source_path)?;
        let temporary = tempfile::NamedTempFile::new_in(scratch.path())?;
        if crate::generated_fs::is_link(&metadata) {
            let temporary = temporary.into_temp_path();
            fs::remove_file(&temporary)?;
            crate::git::copy_symlink(&source_path, &temporary)?;
            temporary.persist(&destination_path).map_err(|e| e.error)?;
        } else {
            fs::copy(&source_path, temporary.path())?;
            temporary
                .as_file()
                .set_permissions(metadata.permissions())?;
            temporary.persist(&destination_path).map_err(|e| e.error)?;
        }
        log::debug!("published live file `{}`", path.display());
    }
    Ok(changed.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, name: &str, value: &str) {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }

    #[test]
    fn unchanged_files_keep_their_identity_and_replacements_preserve_old_hardlinks() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("live");
        write(&source, "content/page.md", "same");
        write(&destination, "content/page.md", "same");
        let path = destination.join("content/page.md");
        fs::hard_link(&path, temp.path().join("old-link")).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let paths = BTreeSet::from([PathBuf::from("content/page.md")]);
        assert_eq!(publish(&source, &destination, &paths).unwrap(), 0);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        write(&source, "content/page.md", "new");
        assert_eq!(publish(&source, &destination, &paths).unwrap(), 1);
        assert_eq!(fs::read_to_string(path).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(temp.path().join("old-link")).unwrap(),
            "same"
        );
        assert_eq!(publish(&source, &destination, &paths).unwrap(), 0);
    }

    #[test]
    fn file_directory_transitions_and_deletions_converge() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("live");
        write(&source, "static/item/child", "child");
        write(&destination, "static/item", "old");
        let paths = BTreeSet::from([
            PathBuf::from("static/item"),
            PathBuf::from("static/item/child"),
        ]);
        publish(&source, &destination, &paths).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("static/item/child")).unwrap(),
            "child"
        );
        fs::remove_dir_all(source.join("static/item")).unwrap();
        write(&source, "static/item", "leaf");
        publish(&source, &destination, &paths).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("static/item")).unwrap(),
            "leaf"
        );
        fs::remove_file(source.join("static/item")).unwrap();
        publish(&source, &destination, &paths).unwrap();
        assert!(!destination.join("static/item").exists());
    }

    #[cfg(unix)]
    #[test]
    fn link_leaves_are_replaced_without_following_them() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("live");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(temp.path().join("outside"), "keep").unwrap();
        std::os::unix::fs::symlink(temp.path().join("outside"), destination.join("leaf")).unwrap();
        write(&source, "leaf", "replace");
        let paths = BTreeSet::from([PathBuf::from("leaf")]);
        publish(&source, &destination, &paths).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("outside")).unwrap(),
            "keep"
        );
        fs::remove_file(source.join("leaf")).unwrap();
        std::os::unix::fs::symlink("target", source.join("leaf")).unwrap();
        publish(&source, &destination, &paths).unwrap();
        assert_eq!(
            fs::read_link(destination.join("leaf")).unwrap(),
            Path::new("target")
        );
        assert_eq!(publish(&source, &destination, &paths).unwrap(), 0);
    }
    #[cfg(unix)]
    #[test]
    fn scheduled_link_directory_transitions_compare_publish_and_retry_without_following_links() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("live");
        let outside = temp.path().join("outside");
        write(&source, "static/item/child", "new child");
        write(&outside, "sentinel", "keep");
        fs::create_dir_all(destination.join("static")).unwrap();
        symlink(&outside, destination.join("static/item")).unwrap();
        let paths = BTreeSet::from([
            PathBuf::from("static/item"),
            PathBuf::from("static/item/child"),
        ]);
        assert_eq!(changed_paths(&source, &destination, &paths).unwrap(), paths);
        let permissions = fs::metadata(destination.join("static"))
            .unwrap()
            .permissions();
        fs::set_permissions(
            destination.join("static"),
            fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        let failure = publish(&source, &destination, &paths);
        fs::set_permissions(destination.join("static"), permissions).unwrap();
        assert!(
            failure.is_err(),
            "the fault must prevent removal of the installed link"
        );
        assert_eq!(publish(&source, &destination, &paths).unwrap(), 2);
        assert_eq!(
            fs::read_to_string(destination.join("static/item/child")).unwrap(),
            "new child"
        );
        assert_eq!(publish(&source, &destination, &paths).unwrap(), 0);
        assert_eq!(
            fs::read_to_string(outside.join("sentinel")).unwrap(),
            "keep"
        );
        assert!(!outside.join("child").exists());
        fs::remove_dir_all(source.join("static/item")).unwrap();
        symlink("payload.txt", source.join("static/item")).unwrap();
        let leaf = BTreeSet::from([PathBuf::from("static/item")]);
        assert_eq!(changed_paths(&source, &destination, &paths).unwrap(), leaf);
        assert_eq!(publish(&source, &destination, &paths).unwrap(), 1);
        assert_eq!(
            fs::read_link(destination.join("static/item")).unwrap(),
            Path::new("payload.txt")
        );
        assert_eq!(publish(&source, &destination, &paths).unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn comparison_still_rejects_an_unplanned_source_link_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("live");
        let outside = temp.path().join("outside");
        write(&outside, "child", "must not read through link");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        std::os::unix::fs::symlink(&outside, source.join("link")).unwrap();
        let paths = BTreeSet::from([PathBuf::from("link/child")]);
        assert!(changed_paths(&source, &destination, &paths).is_err());
        assert!(publish(&source, &destination, &paths).is_err());
        assert!(!destination.join("link").exists());
    }
}
