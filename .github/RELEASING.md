# Release procedure for build-eips

Releases here are EIP-WG development candidates under `ritovision`, not official
Ethereum-controlled releases. The first candidate is `v0.1.0-rc.1`.

1. Change the `[package]` version in `Cargo.toml` and the `build-eips` entry in
   `Cargo.lock` to the same SemVer prerelease (for example, `0.1.0-rc.2`).
2. Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -Dwarnings`,
   and `cargo test --locked`. Push to `main` and verify the cross-platform CI.
3. Dispatch the `release` workflow against the exact `main` commit. That runs
   all four native platform builds and tests, checks binary versions and SHA-256
   sidecars, and uploads workflow artifacts. **It does not publish a Release.**
4. When all jobs pass, create an **annotated** `v<package-version>` Git tag on
   that tested commit and push the tag. The tag-triggered `release` workflow
   repeats the same gates and publishes only if all four jobs succeed.
5. Verify the published release is marked *prerelease*, has four binaries and
   four `.sha256` sidecars, all checksums match, and extracted binaries report
   the intended version on their native platforms. Verify installation scripts
   with actual released assets before suggesting installation to editors.
6. If a tag-triggered release fails, **never move the published tag**. Fix the
   source and create a new `-rc.N` version/tag instead.

The Git tag, Cargo version, and `Cargo.lock` together identify the source and
locked dependencies. Builds are not claimed to be bit-for-bit reproducible
across platforms. The release workflow requires Rust 1.91.1 and verifies:

| Asset | Platform |
| --- | --- |
| `build-eips-ubuntu.tar.xz` | Ubuntu/Linux x86-64 |
| `build-eips-windows.zip` | Windows x86-64 |
| `build-eips-macos-arm64.tar.xz` | macOS Apple Silicon |
| `build-eips-macos-x86_64.tar.xz` | macOS Intel |

The build jobs have read-only repository access. Only the gated release job
receives `contents: write`. Do not attach release or Pages publishing to normal
push/PR CI, and do not use a floating branch or `latest` to select executable
CI dependencies.
