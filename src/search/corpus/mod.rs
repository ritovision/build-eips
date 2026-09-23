/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Rendered-HTML search corpus extraction.

mod chunks;
mod export;
mod extract;

pub(crate) use export::{export_corpus, ExportDestination};
mod output;
mod schema;
mod splitter;

#[cfg(test)]
mod tests;

use snafu::{ResultExt, Whatever};

use crate::{config::SearchCorpusFormat, search::normalized_base_path};

use super::{SearchCorpusRequest, SearchCorpusSummary, SITE_CORPUS_PATH};
use schema::{CorpusOutput, CORPUS_VERSION};

pub(super) fn reconcile_corpus(
    request: SearchCorpusRequest,
) -> Result<Option<SearchCorpusSummary>, Whatever> {
    let output_path = request
        .output_path
        .canonicalize()
        .with_whatever_context(|_| {
            format!(
                "unable to resolve rendered output directory `{}`",
                request.output_path.to_string_lossy()
            )
        })?;
    if !request.config.enabled {
        return Ok(None);
    }

    crate::reserved::ensure_absent(&output_path, SITE_CORPUS_PATH)?;
    let corpus = extract::extract_corpus(&output_path, &normalized_base_path(&request.base_url))?;
    let output = format_corpus(corpus, request.config.format, request.source);
    let documents = output.documents.len();
    let chunks = output.chunks.len();
    let output_file = output::write_corpus_file(&output_path, &output)?;

    Ok(Some(SearchCorpusSummary {
        documents,
        chunks,
        output_path: output_file,
    }))
}

fn format_corpus(
    mut corpus: schema::ExtractedCorpus,
    format: SearchCorpusFormat,
    source: crate::search::CorpusSource,
) -> CorpusOutput {
    match format {
        SearchCorpusFormat::Documents => corpus.chunks.clear(),
        SearchCorpusFormat::Chunks => corpus.documents.clear(),
        SearchCorpusFormat::DocumentsAndChunks => {}
    }
    CorpusOutput {
        version: CORPUS_VERSION,
        source,
        documents: corpus.documents,
        chunks: corpus.chunks,
    }
}
