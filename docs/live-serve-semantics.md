# Live-serve semantic generations

`serve` uses the same complete proposal catalog and network-upgrade builders as
initial preparation. This implements amended DEC-003; it does not introduce a
second metadata schema, migration path, or optional proposal-universe mode.

## Accepted input and startup

Initial preparation admits and transforms a complete raw merged source
snapshot. Before the first forced live reconciliation, `LiveSource` retains
that snapshot's raw tree/commit identity, full `ProposalCatalog`, full
`NetworkUpgradeIndex`, and hashes of the retained prepared proposal projections.
These values are initialized from the prepared raw clone, not from a possibly
newer active working tree. An edit in the preparation/watcher gap is therefore
compared with the actual prepared generation, including its memberships.

The active repository remains a tracked HEAD/index/working-tree source. New
untracked files are ignored until staged; index-only events and clean HEAD
changes are observed. Siblings remain the admitted startup snapshot. Existing
configuration/source-selector changes still require restarting serve.

## Candidate computation and publication

A coalesced source update is staged privately. Raw path, source-read,
reservation, catalog, network-upgrade and transformed-path validation happen
before publication. Every changed raw generation rebuilds the complete catalog
and aggregate index. There is no incrementally patched catalog or mutable
per-entry semantic cache.

Actual raw changes, changed membership projections and retained link/identity
changes determine which Markdown projections must be reconsidered. Inventory
changes may require reconsidering retained links, but do not authorize rewriting
all proposal files. The new projection hashes and generated aggregate bytes are
compared with accepted/current prepared input. Only changed leaves are published.

Generated front-matter maps have deterministic ordering. The staging commit
keeps the last accepted raw commit as a parent, so reconsidering an already-dirty
but unchanged proposal does not invent a new history-derived update timestamp.
The existing per-file history lookup remains; batching that lookup is not part
of this lifecycle change.

Regular files and symbolic-link leaves are replaced by rename from private
staging on the same build filesystem. Unchanged leaves keep their inode and
modification time. Temporary files stay outside Zola's watched prepared tree.
Directory/file transitions may require removals before replacement; neither
those transitions nor the overall multi-file publication are a website-wide
transaction.

The generator owns two live aggregate files: `static/assets/data/proposals.json`
and `content/hardforks/_index.md`. They are regenerated in private staging after
raw admission, byte-diffed for publication, and removed only from that private
staging area before its next raw admission. Authored collisions still fail the
normal reservation checks.

## Renderer escalation

Ordinary proposal body edits keep Zola's fast path. Listing-visible metadata
changes—including title, description, status, type, category, authors and
created date—request a coalesced full render. Authors and created date participate
in the semantic comparison even though they are not serialized in popover JSON.
Membership and relevant structural changes likewise request a full render so
listings, taxonomies, feeds, hardfork rows and badges converge.

The request is an atomic, content-changing bounded comment in the **mounted**
theme config. It does not edit the source theme or rely on mtime-only touching,
which Zola 0.22.1 ignores. Publication finishes before that request. Repeated
source events that describe the same accepted raw generation produce no writes
or repeated render requests. Filesystem events are coalesced by the existing
watcher before a candidate is computed. Pure non-Markdown bundle-asset writes
already cause Zola to render globally, so an asset-only add/rename does not add
a redundant explicit trigger. Structural deletion still requests a render when
there is no native content write to make the renderer observe it.

## Failure and recovery boundaries

A candidate rejected during private preprocessing/semantic validation leaves
accepted prepared input unchanged. A later valid source edit can recover.
Initial preparation/reconciliation failure aborts startup rather than claiming
a successfully running server.

An I/O failure during publication can leave a partial prepared tree. It is
reported explicitly as uncertain; the accepted semantic generation does not
advance. The watcher retries pending publication without requiring another
source edit. Pending prepared paths and render intent survive the failure, so
both a retry of the candidate and a reversion to accepted raw source repair
partially installed projections. Accepted semantic state advances only after
preprocessor validation, publication, and any required render request succeed.

That acceptance is **not** a promise that Zola has finished rendering, nor a
versioned HTTP snapshot. Zola errors remain visible in process diagnostics. A
page can temporarily be unavailable until a valid edit is rendered. The actual
renderer failure/recovery and coalescing behavior must be checked through the
process/HTTP acceptance, not inferred from unit tests alone.

Pagefind, corpus extraction, completed-build receipts and reusable build-output
promotion remain build-only. `serve` still uses its separate disposable output.
Standalone corpus export, deployment/selector promotion and broader timestamp
optimization are outside this phase.

## Evidence ownership

`src/live_semantics_tests.rs` covers accepted-state initialization, body-only
write counts, complete metadata and membership changes, rejected candidates,
unchanged-file preservation across add/delete, and a real partial I/O failure
followed by source reversion. `src/publication.rs` tests safe leaf replacement
and directory/file transitions. Coordination acceptance in `check-phase5.py`
exercises the real binary, complete WG source universe, production theme,
watcher, persistent Zola process, and HTTP-rendered surfaces.
