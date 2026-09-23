# Standalone corpus export

This is the first-production Phase 6 contract. No earlier prerelease export or
corpus schema is supported through compatibility adapters.

```text
build-eips corpus --output PATH [--format documents|chunks|documents-and-chunks]
                  [--only NUMBER ...] [--base-url URL] [--clean] [--force]
```

The root-level `-C`, `--build-root` and `--remote-siblings` controls are supported.
For example: `build-eips --remote-siblings corpus --output proposals.json --clean`.
The default format is `documents-and-chunks`; an unrequested record view is an
empty array, not a second schema. Normal site generation still uses
`assets/generated/search/corpus.json` and has no configurable destination.

## Source and URL context

An export without explicit `--only` uses the complete admitted merged proposal
universe, regardless of workspace `[render].only`. Explicit selectors are positive
proposal numbers, sorted/deduplicated, checked against the merged inventory, and
recorded as `source.only`. A null selection means full coverage. Selection limits
rendered proposal pages, not source admission or the hardfork invariant. As in a
targeted build, omitted proposal/asset links use the established public fallback.
Ancillary pages carrying the rendered corpus hooks may also be included.

Source semantics reuse normal build resolution: the active local checkout's
tracked working-tree content is used by default; `--clean` requires that active
checkout to be clean and materializes its commit. Siblings use their committed
snapshots (local workspace repositories by default, declared manifest repositories
with `--remote-siblings`, including bare `file://` Git remotes). The local theme
remains editable in either source mode.
Untracked active files and dirty sibling edits are not automatically admitted.
Unlike the developer build subset control, explicit export selection works in
clean and remote-sibling modes too.

Effective base URL precedence is CLI `--base-url`, workspace `[site].base_url`, then
the active manifest location `base-url`. Export requires an absolute HTTP(S) URL
without credentials, query or fragment. Root and nested deployment paths are
supported, with or without a final slash. Standalone document and chunk `url`
fields are absolute and resolve the deployment prefix exactly once. Filesystem
route names are URL-encoded once, and heading anchors are retained. The normal
site corpus keeps deployment-root-relative URLs, with the same source context.

Both publication forms use one schema:

- `version`: the first-production schema version, currently 1;
- `source`: `product`, `base_url`, `source_mode` (`dirty` or `clean`), `source_head`,
  `materialized_head`, `merged_source_tree`, and `only` (null or selected numbers);
- `documents` and `chunks`: the existing rendered-HTML records, with the
  established identity, text, Pagefind filter/meta/sort and chunk splitter fields.

Source identity describes the admitted input, not a promise that the theme or
local configuration is immutable across separate exports. No local filesystem
paths or credentials are embedded in source metadata. Proposal document identities
remain semantic (for example `proposal:EIP-1559`); URL-fallback identities retain
logical deployment paths. A chunks-only export may reference document identities
without including a document record array.

## Isolation

The command resolves execution, validates the destination, then uses a unique
`.corpus-*` temporary directory below the selected build root for preparation and
Zola rendering. It calls the existing rendered `data-pagefind-*` extractor, not a
parallel catalog search projection. It never calls reusable `Prepared::build`,
Pagefind indexing, site-corpus reconciliation, completed-build receipt generation,
or reusable output promotion. The generated private search route is disabled even
when normal workspace Pagefind is enabled. Normal site-corpus enablement and
format do not govern the explicit export command.

No prior build is needed. `repo`, `output`, `serve-output` and `.lock` at the shared
build-root level are not used or replaced, and no shared-build lock is acquired.
Private render/preparation state is dropped on success and ordinary failure. A
process forcibly killed without unwinding can leave disposable scratch; this is
not a crash-recovery service. A build-root parent newly created by the command may
remain empty after cleanup.

## Destination and publication

`--output` is relative to the invocation working directory, not the `-C` source
root. The parent must already exist and every supplied parent must be a real
accessible directory, not a symlink or reparse point. The destination must be
absent or a regular file; directories, FIFOs, device nodes and links (including
dangling links) are refused. Active tracked input/content/Git metadata, the local
theme, selected local siblings, loaded configuration and the selected build root
are protected from export writes. An untracked export directly under the active
checkout is permitted.

An existing regular export requires `--force`. After successful rendering and
extraction, serialization uses a fresh temporary in the destination's own directory.
The complete JSON and final newline are flushed before publication. No-force
publication is no-clobber, including when a destination appears during the run.
Forced publication atomically replaces the regular leaf without truncating or
unlinking the previous file first. Preparation, rendering, extraction,
serialization, flushing and pre-publication safety failures leave the previous
export unchanged. Temporary files are removed on normal error unwinding.

This does not claim power-loss durability or a sandbox against a same-user process
concurrently replacing parent directories between checks and filesystem calls.
Trusted configuration/theme/executable requirements and source-read reservations
remain those of the existing build system.

## Scope boundary

Phase 6 does not batch Git history, optimize documents-only extraction, add a
provenance database, generalize proposal families, change the live-server
consistency contract, promote selectors, or perform releases/deployments.
