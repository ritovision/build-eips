/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::{
    fs::File,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

use log::info;
use snafu::{ResultExt, Whatever};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::config::ServerBinding;

const INDEX_HTML: &str = "index.html";

pub fn serve(output_path: &Path, server_binding: &ServerBinding) -> Result<(), Whatever> {
    if !output_path.is_dir() {
        snafu::whatever!(
            "preview output directory `{}` is missing; run `build-eips build` for this profile first",
            output_path.to_string_lossy()
        );
    }

    let server = match Server::http((server_binding.interface, server_binding.port)) {
        Ok(server) => server,
        Err(error) => {
            snafu::whatever!("unable to bind preview server on {server_binding}: {error}")
        }
    };

    info!(
        "serving static preview from `{}` at http://{server_binding}/",
        output_path.to_string_lossy()
    );

    for request in server.incoming_requests() {
        handle_request(output_path, request)?;
    }

    Ok(())
}

fn handle_request(output_path: &Path, request: Request) -> Result<(), Whatever> {
    match *request.method() {
        Method::Get | Method::Head => {}
        _ => {
            request
                .respond(Response::empty(StatusCode(405)))
                .whatever_context("unable to send preview method error response")?;
            return Ok(());
        }
    }

    let Some(paths) = resolve_request_paths(output_path, request.url()) else {
        request
            .respond(Response::empty(StatusCode(400)))
            .whatever_context("unable to send preview bad request response")?;
        return Ok(());
    };

    let Some((path, file)) = open_preview_asset(output_path, paths)? else {
        request
            .respond(Response::empty(StatusCode(404)))
            .whatever_context("unable to send preview not found response")?;
        return Ok(());
    };

    let response = if let Some(value) = content_type(&path) {
        Response::from_file(file).with_header(content_type_header(value))
    } else {
        Response::from_file(file)
    };

    request
        .respond(response)
        .with_whatever_context(|e| format!("unable to send preview response: {e}"))?;

    Ok(())
}

fn open_preview_asset(
    output_path: &Path,
    paths: Vec<PathBuf>,
) -> Result<Option<(PathBuf, File)>, Whatever> {
    let root = output_path
        .canonicalize()
        .whatever_context("unable to resolve preview output root")?;
    for requested in paths {
        let path = match requested.canonicalize() {
            Ok(path) if path.starts_with(&root) => path,
            Ok(_) => continue, // A planted file or directory link is not an output asset.
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
            {
                continue
            }
            Err(error) => return Err(error).whatever_context("unable to resolve preview asset"),
        };
        // Do not open special files such as FIFOs, which can block the server.
        if !std::fs::metadata(&path)
            .whatever_context("unable to inspect preview asset")?
            .is_file()
        {
            continue;
        }
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
            {
                continue
            }
            Err(error) => return Err(error).whatever_context("unable to open preview asset"),
        };
        if file
            .metadata()
            .whatever_context("unable to inspect opened preview asset")?
            .is_file()
        {
            // Use the request's filename for MIME selection and diagnostics. The
            // opened file was resolved and confined above; canonical path syntax
            // (notably Windows verbatim prefixes) is not part of the URL API.
            return Ok(Some((requested, file)));
        }
    }
    Ok(None)
}

/// Decode exactly once, before filesystem validation. Encoded separators are
/// rejected rather than being allowed to introduce new path components.
fn decode_url_path(raw: &str) -> Option<String> {
    if !raw.starts_with('/') {
        return None;
    }
    let mut bytes = Vec::with_capacity(raw.len());
    let mut input = raw.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        let decoded = if byte == b'%' {
            let high = (input.next()? as char).to_digit(16)? as u8;
            let low = (input.next()? as char).to_digit(16)? as u8;
            let decoded = (high << 4) | low;
            if decoded == b'/' || decoded == b'\\' {
                return None;
            }
            decoded
        } else {
            byte
        };
        // Backslashes, drive/ADS colons and controls are not portable URL-path
        // filenames. Apply the same decision on Unix and Windows.
        if decoded == b'\\' || decoded == b':' || decoded < 0x20 || decoded == 0x7f {
            return None;
        }
        bytes.push(decoded);
    }
    String::from_utf8(bytes).ok()
}

fn resolve_request_paths(output_path: &Path, url: &str) -> Option<Vec<PathBuf>> {
    let raw_path = decode_url_path(url.split(['?', '#']).next().unwrap_or("/"))?;
    let mut resolved = output_path.to_path_buf();
    let mut saw_normal_component = false;

    for component in Path::new(raw_path.trim_start_matches('/')).components() {
        match component {
            Component::CurDir | Component::RootDir => {}
            Component::Normal(component) => {
                saw_normal_component = true;
                resolved.push(component);
            }
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }

    if raw_path.ends_with('/') || !saw_normal_component {
        return Some(vec![resolved.join(INDEX_HTML)]);
    }

    if resolved.extension().is_none() {
        return Some(vec![resolved.join(INDEX_HTML), resolved]);
    }

    Some(vec![resolved])
}

fn content_type(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("css") => Some("text/css; charset=utf-8"),
        Some("gif") => Some("image/gif"),
        Some("htm" | "html") => Some("text/html; charset=utf-8"),
        Some("ico") => Some("image/x-icon"),
        Some("jpeg" | "jpg") => Some("image/jpeg"),
        Some("js") => Some("application/javascript; charset=utf-8"),
        Some("json") => Some("application/json; charset=utf-8"),
        Some("mjs") => Some("application/javascript; charset=utf-8"),
        Some("png") => Some("image/png"),
        Some("svg") => Some("image/svg+xml"),
        Some("txt") => Some("text/plain; charset=utf-8"),
        Some("webp") => Some("image/webp"),
        Some("xml") => Some("application/xml; charset=utf-8"),
        _ => None,
    }
}

fn content_type_header(value: &str) -> Header {
    Header::from_bytes(b"Content-Type", value.as_bytes())
        .expect("hard-coded content-type headers must be valid")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{open_preview_asset, resolve_request_paths, INDEX_HTML};

    fn write_file(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn candidates(root: &Path, url: &str) -> Vec<PathBuf> {
        resolve_request_paths(root, url).unwrap()
    }

    fn selected_path(root: &Path, url: &str) -> Option<PathBuf> {
        open_preview_asset(root, candidates(root, url))
            .unwrap()
            .map(|(path, _file)| path)
    }

    #[test]
    fn slash_path_resolves_to_index_candidate() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        assert_eq!(
            candidates(root, "/foo/"),
            vec![root.join("foo").join(INDEX_HTML)]
        );
    }

    #[test]
    fn extensionless_path_prefers_index_then_file_candidate() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        assert_eq!(
            candidates(root, "/foo"),
            vec![root.join("foo").join(INDEX_HTML), root.join("foo")]
        );
    }

    #[test]
    fn extension_path_resolves_to_file_candidate() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        assert_eq!(candidates(root, "/foo.css"), vec![root.join("foo.css")]);
    }

    #[test]
    fn parent_traversal_is_rejected() {
        let temp = TempDir::new().unwrap();

        assert!(resolve_request_paths(temp.path(), "/../secret.txt").is_none());
    }

    #[test]
    fn extensionless_path_serves_index_when_present() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write_file(root, "foo/index.html", "index");

        assert_eq!(
            selected_path(root, "/foo"),
            Some(root.join("foo").join(INDEX_HTML))
        );
    }

    #[test]
    fn extensionless_path_serves_file_without_index() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write_file(root, "foo", "file");

        assert_eq!(selected_path(root, "/foo"), Some(root.join("foo")));
    }

    #[test]
    fn missing_path_returns_no_asset() {
        let temp = TempDir::new().unwrap();

        assert!(selected_path(temp.path(), "/missing").is_none());
    }
    #[test]
    fn encoded_names_are_decoded_once_and_unsafe_encodings_are_rejected() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write_file(root, "space name.txt", "space");
        write_file(root, "café.txt", "unicode");
        write_file(root, "literal%20.txt", "percent");
        for (url, name) in [
            ("/space%20name.txt?query=ignored", "space name.txt"),
            ("/caf%C3%A9.txt", "café.txt"),
            ("/literal%2520.txt", "literal%20.txt"),
        ] {
            assert_eq!(selected_path(root, url), Some(root.join(name)));
        }
        for url in [
            "/%2e%2e/secret",
            "/a/%2E%2E/secret",
            "/%2fetc/passwd",
            "/%5cwindows",
            "/a%00b",
            "/%",
            "/%2",
            "/%zz",
            "/%ff",
            "/C:/file",
            "/file:stream",
        ] {
            assert!(resolve_request_paths(root, url).is_none(), "{url}");
        }
    }
    #[cfg(unix)]
    #[test]
    fn preview_links_must_resolve_inside_output_including_directory_links() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("output");
        std::fs::create_dir(&root).unwrap();
        write_file(temp.path(), "outside/index.html", "PRIVATE");
        std::os::unix::fs::symlink(temp.path().join("outside"), root.join("outside")).unwrap();
        std::os::unix::fs::symlink(
            temp.path().join("outside/index.html"),
            root.join("outside.html"),
        )
        .unwrap();
        assert!(selected_path(&root, "/outside/").is_none());
        assert!(selected_path(&root, "/outside.html").is_none());
        write_file(&root, "real.txt", "PUBLIC");
        std::os::unix::fs::symlink("real.txt", root.join("inside.txt")).unwrap();
        assert_eq!(
            selected_path(&root, "/inside.txt"),
            Some(root.join("inside.txt"))
        );
    }
}
