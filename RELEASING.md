# Releasing Trace Index

Publishing source, creating a Git tag, publishing a crate, publishing a GitHub Release, and deploying documentation are separate state changes. The first public release is `0.1.0`; Storage Format and release manifests begin at version `1`.

## Prepare a release candidate

1. Confirm `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, root Help, bundled docs, and Storage Format agree.
2. Run the complete local verification:

   ```bash
   cargo fmt --check
   cargo test --all-targets
   cargo clippy --all-targets --all-features -- -D warnings
   RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --document-private-items
   CARGO_TARGET_DIR=target/package-verify cargo package --locked
   npm ci
   npm run docs:check
   ./scripts/test-installer.sh
   ```

3. Inspect `cargo package --list` and verify that no private experiment, internal URL, credential, or local path is present.
4. Build a real index with the release binary, run `PRAGMA quick_check` directly with SQLite, verify the public relations, and confirm an incremental no-change synchronization.
5. Run the private regression gate without copying its corpus, prompts, results, or metrics into the public repository.

## Build GitHub Release assets

Push an annotated `vVERSION` tag only after the release candidate passes. The Release workflow verifies that the tag matches `Cargo.toml`, builds these native artifacts, checks their reported version, and creates a draft GitHub Release:

```text
trace-index-vVERSION-x86_64-unknown-linux-gnu.tar.gz
trace-index-vVERSION-x86_64-apple-darwin.tar.gz
trace-index-vVERSION-aarch64-apple-darwin.tar.gz
```

Every archive has a sibling `.sha256`. The draft also contains `install.sh`, `latest`, and `manifest.json`.

## Publish

Inspect and install the draft assets before the irreversible crate publication:

```bash
cargo publish --dry-run --registry crates-io --locked
cargo publish --registry crates-io --locked
```

After crates.io reports the exact version, publish the draft GitHub Release and verify from clean environments:

```bash
cargo install trace-index --version VERSION --locked
curl --proto '=https' --tlsv1.2 -fsSL \
  'https://github.com/wangyuxinwhy/trace-index/releases/latest/download/install.sh' | sh
```

Verify the GitHub Pages HTML, `llms.txt`, `llms-full.txt`, and at least one per-page Markdown URL. Never delete the previous distribution surface until all public verification succeeds.
