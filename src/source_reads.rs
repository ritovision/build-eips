/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Read admission for self-contained renderer inputs, separate from mutation
//! guards and preview routing. Reject external/metadata link targets before
//! Markdown or Zola can dereference them. Trusted templates/tools are not sandboxed.
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

fn git_component(name: &std::ffi::OsStr) -> bool {
    if cfg!(windows) {
        name.to_string_lossy().eq_ignore_ascii_case(".git")
    } else {
        name == ".git"
    }
}

fn rejected(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("source-read policy rejects `{}`: {why}", path.display()),
    )
}

pub(crate) fn validate_tree(root: &Path) -> io::Result<()> {
    crate::generated_fs::check_path(root, root, false)?;
    let root = root.canonicalize()?;
    inspect(&root, &root, &mut BTreeSet::new(), &mut BTreeSet::new())
}

fn inspect(
    root: &Path,
    path: &Path,
    active: &mut BTreeSet<PathBuf>,
    done: &mut BTreeSet<PathBuf>,
) -> io::Result<()> {
    if path.file_name().is_some_and(|n| n == ".gitattributes") {
        let meta = fs::symlink_metadata(path)?;
        if !meta.is_file() || crate::generated_fs::is_link(&meta) {
            return Err(rejected(
                path,
                "Git attribute configuration must be an ordinary file, not a link or directory",
            ));
        }
    }
    let resolved = path
        .canonicalize()
        .map_err(|e| rejected(path, &format!("unresolvable input/link target ({e})")))?;
    let relative = resolved.strip_prefix(root).map_err(|_| {
        rejected(
            path,
            "target is outside the approved materialized input root",
        )
    })?;
    if relative.components().any(|p| git_component(p.as_os_str())) {
        return Err(rejected(path, "links must not expose Git metadata"));
    }
    let meta = fs::metadata(&resolved)?;
    if meta.is_file() {
        return Ok(());
    }
    if !meta.is_dir() {
        return Err(rejected(
            path,
            "only regular files/directories and confined links are allowed",
        ));
    }
    if active.contains(&resolved) {
        return Err(rejected(path, "cyclic directory link"));
    }
    if done.contains(&resolved) {
        return Ok(());
    }
    active.insert(resolved.clone());
    for entry in fs::read_dir(&resolved)? {
        let entry = entry?;
        // Root Git administration belongs to the repository owner, not renderer
        // content. A link to it anywhere else is explicitly rejected above.
        if resolved == root && git_component(&entry.file_name()) {
            continue;
        }
        inspect(root, &entry.path(), active, done)?;
    }
    active.remove(&resolved);
    done.insert(resolved);
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    #[test]
    fn external_file_directory_and_metadata_links_are_refused_without_reads() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("input");
        let out = t.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&out).unwrap();
        fs::write(out.join("sentinel"), "KEEP").unwrap();
        for target in [out.join("sentinel"), out.clone()] {
            symlink(target, root.join("alias")).unwrap();
            assert!(validate_tree(&root).is_err());
            fs::remove_file(root.join("alias")).unwrap();
        }
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "SECRET").unwrap();
        symlink(".git/config", root.join("alias")).unwrap();
        assert!(validate_tree(&root).is_err());
        assert_eq!(fs::read_to_string(out.join("sentinel")).unwrap(), "KEEP");
    }
    #[test]
    fn internal_file_and_directory_links_work_but_cycles_do_not() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        fs::create_dir(root.join("assets")).unwrap();
        fs::write(root.join("assets/a.txt"), "PUBLIC").unwrap();
        symlink("assets/a.txt", root.join("file")).unwrap();
        symlink("assets", root.join("directory")).unwrap();
        validate_tree(root).unwrap();
        symlink("..", root.join("assets/cycle")).unwrap();
        assert!(validate_tree(root).is_err());
    }
    #[test]
    fn broken_links_are_explicit_failures_not_silently_omitted_inputs() {
        let t = tempfile::tempdir().unwrap();
        symlink("missing", t.path().join("alias")).unwrap();
        assert!(validate_tree(t.path()).is_err());
    }
}
