/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Shared proposal catalog collection from prepared content sources.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use eipw_preamble::Preamble;
use serde::Serialize;
use snafu::{OptionExt, ResultExt, Whatever};
use toml_datetime::Datetime;

use crate::{
    markdown::{extract_authors, Author},
    proposal::{
        flat_proposal_number, path_component_proposal_number, public_site_for_preamble,
        OnlyRenderPlan, ProposalDestination, ProposalNumber, ProposalPublicSite,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(crate) enum ProposalCatalogPrefix {
    #[serde(rename = "EIP")]
    Eip,
    #[serde(rename = "ERC")]
    Erc,
}

impl ProposalCatalogPrefix {
    pub(crate) fn key(self, proposal_number: ProposalNumber) -> String {
        format!("{self}-{proposal_number}").to_ascii_lowercase()
    }
}

impl From<ProposalPublicSite> for ProposalCatalogPrefix {
    fn from(site: ProposalPublicSite) -> Self {
        match site {
            ProposalPublicSite::Eips => Self::Eip,
            ProposalPublicSite::Ercs => Self::Erc,
        }
    }
}

impl fmt::Display for ProposalCatalogPrefix {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eip => formatter.write_str("EIP"),
            Self::Erc => formatter.write_str("ERC"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProposalCatalogRecord {
    pub(crate) number: ProposalNumber,
    pub(crate) prefix: ProposalCatalogPrefix,
    pub(crate) title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    pub(crate) status: String,
    #[serde(rename = "type")]
    pub(crate) proposal_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) category: Option<String>,
    // Only destination consumers (hardforks) receive these fields. Popovers do not.
    #[serde(skip)]
    pub(crate) destination: ProposalDestination,
    #[allow(dead_code)]
    #[serde(skip)]
    pub(crate) authors: Vec<Author>,
    #[allow(dead_code)]
    #[serde(skip)]
    pub(crate) created: Option<Datetime>,
    #[allow(dead_code)]
    #[serde(skip)]
    relationships: ProposalCatalogRelationships,
}

// Canonical parsed relationships are independent of any search projection.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ProposalCatalogRelationships {
    requires: Vec<ProposalNumber>,
    replaces: Vec<ProposalNumber>,
    superseded_by: Vec<ProposalNumber>,
    discussions_to: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ProposalCatalog {
    records: BTreeMap<String, ProposalCatalogRecord>,
    source_documents: BTreeMap<String, ProposalCatalogSourceDocument>,
}

impl ProposalCatalog {
    pub(crate) fn records(&self) -> &BTreeMap<String, ProposalCatalogRecord> {
        &self.records
    }

    pub(crate) fn source_documents(&self) -> impl Iterator<Item = &ProposalCatalogSourceDocument> {
        self.source_documents.values()
    }

    pub(crate) fn source_document(
        &self,
        prefix: ProposalCatalogPrefix,
        proposal_number: ProposalNumber,
    ) -> Option<&ProposalCatalogSourceDocument> {
        self.source_documents.get(&prefix.key(proposal_number))
    }

    #[allow(dead_code)]
    pub(crate) fn into_records(self) -> BTreeMap<String, ProposalCatalogRecord> {
        self.records
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProposalCatalogSourceDocument {
    number: ProposalNumber,
    prefix: ProposalCatalogPrefix,
    source_path: PathBuf,
    preamble_fields: Vec<(String, String)>,
    body: String,
}

impl ProposalCatalogSourceDocument {
    pub(crate) fn number(&self) -> ProposalNumber {
        self.number
    }

    pub(crate) fn prefix(&self) -> ProposalCatalogPrefix {
        self.prefix
    }

    pub(crate) fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub(crate) fn field(&self, name: &str) -> Option<&str> {
        self.preamble_fields
            .iter()
            .find(|(field_name, _)| field_name == name)
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn created(&self) -> Option<&str> {
        self.field("created")
    }

    pub(crate) fn network_upgrade(&self) -> Option<&str> {
        self.field("network-upgrade")
    }

    pub(crate) fn proposal_type(&self) -> Option<&str> {
        self.field("type")
    }

    pub(crate) fn body(&self) -> &str {
        &self.body
    }
}

#[derive(Debug)]
struct CollectedProposalCatalogRecord {
    key: String,
    source_path: PathBuf,
    record: ProposalCatalogRecord,
    source_document: ProposalCatalogSourceDocument,
}

#[derive(Debug)]
struct ProposalCatalogFields {
    title: String,
    description: Option<String>,
    status: String,
    proposal_type: String,
    category: Option<String>,
    authors: Vec<Author>,
    created: Option<Datetime>,
    relationships: ProposalCatalogRelationships,
}

impl ProposalCatalogFields {
    fn parse(
        markdown_path: &Path,
        proposal_number: ProposalNumber,
        preamble: &Preamble<'_>,
    ) -> Result<Self, Whatever> {
        validate_preamble_proposal_number(markdown_path, proposal_number, preamble)?;

        Ok(Self {
            title: required_catalog_field(markdown_path, preamble, "title")?,
            description: optional_catalog_field(preamble, "description"),
            status: required_catalog_field(markdown_path, preamble, "status")?,
            proposal_type: required_catalog_field(markdown_path, preamble, "type")?,
            category: optional_catalog_field(preamble, "category"),
            authors: catalog_authors(markdown_path, preamble)?,
            created: optional_catalog_datetime_field(markdown_path, preamble, "created")?,
            relationships: ProposalCatalogRelationships {
                requires: optional_catalog_number_list(markdown_path, preamble, "requires")?,
                replaces: optional_catalog_number_list(markdown_path, preamble, "replaces")?,
                superseded_by: optional_catalog_number_list(
                    markdown_path,
                    preamble,
                    "superseded-by",
                )?,
                discussions_to: optional_catalog_field(preamble, "discussions-to"),
            },
        })
    }
}

struct ContentRootEntry {
    entry_path: PathBuf,
    file_type: std::fs::FileType,
}

pub(crate) fn collect_proposal_catalog(
    content_root: &Path,
    only_plan: Option<&OnlyRenderPlan>,
) -> Result<ProposalCatalog, Whatever> {
    let mut records = BTreeMap::<String, CollectedProposalCatalogRecord>::new();

    for entry in sorted_content_root_entries(content_root)? {
        if entry.file_type.is_file() {
            let Some(proposal_number) = flat_proposal_number(&entry.entry_path) else {
                continue;
            };
            insert_collected_proposal_catalog_record(
                &mut records,
                collect_proposal_catalog_from_path(
                    content_root,
                    proposal_number,
                    &entry.entry_path,
                    only_plan,
                )?,
            )?;
        } else if entry.file_type.is_dir() {
            let Some(proposal_number) =
                path_component_proposal_number(entry.entry_path.file_name())
            else {
                continue;
            };
            let index_path = entry.entry_path.join("index.md");
            match std::fs::read_to_string(&index_path) {
                Ok(contents) => {
                    insert_collected_proposal_catalog_record(
                        &mut records,
                        collect_proposal_catalog_from_contents(
                            content_root,
                            proposal_number,
                            &index_path,
                            &contents,
                            only_plan,
                        )?,
                    )?;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => {
                    snafu::whatever!(
                        "unable to read proposal markdown `{}`: {error}",
                        index_path.to_string_lossy()
                    );
                }
            }
        }
    }

    let mut source_documents = BTreeMap::new();
    let records = records
        .into_iter()
        .map(|(key, metadata)| {
            source_documents.insert(key.clone(), metadata.source_document);
            (key, metadata.record)
        })
        .collect();

    Ok(ProposalCatalog {
        records,
        source_documents,
    })
}

fn sorted_content_root_entries(content_root: &Path) -> Result<Vec<ContentRootEntry>, Whatever> {
    let entries = std::fs::read_dir(content_root).with_whatever_context(|_| {
        format!(
            "unable to read materialized content directory `{}` for proposal metadata",
            content_root.to_string_lossy()
        )
    })?;

    let mut entries = entries
        .map(|entry| -> Result<ContentRootEntry, Whatever> {
            let entry = entry.with_whatever_context(|_| {
                format!(
                    "unable to read materialized content directory entry in `{}` for proposal metadata",
                    content_root.to_string_lossy()
                )
            })?;
            let entry_path = entry.path();
            let file_type = entry.file_type().with_whatever_context(|_| {
                format!(
                    "unable to inspect materialized content path `{}` for proposal metadata",
                    entry_path.to_string_lossy()
                )
            })?;

            Ok(ContentRootEntry {
                entry_path,
                file_type,
            })
        })
        .collect::<Result<Vec<_>, Whatever>>()?;
    entries.sort_by(|left, right| left.entry_path.cmp(&right.entry_path));
    Ok(entries)
}

fn collect_proposal_catalog_from_path(
    content_root: &Path,
    proposal_number: ProposalNumber,
    markdown_path: &Path,
    only_plan: Option<&OnlyRenderPlan>,
) -> Result<CollectedProposalCatalogRecord, Whatever> {
    let contents = std::fs::read_to_string(markdown_path).with_whatever_context(|_| {
        format!(
            "unable to read proposal markdown `{}`",
            markdown_path.to_string_lossy()
        )
    })?;
    collect_proposal_catalog_from_contents(
        content_root,
        proposal_number,
        markdown_path,
        &contents,
        only_plan,
    )
}

fn collect_proposal_catalog_from_contents(
    content_root: &Path,
    proposal_number: ProposalNumber,
    markdown_path: &Path,
    contents: &str,
    only_plan: Option<&OnlyRenderPlan>,
) -> Result<CollectedProposalCatalogRecord, Whatever> {
    markdown_path
        .strip_prefix(content_root)
        .with_whatever_context(|_| {
            format!(
                "proposal markdown `{}` is outside content root `{}`",
                markdown_path.to_string_lossy(),
                content_root.to_string_lossy()
            )
        })?;

    let (preamble, body) = split_proposal_source(markdown_path, contents)?;
    let site = public_site_for_preamble(&preamble);
    let prefix = ProposalCatalogPrefix::from(site);
    let fields = ProposalCatalogFields::parse(markdown_path, proposal_number, &preamble)?;
    let key = prefix.key(proposal_number);
    let source_document = ProposalCatalogSourceDocument {
        number: proposal_number,
        prefix,
        source_path: markdown_path.to_path_buf(),
        preamble_fields: preamble
            .fields()
            .map(|field| (field.name().to_owned(), field.value().trim().to_owned()))
            .collect(),
        body: body.to_owned(),
    };

    let record = ProposalCatalogRecord {
        number: proposal_number,
        prefix,
        title: fields.title,
        description: fields.description,
        status: fields.status,
        proposal_type: fields.proposal_type,
        category: fields.category,
        destination: ProposalDestination::new(
            proposal_number,
            site,
            only_plan.is_none_or(|plan| plan.is_selected_number(proposal_number)),
        ),
        authors: fields.authors,
        created: fields.created,
        relationships: fields.relationships,
    };

    Ok(CollectedProposalCatalogRecord {
        key,
        source_path: markdown_path.to_path_buf(),
        record,
        source_document,
    })
}

fn split_proposal_source<'a>(
    markdown_path: &Path,
    contents: &'a str,
) -> Result<(Preamble<'a>, &'a str), Whatever> {
    let path_lossy = markdown_path.to_string_lossy();
    let (preamble, body) = Preamble::split(contents)
        .with_whatever_context(|_| format!("couldn't split preamble for `{path_lossy}`"))?;
    let preamble = Preamble::parse(None, preamble)
        .ok()
        .with_whatever_context(|| format!("couldn't parse preamble in `{path_lossy}`"))?;

    Ok((preamble, body))
}

fn insert_collected_proposal_catalog_record(
    records: &mut BTreeMap<String, CollectedProposalCatalogRecord>,
    metadata: CollectedProposalCatalogRecord,
) -> Result<(), Whatever> {
    if let Some(existing) = records.get(&metadata.key) {
        snafu::whatever!(
            "duplicate proposal metadata key `{}` from `{}` and `{}`",
            metadata.key,
            existing.source_path.to_string_lossy(),
            metadata.source_path.to_string_lossy()
        );
    }

    // EIP and ERC metadata keys differ, but both render at /N/. The admitted
    // universe must be unambiguous even when a full build has no OnlyRenderPlan.
    let other_prefix = match metadata.record.prefix {
        ProposalCatalogPrefix::Eip => ProposalCatalogPrefix::Erc,
        ProposalCatalogPrefix::Erc => ProposalCatalogPrefix::Eip,
    };
    if let Some(existing) = records.get(&other_prefix.key(metadata.record.number)) {
        snafu::whatever!(
            "proposal route collision at `/{}/`: `{}` from `{}` conflicts with `{}` from `{}`; proposal numbers must be unique in the merged render universe",
            metadata.record.number,
            existing.key,
            existing.source_path.to_string_lossy(),
            metadata.key,
            metadata.source_path.to_string_lossy()
        );
    }

    records.insert(metadata.key.clone(), metadata);
    Ok(())
}

fn required_catalog_field(
    markdown_path: &Path,
    preamble: &Preamble<'_>,
    field_name: &str,
) -> Result<String, Whatever> {
    let Some(field) = preamble.by_name(field_name) else {
        snafu::whatever!(
            "missing required proposal metadata field `{field_name}` in `{}`",
            markdown_path.to_string_lossy()
        );
    };
    let value = field.value().trim();
    if value.is_empty() {
        snafu::whatever!(
            "missing required proposal metadata field `{field_name}` in `{}`",
            markdown_path.to_string_lossy()
        );
    }

    Ok(value.to_owned())
}

fn optional_catalog_field(preamble: &Preamble<'_>, field_name: &str) -> Option<String> {
    preamble
        .by_name(field_name)
        .map(|field| field.value().trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn optional_catalog_datetime_field(
    markdown_path: &Path,
    preamble: &Preamble<'_>,
    field_name: &str,
) -> Result<Option<Datetime>, Whatever> {
    optional_catalog_field(preamble, field_name)
        .map(|value| {
            value.parse::<Datetime>().with_whatever_context(|_| {
                format!(
                    "couldn't parse {field_name} in `{}`",
                    markdown_path.to_string_lossy()
                )
            })
        })
        .transpose()
}

fn catalog_authors(markdown_path: &Path, preamble: &Preamble<'_>) -> Result<Vec<Author>, Whatever> {
    optional_catalog_field(preamble, "author")
        .map(|value| {
            extract_authors(&value).with_whatever_context(|_| {
                format!(
                    "couldn't parse author in `{}`",
                    markdown_path.to_string_lossy()
                )
            })
        })
        .unwrap_or_else(|| Ok(Vec::new()))
}

fn optional_catalog_number_list(
    markdown_path: &Path,
    preamble: &Preamble<'_>,
    field_name: &str,
) -> Result<Vec<ProposalNumber>, Whatever> {
    optional_catalog_field(preamble, field_name)
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .map(|item| {
                    let number = item.parse::<u32>().with_whatever_context(|_| {
                        format!(
                            "couldn't parse {field_name} in `{}`",
                            markdown_path.to_string_lossy()
                        )
                    })?;
                    let Ok(proposal_number) = ProposalNumber::from_u32(number) else {
                        snafu::whatever!(
                            "proposal number in `{field_name}` in `{}` must be positive",
                            markdown_path.to_string_lossy()
                        )
                    };
                    Ok(proposal_number)
                })
                .collect()
        })
        .unwrap_or_else(|| Ok(Vec::new()))
}

fn validate_preamble_proposal_number(
    markdown_path: &Path,
    path_proposal_number: ProposalNumber,
    preamble: &Preamble<'_>,
) -> Result<(), Whatever> {
    for field in preamble
        .fields()
        .filter(|field| matches!(field.name(), "eip" | "number"))
    {
        let field_name = field.name();
        let field_value = field.value().trim();
        let parsed = field_value.parse::<u32>().with_whatever_context(|_| {
            format!(
                "couldn't parse proposal number field `{field_name}` in `{}`",
                markdown_path.to_string_lossy()
            )
        })?;
        let Ok(preamble_proposal_number) = ProposalNumber::from_u32(parsed) else {
            snafu::whatever!(
                "proposal number field `{field_name}` in `{}` must be positive",
                markdown_path.to_string_lossy()
            );
        };

        if preamble_proposal_number != path_proposal_number {
            snafu::whatever!(
                "proposal metadata number mismatch in `{}`: path indicates `{path_proposal_number}`, but `{field_name}` contains `{preamble_proposal_number}`",
                markdown_path.to_string_lossy()
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;
    use toml_datetime::Datetime;

    use super::{
        collect_proposal_catalog, ProposalCatalogPrefix, ProposalCatalogRelationships,
    };
    use crate::proposal::{OnlyRenderPlan, ProposalNumber};

    fn number(value: u32) -> ProposalNumber {
        ProposalNumber::from_u32(value).unwrap()
    }

    fn write_file(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn catalog_proposal_markdown(
        number: u32,
        title: &str,
        category: Option<&str>,
        description: Option<&str>,
    ) -> String {
        let description = description
            .map(|description| format!("description: {description}\n"))
            .unwrap_or_default();
        let category = category
            .map(|category| format!("category: {category}\n"))
            .unwrap_or_default();
        format!(
            "---\neip: {number}\ntitle: {title}\n{description}status: Final\ntype: Standards Track\n{category}---\nBody\n"
        )
    }

    fn catalog_proposal_markdown_with_extra(
        number: u32,
        title: &str,
        category: Option<&str>,
        description: Option<&str>,
        extra: &str,
    ) -> String {
        let description = description
            .map(|description| format!("description: {description}\n"))
            .unwrap_or_default();
        let category = category
            .map(|category| format!("category: {category}\n"))
            .unwrap_or_default();
        format!(
            "---\neip: {number}\ntitle: {title}\n{description}status: Final\ntype: Standards Track\n{category}{extra}---\nBody\n"
        )
    }

    #[test]
    fn proposal_catalog_collects_flat_and_directory_proposals() {
        let temp = TempDir::new().unwrap();
        write_file(
            temp.path(),
            "020.md",
            &catalog_proposal_markdown(20, "Flat Proposal", None, None),
        );
        write_file(
            temp.path(),
            "21/index.md",
            &catalog_proposal_markdown(21, "Directory Proposal", Some("ERC"), Some("Desc")),
        );

        let catalog = collect_proposal_catalog(temp.path(), None).unwrap();
        let records = catalog.into_records();

        assert_eq!(records.len(), 2);
        assert_eq!(records["eip-20"].number, number(20));
        assert_eq!(records["eip-20"].prefix, ProposalCatalogPrefix::Eip);
        assert_eq!(records["eip-20"].title, "Flat Proposal");
        assert_eq!(records["eip-20"].destination.local_route.as_deref(), Some("/20/"));
        assert_eq!(records["erc-21"].number, number(21));
        assert_eq!(records["erc-21"].prefix, ProposalCatalogPrefix::Erc);
        assert_eq!(records["erc-21"].description.as_deref(), Some("Desc"));
    }

    #[test]
    fn proposal_catalog_reports_malformed_preambles() {
        let temp = TempDir::new().unwrap();
        write_file(temp.path(), "001.md", "not front matter\n");

        let error = collect_proposal_catalog(temp.path(), None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("couldn't split preamble"));
        assert!(error.contains("001.md"));
    }

    #[test]
    fn proposal_catalog_reports_path_preamble_number_mismatch() {
        let temp = TempDir::new().unwrap();
        write_file(
            temp.path(),
            "001.md",
            "---\neip: 2\ntitle: Proposal 1\nstatus: Final\ntype: Standards Track\n---\nBody\n",
        );

        let error = collect_proposal_catalog(temp.path(), None)
            .unwrap_err()
            .to_string();

        assert!(error.contains("proposal metadata number mismatch"));
        assert!(error.contains("001.md"));
        assert!(error.contains("path indicates `1`"));
        assert!(error.contains("`eip` contains `2`"));
    }

    #[test]
    fn proposal_catalog_collects_independently_of_json_writing() {
        let temp = TempDir::new().unwrap();
        write_file(
            temp.path(),
            "001.md",
            &catalog_proposal_markdown(1, "Proposal 1", None, None),
        );

        let catalog = collect_proposal_catalog(temp.path(), None).unwrap();
        let records = catalog.into_records();

        assert_eq!(records["eip-1"].status, "Final");
        assert!(!temp
            .path()
            .join("static/assets/data/proposals.json")
            .exists());
    }

    #[test]
    fn proposal_catalog_exposes_source_documents_separately_from_records() {
        let temp = TempDir::new().unwrap();
        write_file(
            temp.path(),
            "7773.md",
            "---\neip: 7773\ntitle: Hardfork Meta\nstatus: Draft\ntype: Meta\ncreated: 2024-09-26\nnetwork-upgrade: Glamsterdam\n---\nBody\n",
        );

        let catalog = collect_proposal_catalog(temp.path(), None).unwrap();
        let records = catalog.records();
        let document = catalog.source_documents().next().unwrap();

        assert_eq!(records["eip-7773"].title, "Hardfork Meta");
        assert_eq!(document.number(), number(7773));
        assert_eq!(document.prefix(), ProposalCatalogPrefix::Eip);
        assert_eq!(document.created(), Some("2024-09-26"));
        assert_eq!(document.network_upgrade(), Some("Glamsterdam"));
        assert_eq!(document.body(), "Body\n");
    }

    #[test]
    fn proposal_catalog_retains_canonical_semantics_without_search_projection() {
        let temp = TempDir::new().unwrap();
        write_file(temp.path(), "1559/index.md", &catalog_proposal_markdown_with_extra(
            1559, "Fee Market Change", Some("Core"), Some("Description"),
            "author: Alice <alice@example.com>, Bob (@bob), Carol (@carol) <carol@example.com>, Dan\ncreated: 2021-04-13\nrequires: 1, 2\nreplaces: 3\nsuperseded-by: 4\ndiscussions-to: https://example.test/discuss\n",
        ));
        let catalog = collect_proposal_catalog(temp.path(), None).unwrap();
        let record = &catalog.records()["eip-1559"];
        assert_eq!(record.authors.len(), 4);
        assert_eq!(record.authors[0].email.as_deref(), Some("alice@example.com"));
        assert_eq!(record.authors[1].github.as_deref(), Some("bob"));
        assert_eq!(record.authors[2].github.as_deref(), Some("carol"));
        assert_eq!(record.authors[2].email.as_deref(), Some("carol@example.com"));
        assert_eq!(record.authors[3].name, "Dan");
        assert_eq!(record.created, Some("2021-04-13".parse::<Datetime>().unwrap()));
        assert_eq!(record.relationships.requires, vec![number(1), number(2)]);
        assert_eq!(record.relationships.replaces, vec![number(3)]);
        assert_eq!(record.relationships.superseded_by, vec![number(4)]);
        assert_eq!(record.relationships.discussions_to.as_deref(), Some("https://example.test/discuss"));
        let value = serde_json::to_value(record).unwrap();
        let keys = value.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(keys, vec!["category", "description", "number", "prefix", "status", "title", "type"]);
        let document = catalog.source_documents().next().unwrap();
        assert_eq!(document.body(), "Body\n");
        assert!(document.source_path().ends_with("1559/index.md"));
    }

    #[test]
    fn proposal_catalog_canonical_optional_fields_and_validation() {
        let temp = TempDir::new().unwrap();
        write_file(temp.path(), "001.md", &catalog_proposal_markdown(1, "Minimal", None, None));
        let catalog = collect_proposal_catalog(temp.path(), None).unwrap();
        let record = &catalog.records()["eip-1"];
        assert!(record.authors.is_empty());
        assert_eq!(record.created, None);
        assert_eq!(record.relationships, ProposalCatalogRelationships::default());
        for extra in ["created: not-a-date\n", "requires: not-a-number\n", "replaces: 0\n", "superseded-by: bad\n"] {
            write_file(temp.path(), "001.md", &catalog_proposal_markdown_with_extra(1, "Invalid", None, None, extra));
            assert!(collect_proposal_catalog(temp.path(), None).is_err(), "{extra}");
        }
    }

    #[test]
    fn proposal_catalog_applies_targeted_url_policy() {
        let temp = TempDir::new().unwrap();
        write_file(
            temp.path(),
            "020.md",
            &catalog_proposal_markdown(20, "Proposal 20", None, None),
        );
        write_file(
            temp.path(),
            "021.md",
            &catalog_proposal_markdown(21, "Proposal 21", None, None),
        );
        write_file(
            temp.path(),
            "022.md",
            &catalog_proposal_markdown(22, "Proposal 22", Some("ERC"), None),
        );
        let plan = OnlyRenderPlan::build(temp.path(), [number(20)].into_iter().collect()).unwrap();

        let catalog = collect_proposal_catalog(temp.path(), Some(&plan)).unwrap();
        let records = catalog.into_records();

        assert_eq!(records["eip-21"].destination.local_route, None);
        assert_eq!(records["erc-22"].destination.local_route, None);
        assert_eq!(records["eip-20"].destination.public_url, "https://eips.ethereum.org/EIPS/eip-20");
        assert_eq!(records["eip-20"].destination.local_route.as_deref(), Some("/20/"));
        assert_eq!(
            records["eip-21"].destination.public_url,
            "https://eips.ethereum.org/EIPS/eip-21"
        );
        assert_eq!(
            records["erc-22"].destination.public_url,
            "https://ercs.ethereum.org/ERCS/erc-22"
        );
    }
}
