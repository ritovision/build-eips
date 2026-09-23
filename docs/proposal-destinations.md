# Proposal destinations and rendered search contracts

The preprocessor and theme form one pinned first-production pair. These generated
contracts have not shipped. There is no support for intermediate prerelease shapes.

## Destinations

`ProposalDestination` is a presentation projection, not proposal identity or an SEO
canonical URL. A `public_url` is always the current product's official fallback.
`local_route`, when present, is the logical `/<number>/` route without a deployment
prefix. Its absence means the proposal was intentionally omitted by targeted rendering.
Full builds include a local route for every admitted proposal. Targeted builds include
it only for selected proposals. Source category, not the active repository, selects
EIP versus ERC public policy.

Hardfork data carries `meta = { local_route?, public_url }` and member rows carry
`destination = { local_route?, public_url }`. Proposal membership front matter uses
the same `meta` shape. The paired theme resolves local routes with `get_url` exactly
once; an omitted target uses `public_url` directly. It never feeds a deployment-prefixed
path to `get_url`. Ordinary Markdown links to selected sources keep Zola source validation.
Zola treats queries on `@/file.md?query` as part of the source filename. For query-
bearing links, the preprocessor validates the source/asset and emits a relative
owned proposal/asset rendered route instead; the browser applies the deployment
prefix once. Query and fragment bytes are then appended unchanged. Encoded asset
source names use the same validated route path, classified after decoding once.
For TOML Markdown assets, use the admitted explicit `path`, not the filename-derived
route. A query-bearing or encoded link to an implicit TOML route is refused with an
actionable diagnostic instead of guessing renderer/config-dependent slug behavior.
Ordinary unencoded, query-free source links still use Zola's resolver, including
implicit TOML routing. This bounded refusal is not a live semantic lifecycle change.

Official page fallback is `https://eips.ethereum.org/EIPS/eip-N` or
`https://ercs.ethereum.org/ERCS/erc-N`. Official assets live under `/assets/eip-N/`
or `/assets/erc-N/` on the corresponding host, not under the WG `/<number>/assets/`
layout. Each asset path segment is encoded once. Static filenames are preserved;
ordinary Markdown assets use `.html`, and `README.md` (case-insensitive)/`index.md` use their containing
directory. Queries and fragments are appended once, unchanged. This is a routing
policy, not a promise that every unmerged proposal or asset exists publicly.

## Popovers and search

`proposals.json` schema 1 contains `schema_version`, `active_prefix`, and a proposal
map. Each record contains number, prefix, title, status, type, and optional description
and category. It has **no URL or destination fields**, and no authors/date/relationship
search projection. The catalog retains canonical validated semantics and raw source
body/provenance for their actual consumers, without a duplicate search-specific map.

Pagefind and the site corpus consume rendered HTML, not proposal JSON or another
catalog projection. The theme emits literal `key:value` hooks and attribute-value
`key[data-pagefind-value]` hooks for filters, metadata and sort values. Integration
acceptance compares these emitted hooks with actual corpus records and exercises the
Pagefind indexing/search fixtures. Popovers only recognize the current origin/base path
or explicitly allowed official EIP/ERC hosts.

Live semantic recomputation/convergence is a separate Phase 5 lifecycle contract.
This destination slice does not implement it. Standalone corpus export is separate.
