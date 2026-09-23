/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Text boundaries accept Git for Windows line endings without rewriting sources.
use eipw_preamble::SplitError;
use lazy_static::lazy_static;
use regex::Regex;

/// Like eipw's splitter, with LF or CRLF delimiter lines. Both returned slices
/// borrow the input; Markdown body bytes (including lone CRs) are untouched.
pub(crate) fn split_preamble(text: &str) -> Result<(&str, &str), SplitError> {
    lazy_static! {
        static ref MARKER: Regex = Regex::new(r"(^|\r?\n)---(\r?\n|$)").unwrap();
    }
    let mut markers = MARKER.find_iter(text);
    let start = markers.next().ok_or(SplitError::MissingStart)?;
    let end = markers.next().ok_or(SplitError::MissingEnd)?;
    if start.start() != 0 {
        return Err(SplitError::LeadingGarbage);
    }
    Ok((&text[start.end()..end.start()], &text[end.end()..]))
}

/// eipw's lint parser is LF-only. Normalize its in-memory input, not files or
/// assets. Applying the same adapter to referenced proposals preserves linting.
#[derive(Debug)]
pub(crate) struct LintFetch;
impl eipw_lint::fetch::Fetch for LintFetch {
    fn fetch(&self, path: std::path::PathBuf) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, std::io::Error>>>> {
        Box::pin(async move { Ok(tokio::fs::read_to_string(path).await?.replace("\r\n", "\n")) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lf_crlf_and_mixed_delimiters_preserve_body_bytes() {
        for first in ["\n", "\r\n"] {
            for last in ["\n", "\r\n", ""] {
                let body = if last.is_empty() { "" } else { "\r\nCode\rData\r\n---\r\n" };
                let text = format!("---{first}eip: 1\r\ntitle: Café{first}---{last}{body}");
                let (header, actual) = split_preamble(&text).unwrap();
                assert_eq!(actual.as_bytes(), body.as_bytes());
                let parsed = eipw_preamble::Preamble::parse(None, header).unwrap();
                assert_eq!(parsed.by_name("title").unwrap().value().trim(), "Café");
            }
        }
    }
    #[test]
    fn invalid_delimiters_and_leading_garbage_still_fail() {
        for text in ["---\reip: 1\r---\r", "--- \neip: 1\n---\n", "---\neip: 1\n", "x\n---\neip: 1\n---\n"] {
            assert!(split_preamble(text).is_err(), "{text:?}");
        }
    }
    #[tokio::test]
    async fn lint_adapter_keeps_source_unchanged_and_preserves_lone_cr() {
        use eipw_lint::fetch::Fetch;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("proposal.md");
        let original = "---\r\neip: 1\r\n---\r\nBody\rdata\r\n";
        std::fs::write(&path, original).unwrap();
        assert_eq!(LintFetch.fetch(path.clone()).await.unwrap(), original.replace("\r\n", "\n"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }
}
