# Layer cache regression evidence

Baseline: `b662f247604f11eb3bb4dbe715c8a8792d0f25e4`.
Toolchain: Rust 1.98.1, Linux x86_64. All fixtures are synthetic.

The final `layer_cache_correctness.rs` was run against the unchanged baseline
`src/layer.rs`, then against the fixed implementation:

| Behavior | Baseline | Fixed |
| --- | --- | --- |
| File content, unchanged size/mtime | stale `before` bytes | `after!` |
| File content with explicit mode | stale `before` bytes | `after!` |
| Directory content, unchanged size/mtime | stale `before` bytes | `after!` |
| Archive entry ordering | old `[a,b]` order | requested `[b,a]` |
| Symlink target with preserved mtime | old `first` target | requested `other` |
| Explicit owner/mode headers | pass | pass |
| Repeated content yields stable compressed output | pass | pass |

Baseline: 2 passed, 5 failed, exit 101. Fixed: 7 passed, exit 0. Each fixed test
runs both Zstd and gzip; baseline failing loops stop at their first assertion.
Archive contents, links, headers and ordering are inspected after decompression.
The stable-output control alone does not claim an instrumented cache hit.
All 38 existing library tests also pass. All-target/all-feature clippy passes.

Reproduce in a disposable checkout:

```sh
cargo test --test layer_cache_correctness
cargo test --lib
cargo clippy --all-targets --all-features --locked -- -D warnings
# Keep the final test file, replace only src/layer.rs with the baseline version,
# then rerun the regression target: it must fail five tests.
git show b662f247604f11eb3bb4dbe715c8a8792d0f25e4:src/layer.rs > src/layer.rs
cargo test --test layer_cache_correctness
```

The finalized tar was already written before the former early lookup. Hashing it
now costs a complete tar read on every lookup but includes the exact bytes, all
headers, symlinks and entry order. Old input mappings are ignored; existing
content mappings can still avoid compression. No tar normalization is added.

The `cache-evidence-push` example can push a synthetic OCI archive to a local HTTP
registry for independent digest/payload checks. In the accompanying build-cache
experiment, every registry layer and config payload matched its archive bytes;
normalizing the OCI index also preserved those bytes. This does not imply that
arbitrary Dockerfile execution is reproducible.

Downstream consumers must adopt a release or revision containing this change.
An existing published version or pinned older revision does not acquire the fix.
No crate publication is part of this change.
