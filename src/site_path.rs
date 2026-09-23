/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Convert relative filesystem paths at the site-path boundary, never vice versa.
use std::path::{Component, Path};
use snafu::{OptionExt, Whatever};

pub(crate) fn relative(path: &Path) -> Result<String, Whatever> {
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => segments.push(name.to_str().whatever_context("site paths must be UTF-8")?),
            Component::CurDir => {},
            _ => snafu::whatever!("site path `{}` must be relative and contain no parent traversal", path.display()),
        }
    }
    Ok(segments.join("/"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_filesystem_components_become_site_separators() {
        assert_eq!(relative(&Path::new("00858").join("assets").join("calculations")).unwrap(), "00858/assets/calculations");
        assert_eq!(relative(Path::new("")).unwrap(), "");
        assert_eq!(relative(Path::new("café 100% #")).unwrap(), "café 100% #");
    }
    #[test]
    fn conversion_does_not_discard_parent_or_absolute_components() {
        assert!(relative(Path::new("../outside")).is_err());
        assert!(relative(Path::new("inside/../outside")).is_err());
        assert!(relative(&std::env::current_dir().unwrap()).is_err());
    }
}
