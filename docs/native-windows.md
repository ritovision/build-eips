# Native Windows development

The preprocessor uses native filesystem paths for filesystem operations and forward-slash strings for site routes, Markdown destinations, and aliases. Canonical comparison operands must both be resolved; do not remove the Windows extended-length prefix globally or relax containment checks.

## Tools and local execution

Use a supported MSVC Rust toolchain (the compatibility validation uses Rust 1.91.1), Git for Windows, and Zola 0.22.1. Run checkout creation and builds under the same Windows account. Supply the directories containing Git, Zola, and build-eips explicitly in the child process PATH. No global Git configuration or registry changes are required.

Ordinary Git for Windows CRLF conversion is supported. Proposal parsing accepts LF and CRLF delimiter lines while borrowing the original Markdown body. The eipw lint fetch adapter normalizes CRLF only in memory, including referenced proposals; it does not rewrite authored files or binary assets.

When Git checks out a repository-level Markdown symlink as an ordinary file (`core.symlinks=false`), a link such as the repository license alias is resolved only if its prepared index mode is a symlink and the stub bytes match the recorded Git blob. Its final destination must be inside the content tree. Arbitrary text files, modified stubs, Git metadata, and outside targets are not admitted. This is not general emulation of arbitrary directory symlinks.

## Long paths

Only private generated repositories are configured with `core.longpaths=true`. This lets libgit2 create long internal checkout paths without changing the source repository, user configuration, or Windows registry. It does not remove filesystem component-length limits or guarantee that every external tool supports every path length.

For environments where an external tool still rejects a long path, select a shorter writable build root, for example:

```powershell
build-eips -C C:\work\EIPs --build-root C:\builds\eips build
```

Keep the build root outside the authored source directories. A shorter path is an explicit environment workaround, not a replacement for failed platform acceptance.

Private live-generation clones also enable `core.longpaths` before fetch and checkout, without changing source Git configuration. On Windows their disposable staging repository is created under the system temporary root instead of beneath the build root, so an already-deep `--build-root` is not compounded by another generated Git metadata path. The owned staging repository is still validated before use, and its temporary directory is removed automatically when the live source is dropped.

### Source-checkout depth limitation

The internal-checkout long-path setting does not change authored repositories. On Git for Windows configurations where source-repository long-path support is disabled, a deeply nested source checkout can exceed libgit2's status-path limit before a build begins. The preprocessor retains the underlying Git path error and explains that the **source checkout itself** must move to a shorter directory. Changing only `--build-root` cannot repair an overlong source path.

The native acceptance matrix uses short disposable source roots with ordinary CRLF settings, while deliberately keeping generated build roots deep enough to exercise long internal paths. A separate overlong-source attempt is retained as limitation evidence. No global Git configuration or registry changes are made. Fully transparent long-path support for arbitrary authored checkout locations is not claimed.

## Reproducible checks

```powershell
$env:RUSTFLAGS = "-Dwarnings"
cargo +1.91.1 fmt --all -- --check
cargo +1.91.1 clippy --locked --all-targets -- -Dwarnings
cargo +1.91.1 test --locked
cargo +1.91.1 build --locked --release
python tests/serve_generation.py --build-eips C:\work\preprocessor\target\release\build-eips.exe --evidence C:\work\scratch\windows-watch
```

The watcher runner creates disposable repositories with complete proposal and network-upgrade metadata, checks staging and unstaging, branch switches, linked worktrees, assets, last-good retention, and recovery, and terminates only its own process trees. Its evidence directory must not already exist. Configure its PATH before invoking it; the runner does not change global settings.

A passing build or test suite is not by itself full platform acceptance. Also test full and targeted EIPs and ERCs builds, root and nested deployment paths, preview, live search navigation, exact popover metadata, and packaging. An executable launched on a developer machine is not a clean-Windows-VM certification.
