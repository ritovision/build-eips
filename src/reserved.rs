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

fn confined_route(route: &str, source: &Path) -> Result<(), Whatever> {
    // Zola appends route components to its output path. Never merely normalize
    // traversal for comparison and then hand the original escape to the writer.
    let route = route.trim();
    if route.contains('\\') || route.contains('\0') || route.split('/').any(|part| {
        part == ".." || (part.len() >= 2 && part.as_bytes()[0].is_ascii_alphabetic() && part.as_bytes()[1] == b':')
    }) {
        snafu::whatever!("route `{route}` in `{}` is not confined to the rendered site; parent traversal and filesystem prefixes are forbidden", source.display());
    }
    Ok(())
}

fn route_is_reserved(route: &str) -> bool {
    fn owned(route: &str) -> bool {
        RESERVED.iter().filter_map(|r| r.route).any(|reserved| {
            route == reserved || route.strip_prefix(reserved).is_some_and(|tail| tail.starts_with('/'))
        })
    }
    let parts = Path::new(route.trim().trim_matches('/')).components()
        .filter_map(|c| match c { Component::Normal(p) => Some(p.to_string_lossy().into_owned()), _ => None })
        .collect::<Vec<_>>();
    // Reserve equivalent names across Zola's on/safe/off strategies. Admission
    // must stay valid when the independently watched theme changes strategy.
    // `on` uses the same slug 0.1.6 implementation as Zola 0.22.1.
    let exact = parts.join("/");
    let normal = parts.iter().map(slug::slugify).collect::<Vec<_>>().join("/");
    let safe = parts.iter().map(|p| p.trim_end_matches([' ', '.']).chars()
        .filter(|c| !"<>:\"/\\|?*".contains(*c)).collect::<String>()).collect::<Vec<_>>().join("/");
    owned(&exact) || owned(&normal) || owned(&safe)
}

fn check_route(route: &str, source: &Path) -> Result<(), Whatever> {
    confined_route(route, source)?;
    if route_is_reserved(route) {
        snafu::whatever!("authored route `{route}` in `{}` collides with a reserved generated route", source.display());
    }
    Ok(())
}

/// Zola slugifies the entire slug field, unlike a route's separate components.
/// For example safe-mode taxonomy "se/arch" is one slug "search", not two paths.
fn check_slug(container: &Path, value: &str, source: &Path) -> Result<(), Whatever> {
    confined_route(value, source)?;
    let safe = value.trim_end_matches([' ', '.']).chars()
        .filter(|c| !"<>:\"/\\|?*".contains(*c)).collect::<String>();
    for variant in [value.to_owned(), slug::slugify(value), safe] {
        check_route(&container.join(variant).to_string_lossy(), source)?;
    }
    Ok(())
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RouteOverrides {
    path: Option<String>,
    slug: Option<String>,
    aliases: Vec<String>,
    paginate_path: Option<String>,
    taxonomies: std::collections::BTreeMap<String, Vec<String>>,
}

fn undated_name(name: &str) -> Option<&str> {
    // Match the dated-filename behavior without interpreting ordinary dashes as
    // dates. Both RFC3339 timestamps and YYYY-MM-DD prefixes can precede - or _.
    name.char_indices().filter(|(_, c)| *c == '-' || *c == '_').find_map(|(i, _)| {
        let prefix = name[..i].trim_end();
        if prefix.len() >= 10 && (chrono::NaiveDate::parse_from_str(prefix, "%Y-%m-%d").is_ok()
            || chrono::DateTime::parse_from_rfc3339(prefix).is_ok()) {
            name.get(i + 1..).filter(|s| !s.is_empty())
        } else { None }
    })
}

/// YAML proposals get numeric routes from markdown.rs. Native Zola TOML retains
/// explicit paths and infers slugs from filenames/bundles, so check both here.
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
        for route in &routes.aliases { check_route(route, entry.path())?; }
        if let Some(path) = &routes.path { check_route(path, entry.path())?; }
        if let Some(slug) = &routes.slug { confined_route(slug, entry.path())?; }
        if let Some(pagination) = &routes.paginate_path { confined_route(pagination, entry.path())?; }
        for (taxonomy, terms) in &routes.taxonomies {
            confined_route(taxonomy, entry.path())?;
            for term in terms { confined_route(term, entry.path())?; }
        }
        if routes.path.is_some() { continue; }
        let filename = relative.file_stem().unwrap().to_string_lossy();
        let parent = relative.parent().unwrap_or(Path::new(""));
        // Language-qualified bundle index names share the same bundle semantics.
        // Check both qualified/unqualified forms so future language configuration
        // cannot turn an admitted path into a reserved route.
        let mut names = vec![filename.as_ref()];
        if let Some((name, _)) = filename.split_once('.') { names.push(name); }
        for name in names {
            if name == "_index" {
                check_route(&parent.to_string_lossy(), entry.path())?;
                if let Some(pagination) = &routes.paginate_path {
                    check_route(&parent.join(pagination).to_string_lossy(), entry.path())?;
                }
                continue;
            }
            let (container, default_slug) = if name == "index" {
                if parent.as_os_str().is_empty() { continue; }
                (parent.parent().unwrap_or(Path::new("")), parent.file_name().unwrap().to_string_lossy().into_owned())
            } else { (parent, name.to_owned()) };
            let mut slugs = vec![routes.slug.as_deref().unwrap_or(&default_slug)];
            if routes.slug.is_none() {
                if let Some(short) = undated_name(&default_slug) { slugs.push(short); }
            }
            for slug in slugs {
                check_slug(container, slug, entry.path())?;
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
                check_route(language, &path)?;
                check_config_routes(settings, &path)?;
            }
        }
    }
    Ok(())
}

fn check_config_routes(config: &toml::Value, path: &Path) -> Result<(), Whatever> {
    for field in ["taxonomy_root", "default_language"] {
        if let Some(value) = config.get(field).and_then(|v| v.as_str()) { check_route(value, path)?; }
    }
    for taxonomy in config.get("taxonomies").and_then(|v| v.as_array()).into_iter().flatten() {
        for field in ["name", "slug"] {
            if let Some(value) = taxonomy.get(field).and_then(|v| v.as_str()) { check_slug(Path::new(""), value, path)?; }
        }
        if let Some(value) = taxonomy.get("paginate_path").and_then(|v| v.as_str()) { confined_route(value, path)?; }
    }
    for route in config.get("feed_filenames").and_then(|v| v.as_array()).into_iter().flatten().filter_map(|v| v.as_str()) {
        check_route(route, path)?;
    }
    Ok(())
}

/// Git tracks files, not empty working directories. Removing a rejected raw
/// candidate's leaf paths can leave empty directories behind in the private
/// stage. Remove only those empty reserved directories before admission. Never
/// call this on authored checkouts or rendered output; occupied paths still fail.
pub(crate) fn clear_empty_staged_reservations(root: &Path) -> Result<(), Whatever> {
    for relative in RESERVED.iter().flat_map(|r| r.inputs.iter()) {
        let path = root.join(relative);
        crate::generated_fs::check_path(root, &path, false)
            .whatever_context("unsafe reserved staging path")?;
        if !path.is_dir() { continue; }
        for entry in walkdir::WalkDir::new(&path).contents_first(true).follow_links(false) {
            let entry = entry.whatever_context("unable to inspect reserved staging directories")?;
            if !entry.file_type().is_dir() { continue; }
            match fs::remove_dir(entry.path()) {
                Ok(()) => {},
                Err(e) if matches!(e.kind(), std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound) => {},
                Err(e) => return Err(e).whatever_context("unable to clear empty reserved staging directory"),
            }
        }
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

#[cfg(test)]
mod staging_recovery_regressions {
    use super::*;
    #[test]
    fn rejected_leaf_removal_does_not_poison_later_admission() {
        let temp = tempfile::tempdir().unwrap();
        for relative in ["static/assets/generated/nested/bad.txt", "data/build_eips_search.toml/bad.txt"] {
            let path = temp.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "KEEP").unwrap();
            clear_empty_staged_reservations(temp.path()).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "KEEP");
            assert!(validate_inputs(temp.path()).is_err());
            fs::remove_file(path).unwrap();
            clear_empty_staged_reservations(temp.path()).unwrap();
            validate_inputs(temp.path()).unwrap();
        }
        fs::create_dir_all(temp.path().join("static/unrelated-empty")).unwrap();
        clear_empty_staged_reservations(temp.path()).unwrap();
        assert!(temp.path().join("static/unrelated-empty").is_dir());
    }
}

#[cfg(test)]
mod reviewer_route_regressions {
    use super::*;
    fn fixture(relative: &str, header: &str) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("content").join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("+++\n{header}\n+++\nBody\n")).unwrap();
        temp
    }
    #[test]
    fn effective_filename_date_bundle_and_slug_routes_are_reserved() {
        for (name, header) in [
            ("Search.md", ""), ("2026-09-22-pagefind.md", ""),
            ("2026-09-22T12:00:00Z-pagefind.md", ""), ("Search.fr.md", ""),
            ("bundle/index.md", "slug='pagefind'"), ("bundle/index.fr.md", "slug='Search'"),
            ("ordinary.md", "slug='pâgefind'"), ("ordinary.md", "slug='Search!'"),
            ("_index.md", "paginate_path='search'"),
        ] {
            let temp = fixture(name, header);
            let error = validate_inputs(temp.path()).unwrap_err().to_string();
            assert!(error.contains("reserved generated route"), "{name}: {error}");
        }
        for name in ["reference/Search.md", "ordinary.md", "index.md", "_index.md"] {
            validate_inputs(fixture(name, "").path()).unwrap();
        }
    }
    #[test]
    fn parent_escape_and_filesystem_prefixes_never_reach_renderer() {
        for header in ["aliases=['../../output/index.html']", "path='../../output/hijacked'",
            "slug='../bad'", "paginate_path='../bad'", "aliases=['C:/output/index.html']",
            r"aliases=['..\output\index.html']", "[taxonomies]\nTag=['../../bad']"] {
            let temp = fixture("ordinary.md", header);
            let error = validate_inputs(temp.path()).unwrap_err().to_string();
            assert!(error.contains("not confined"), "{header}: {error}");
        }
        let temp = fixture("ordinary.md", "aliases=['/old-route/','docs/archive.html']");
        validate_inputs(temp.path()).unwrap();
    }
    #[test]
    fn configured_normalization_and_destinations_cannot_bypass_ownership() {
        for config in ["taxonomies=[{name='Search'}]", "taxonomies=[{name='pâgefind'}]",
            "taxonomy_root='search'", "feed_filenames=['../../output/index.html']",
            "taxonomies=[{name='ordinary',paginate_path='../../bad'}]"] {
            let temp = tempfile::tempdir().unwrap();
            fs::create_dir(temp.path().join("config")).unwrap();
            fs::write(temp.path().join("config/zola.toml"), config).unwrap();
            assert!(validate_inputs(temp.path()).is_err(), "{config}");
        }
    }
}

#[cfg(test)]
mod whole_slug_regressions {
    use super::*;
    #[test]
    fn safe_mode_normalizes_whole_fields_not_route_segments() {
        for slug in ["se/arch", "hard/forks", "page/find"] {
            assert!(check_slug(Path::new(""), slug, Path::new("fixture")).is_err(), "{slug}");
            check_slug(Path::new("reference"), slug, Path::new("fixture")).unwrap();
        }
        for config in ["taxonomies=[{name='se/arch'}]\n[slugify]\ntaxonomies='safe'", "taxonomies=[{name='hard/forks'}]"] {
            let temp = tempfile::tempdir().unwrap();
            fs::create_dir(temp.path().join("config")).unwrap();
            fs::write(temp.path().join("config/zola.toml"), config).unwrap();
            assert!(validate_inputs(temp.path()).is_err());
        }
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("content")).unwrap();
        fs::write(temp.path().join("content/ordinary.md"), "+++\nslug='se/arch'\n+++\n").unwrap();
        assert!(validate_inputs(temp.path()).is_err());
    }
}
