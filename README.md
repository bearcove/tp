# tp

Trusted publishing setup tool for Cargo workspaces.

Configures [crates.io trusted publishing](https://blog.rust-lang.org/2023/11/09/crates-io-trusted-publishing.html) for all publishable crates in a workspace.

## Installation

```bash
cargo install --git https://github.com/bearcove/tp
```

## Usage

```bash
# Set your crates.io API token
export CRATES_IO_TOKEN="your-token-here"

# Run in your Cargo workspace
tp <owner> <repo> [options]
```

### Arguments

- `<owner>` - GitHub repository owner (e.g., "facet-rs")
- `<repo>` - GitHub repository name (e.g., "facet")

### Options

- `-w, --workflow <FILE>` - Workflow filename (default: "release-plz.yml")
- `-e, --token-env <VAR>` - Environment variable for crates.io token (default: "CRATES_IO_TOKEN")
- `-n, --dry-run` - Don't actually configure trusted publishing, just show what would happen

### Example

```bash
# Configure trusted publishing for all crates in the facet workspace
CRATES_IO_TOKEN=cio_xxx tp facet-rs facet

# Dry run to see what would be configured
tp facet-rs facet --dry-run

# Use a different workflow file
tp myorg myrepo --workflow ci.yml
```

## How it works

1. Runs `cargo metadata` to discover all publishable crates in the workspace
2. Checks that each crate has been published to crates.io at least once
3. Configures trusted publishing via the crates.io API for each crate

All crates must be published at least once before trusted publishing can be configured.

## License

MIT OR Apache-2.0
