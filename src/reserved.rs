/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Finite generator ownership, not a provenance registry. Check raw inputs before
//! generation and live publication; check post-Zola destinations before indexing.
use std::{fs, path::{Component, Path}};
use serde::Deserialize;
use snafu::{ResultExt, Whatever};

struct Reservation {
    inputs: &'static [&'static str],
    route: Option<&'static str>,
    post_render: bool,
}

const RESERVED: &[Reservation] = &[
    Reservation { inputs: &["content/search.md", "content/search", "static/search"], route: Some("search"), post_render: false },
    Reservation { inputs: &["content/hardforks.md", "content/hardforks", "static/hardforks"], route: Some("hardforks"), post_render: false },
    Reservation { inputs: &["data/build_eips_search.toml"], route: None, post_render: false },
    Reservation { inputs: &["static/assets/data/proposals.json", "content/assets/data/proposals.json"], route: Some("assets/data/proposals.json"), post_render: false },
    Reservation { inputs: &["static/pagefind", "sass/pagefind", "content/pagefind", "content/pagefind.md"], route: Some("pagefind"), post_render: true },
    Reservation { inputs: &["static/assets/generated", "sass/assets/generated", "content/assets/generated", "content/assets/generated.md"], route: Some("assets/generated"), post_render: true },
    Reservation { inputs: &["static/.build-eips-build.json", "content/.build-eips-build.json"], route: Some(crate::build_artifact::RECEIPT), post_render: true },
    Reservation { inputs: &["themes/eips-theme"], route: None, post_render: false },
];

pub(crate) fn ensure_absent(root: &Path, relative: &str) -> Result<(), Whatever> {
    let path = root.join(relative);
    crate::generated_fs::check_path(root, &path, false)
        .with_whatever_context(|_| format!("unsafe reserved generated path `{}`", path.display()))?;
    match fs::symlink_metadata(&path) {
        Ok(_) => snafu::whatever!("reserved generated path `{}` is occupied; refusing to overwrite authored/project/theme content", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_whatever_context(|_| format!("unable to inspect reserved generated path `{}`", path.display())),
    }
}

fn route_is_reserved(route: &str) -> bool {
    // Normalize lexical route components, including aliases with a leading slash.
    let mut parts = Vec::new();
    for component in Path::new(route.trim_matches('/')).components() {
        match component {
            Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            Component::ParentDir => { parts.pop(); }
            _ => {}
        }
    }
    let route = parts.join("/");
    RESERVED.iter().filter_map(|r| r.route).any(|reserved| {
        route == reserved || route.strip_prefix(reserved).is_some_and(|tail| tail.starts_with('/'))
    })
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RouteOverrides {
    path: Option<String>,
    slug: Option<String>,
    aliases: Vec<String>,
}

/// Only native Zola TOML can override a page's route here: proposal YAML is
/// transformed to its numeric identity by markdown.rs, not forwarded to Zola.
fn check_content_routes(content: &Path) -> Result<(), Whatever> {
    if !content.is_dir() { return Ok(()); }
    for entry in walkdir::WalkDir::new(content).follow_links(true) {
        let entry = entry.whatever_context("unable to inspect authored content routes")?;
        if !entry.file_type().is_file() { continue; }
        let relative = entry.path().strip_prefix(content).unwrap();
        if entry.path().extension().is_none_or(|extension| extension != "md") { continue; }
        let text = fs::read_to_string(entry.path()).whatever_context("unable to read authored route input")?;
        let mut lines = text.lines();
        if lines.next().map(str::trim) != Some("+++") { continue; }
        let header = lines.take_while(|line| line.trim() != "+++").collect::<Vec<_>>().join("\n");
        let routes: RouteOverrides = toml::from_str(&header).with_whatever_context(|_| format!("unable to inspect route overrides in `{}`", entry.path().display()))?;
        let mut candidates = routes.aliases;
        if let Some(path) = routes.path { candidates.push(path); }
        if let Some(slug) = routes.slug {
            candidates.push(relative.parent().unwrap_or(Path::new("")).join(slug).to_string_lossy().into_owned());
        }
        for route in candidates {
            if route_is_reserved(&route) {
                snafu::whatever!("authored route `{route}` in `{}` collides with a reserved generated route", entry.path().display());
            }
        }
    }
    Ok(())
}

/// Caller first admits the tree under source_reads (including confined links).
pub(crate) fn validate_inputs(root: &Path) -> Result<(), Whatever> {
    for reservation in RESERVED {
        for input in reservation.inputs { ensure_absent(root, input)?; }
    }
    check_content_routes(&root.join("content"))?;
    // Declarative Zola route producers are admitted under the same finite policy,
    // including live theme configuration edits. Templates remain trusted code.
    for relative in ["config.toml", "config/zola.toml"] {
        let path = root.join(relative);
        if !path.is_file() { continue; }
        let text = fs::read_to_string(&path).whatever_context("unable to inspect Zola route configuration")?;
        let config: toml::Value = toml::from_str(&text).whatever_context("unable to parse Zola route configuration")?;
        check_config_routes(&config, &path)?;
        if let Some(languages) = config.get("languages").and_then(|v| v.as_table()) {
            for (language, settings) in languages {
                if route_is_reserved(language) { snafu::whatever!("language route `{language}` collides with a reserved generated route"); }
                check_config_routes(settings, &path)?;
            }
        }
    }
    Ok(())
}

fn check_config_routes(config: &toml::Value, path: &Path) -> Result<(), Whatever> {
    let taxonomies = config.get("taxonomies").and_then(|v| v.as_array()).into_iter().flatten()
        .filter_map(|v| v.get("name").and_then(|v| v.as_str()));
    let feeds = config.get("feed_filenames").and_then(|v| v.as_array()).into_iter().flatten().filter_map(|v| v.as_str());
    for route in taxonomies.chain(feeds) {
        if route_is_reserved(route) { snafu::whatever!("Zola route `{route}` in `{}` collides with a reserved generated route", path.display()); }
    }
    Ok(())
}

pub(crate) fn validate_rendered(root: &Path) -> Result<(), Whatever> {
    for reservation in RESERVED.iter().filter(|r| r.post_render) {
        ensure_absent(root, reservation.route.unwrap())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finite_input_paths_refuse_all_occupants_without_removing_them() {
        for relative in RESERVED.iter().flat_map(|r| r.inputs.iter()) {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "KEEP").unwrap();
            assert!(validate_inputs(temp.path()).is_err(), "{relative}");
            assert_eq!(fs::read_to_string(path).unwrap(), "KEEP");
        }
    }
    #[test]
    fn path_slug_and_alias_routes_cannot_occupy_generated_routes() {
        for field in ["path = '/search/'", "slug = 'hardforks'", "aliases = ['/pagefind/index.html']", "aliases = ['/assets/generated/search/corpus.json']", "path = '/assets/data/proposals.json'"] {
            let temp = tempfile::tempdir().unwrap();
            fs::create_dir(temp.path().join("content")).unwrap();
            fs::write(temp.path().join("content/ordinary.md"), format!("+++\n{field}\n+++\nBody\n")).unwrap();
            assert!(validate_inputs(temp.path()).is_err(), "{field}");
        }
        assert!(!route_is_reserved("searching"));
        assert!(!route_is_reserved("reference/search"));
        assert!(route_is_reserved("/assets/generated/other"));
    }
    #[test]
    fn rendered_collisions_are_rejected_even_when_feature_is_disabled() {
        for relative in ["pagefind", "assets/generated", crate::build_artifact::RECEIPT] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "KEEP").unwrap();
            assert!(validate_rendered(temp.path()).is_err());
            assert_eq!(fs::read_to_string(path).unwrap(), "KEEP");
        }
    }
}

#[cfg(test)]
mod configured_route_regressions {
    use super::*;
    #[test]
    fn configured_taxonomy_feed_and_language_routes_are_reserved() {
        for text in ["taxonomies = [{name = 'search'}]", "feed_filenames = ['assets/generated/corpus.xml']", "[languages.pagefind]\ntitle = 'Fixture'"] {
            let temp = tempfile::tempdir().unwrap();
            std::fs::create_dir(temp.path().join("config")).unwrap();
            std::fs::write(temp.path().join("config/zola.toml"), text).unwrap();
            assert!(validate_inputs(temp.path()).is_err(), "{text}");
        }
    }
}
