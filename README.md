build-eips
==========

`build-eips` is the local and CI build tool for Ethereum Improvement Proposal repositories. It prepares a multi-repo workspace with [EIPs], [ERCs] and the site theme, runs proposal validation, and coordinates the full site build pipeline.

## Prerequisites

`build-eips` requires a few runtime dependencies, available from wherever you get your software:

- git
- libgit2
- openssl
- [Zola](https://github.com/getzola/zola) 0.22.1[^1]

[^1]: The setup scripts can install or reuse the pinned Zola version for local workspace testing.

## Installation

### Pre-compiled Binaries

Pre-compiled binaries for Ubuntu, Windows, and macOS are available from [GitHub Releases].

[GitHub Releases]: https://github.com/ethereum/build-eips/releases

### From Source

If you're feeling particularly adventurous, you can install the latest version of `build-eips` like so:

```sh
cargo install --git https://github.com/ethereum/build-eips.git
```

[EIPs]: https://github.com/ethereum/EIPs/
[ERCs]: https://github.com/ethereum/ERCs/

### Local Testing

For direct manual testing, build and run the local binary explicitly:

```bash
cargo build
./target/debug/build-eips --help
./target/debug/build-eips -C ../EIPs check
```

```powershell
cargo build
.\target\debug\build-eips.exe --help
.\target\debug\build-eips.exe -C ..\EIPs check
```

For repeated manual testing, you can put the local debug binary first on `PATH` for the current shell:

```bash
export PATH="$PWD/target/debug:$PATH"
build-eips --help
build-eips -C ../EIPs check
```

```powershell
$env:Path = "$PWD\target\debug;$env:Path"
build-eips --help
build-eips -C ..\EIPs check
```

## Workspace Bootstrap

Use the contributor setup script when you are changing `build-eips` itself and need the full local multi-repo workspace that runs this checkout.

That workspace lets you test the proposal validation and site build pipeline against local EIPs/ERCs/theme checkouts without installing or reusing a released `build-eips`.

Linux and macOS:

```sh
./scripts/dev-setup
```

Windows PowerShell:

```powershell
.\scripts\dev-setup.ps1
```

The setup script builds the local debug binary, ensures the pinned Zola version is available, bootstraps the workspace with that binary, and runs `doctor`. It does not install or reuse a released `build-eips`.

The setup script selects its active proposal checkout through `ACTIVE_REPO_ROOT`; the selected checkout must contain a valid `Build.toml`. In the standard layout the active checkout is `EIPs/`. Set `ACTIVE_REPO_ROOT` when using another manifest-bearing proposal checkout.

After setup, the workspace has this layout:

```text
EIPs-project/
├── .build-eips.toml
├── WORKSPACE.md
├── .local-build/
├── EIPs/
├── ERCs/
├── theme/
├── preprocessor/
└── eipw/
```

## Workspace Reference

After bootstrapping, use the generated `WORKSPACE.md` in the workspace root for the full local command reference, workspace layout, configuration, source modes, build outputs, and troubleshooting notes.

The generated guide is sourced from [`src/workspace_doc.md`](src/workspace_doc.md). Update that file when changing workspace documentation.

Use `build-eips doctor` when the workspace does not behave as expected. It checks the active repo, sibling repos, theme checkout, local config, required tools, and generated workspace docs.

[`ethereum/EIPs`]: https://github.com/ethereum/EIPs/
[`ethereum/ERCs`]: https://github.com/ethereum/ERCs/

## Input trust and local file links

Build inputs must be self-contained: external source/static/theme link targets,
Git-metadata links, cycles and unresolved links are rejected before rendering.
Relative links to existing files/directories within the materialized input remain
supported. Git attribute configuration (`.gitattributes`) must be ordinary
files, not links or directories. Include external assets deliberately rather than linking a build to
unrelated host files. A local absolute link back to the original checkout is not
inside the generated input root.

This is not a sandbox for arbitrary untrusted templates or executables. Workspace
configuration, theme code and tools remain trusted; use an isolated environment
without secrets when running untrusted code. See `docs/audit/R06.md` for the exact
read/write/preview boundaries and limitations.

## Completed builds and preview

`build` renders and runs requested Pagefind/corpus stages privately. Only a
successful candidate replaces `<build-root>/output`. A small internal
`.build-eips-build.json` receipt travels inside that directory, recording its
completion, effective base URL/mount, source generation and tool identity.
A failed render or post-processing stage leaves the prior completed output alone.
Promotion uses two directory renames with rollback; a process crash between them
may leave no current output, which preview refuses rather than treating as complete.

`serve` uses `<build-root>/serve-output` and cannot clear or invalidate reusable
build output, including when content/theme static files change. The existing
single build-root writer lock still serializes build/check/serve operations.
`preview` serves only receipt-bearing completed output at its recorded mount
(`/`, `/EIPs/`, `/ERCs/`, etc.), irrespective of later workspace base-URL changes.
It does not rewrite HTML or regenerate search. Absolute HTML URLs retain the
build's original host, so build for the desired preview host when browsing locally.
During replacement, preview refuses incomplete or cross-generation reads with
HTTP 503. The receipt is not served as a public endpoint. Rebuild prerelease
output that lacks a receipt; there is no compatibility/migration path.

## Generated locations and site corpus

`src/reserved.rs` is the finite ownership list for generated search route/data,
proposal metadata, hardfork routes, Pagefind, `assets/generated/`, the internal
completed-build receipt and theme mount. Raw project/theme input, explicit native
Zola page paths/slugs/aliases and configured taxonomy/feed routes are checked
before generation or live admission. A collision is an error, including with
search/corpus disabled. Post-Zola Pagefind/corpus destinations must still be empty;
indexing no longer deletes output merely because it resembles an older index.

Normal `build --search-corpus` output is always
`assets/generated/search/corpus.json`; `[search.corpus]` has only `enabled` and
`format`. The removed `output` key is rejected with an actionable error, not
migrated. Standalone `corpus --output` is separately planned, not implemented here.
Ordinary authored files outside the reserved paths, including a file named
`search-corpus.json`, are not deleted as presumed legacy data.
