<p align="center">
  <h1 align="center">dockdash</h1>
  <p align="center">
    Build and push Docker images from Rust 🦀 — without Docker installed.
  </p>
</p>

<p align="center">
  <a href="https://crates.io/crates/dockdash"><img src="https://img.shields.io/crates/v/dockdash.svg" alt="crates.io"></a>
  <a href="https://docs.rs/dockdash"><img src="https://docs.rs/dockdash/badge.svg" alt="docs.rs"></a>
  <a href="https://github.com/alienplatform/dockdash/actions"><img src="https://github.com/alienplatform/dockdash/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License"></a>
</p>

---

**Dockdash** is a Rust library for building and pushing Docker (OCI) images — no Docker daemon, no `docker` CLI, no privileged containers. Just a normal Rust crate that produces real container images and ships them to any registry.

It's fast enough to run inside a serverless function: build an image in a Lambda, push it to ECR, and exit before your cold start budget runs out.

## Why Dockdash?

- 🔥 **Blazingly fast** — native Rust, zstd layer compression, and content-addressable blob caching. Builds images in milliseconds, not seconds.
- ☁️ **Runs anywhere** — serverless functions, sandboxed CI, edge runtimes, CLIs. If Rust runs there, Dockdash works there.
- 🐳 **No Docker required** — no daemon, no socket, no root. Just a library call.
- 🧱 **Simple API** — a builder pattern for layers and images. No Dockerfiles, no shelling out.
- 🌍 **Multi-arch** — build for `amd64`, `arm64`, and any other platform you need.
- 📦 **Push to any OCI registry** — Docker Hub, ECR, GCR, ACR, GHCR, or your own. Anonymous, basic auth, and token auth supported.
- ⚡ **Incremental builds** — local content-addressable cache means unchanged layers are reused instantly.

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
dockdash = "0.2"
tokio = { version = "1", features = ["full"] }
```

Build and push an image:

```rust
use dockdash::{Arch, Image, Layer, PushOptions, Result};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    // Create a layer from a local binary
    let layer = Layer::builder()?
        .file("./target/release/my-app", "./my-app", Some(0o755))?
        .build()
        .await?;

    // Build the OCI image
    let (image, _) = Image::builder()
        .from("ubuntu:latest")
        .platform("linux", &Arch::ARM64)
        .layer(layer)
        .entrypoint(vec!["/my-app".to_string()])
        .output_to(PathBuf::from("image.oci.tar"))
        .build()
        .await?;

    // Push to a registry
    image.push("my-registry.com/my-app:latest", &PushOptions::default()).await?;

    Ok(())
}
```

## Features

### Layer Builder

Create layers from files or raw data:

```rust
// From a file on disk
let layer = Layer::builder()?
    .file("./my-binary", "./app/my-binary", Some(0o755))?
    .build()
    .await?;

// From raw bytes
let layer = Layer::builder()?
    .data("./app/config.toml", config_bytes, None)?
    .build()
    .await?;

// Multiple files in one layer
let layer = Layer::builder()?
    .file("./binary", "./app/binary", Some(0o755))?
    .file("./config.toml", "./app/config.toml", None)?
    .data("./app/version.txt", b"1.0.0", None)?
    .build()
    .await?;
```

### Image Builder

Compose images from a base and custom layers:

```rust
let (image, diagnostics) = Image::builder()
    .from("alpine:3.19")
    .platform("linux", &Arch::Amd64)
    .layer(app_layer)
    .layer(config_layer)
    .entrypoint(vec!["/app/server".to_string()])
    .working_dir("/app")
    .build()
    .await?;
```

### Blob Caching

Speed up repeated builds with content-addressable caching:

```rust
use dockdash::BlobCache;

// Default location: ~/.dockdash/cache/blobs
let cache = BlobCache::new()?;

// Or use a custom cache directory (path is used as-is)
let cache = BlobCache::with_path("/my/custom/cache".into())?;

let layer = Layer::builder()?
    .blob_cache(cache.clone())
    .file("./my-binary", "./app/my-binary", Some(0o755))?
    .build()
    .await?;

let (image, _) = Image::builder()
    .from("alpine:latest")
    .blob_cache(cache)
    .layer(layer)
    .build()
    .await?;
```

### Registry Authentication

```rust
use dockdash::{RegistryAuth, PushOptions, ClientProtocol};

// Anonymous (e.g., ttl.sh)
let opts = PushOptions::default();

// Basic auth
let opts = PushOptions {
    auth: RegistryAuth::Basic("user".into(), "pass".into()),
    ..Default::default()
};

// HTTP (for local registries)
let opts = PushOptions {
    protocol: ClientProtocol::Http,
    ..Default::default()
};
```

## Use Cases

- **Serverless functions** — build and push images from Lambda, Cloud Run, Vercel, or Cloudflare Workers, in milliseconds.
- **CI/CD pipelines** — ship container images without Docker-in-Docker, root, or privileged runners.
- **Deployment platforms** — bake user code into images on the fly as part of your build/deploy pipeline.
- **CLI tools** — embed container image building directly into your Rust CLI, no external dependencies.
- **Edge & embedded** — build images on resource-constrained devices where Docker simply isn't an option.

## Testing

```bash
# Unit tests
cargo test

# Integration tests (requires Docker for Bollard-based tests)
cargo test --features test-utils
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
