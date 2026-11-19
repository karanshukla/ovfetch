# Development

## CI checks

CI (`.github/workflows/ci.yml`) runs these commands on every pull request:

```bash
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --no-default-features --all-targets -- -D warnings
cargo test --locked
cargo test --locked --no-default-features
cargo +1.88 check --locked --all-targets          # MSRV, with and without default features
cargo test --locked --target x86_64-unknown-linux-musl
cargo deny check
```

The workflow also builds and smoke-tests the static musl release binary, enforces a line coverage floor with `cargo llvm-cov`, lints workflow files with `actionlint`, and runs `.github/tests/guard.sh` against a stub `gh`.

All tests run offline and require neither an NPU nor root access. `resolve`, `npu::plan`, and `ci audit` run end-to-end against `upstream::fake::Fake`. Because the `data/*.toml` files are compiled into the binary, `cargo test` verifies that each file parses cleanly, and `tests/data_invariants.rs` validates their structure without hardcoding release versions so automated Discover PRs do not need to edit tests.

## Module layout

Disabling the default `cli` feature compiles only the offline detection and status modules.

| Module | Responsibility | Feature |
|---|---|---|
| `data.rs` | Loads `data/*.toml` files (compiled in or read from `--data-dir`) | always |
| `detect.rs` | Inspects the machine: Intel NPUs and GPUs from sysfs, plus the NPU driver and compiler from `ldconfig` and the NPU prefix | always |
| `status.rs` | Computes floor, ceiling, and warnings for a machine from local data alone | always |
| `version.rs` | `Ver` dotted numeric version type | always |
| `net.rs` | HTTPS client restricted to the host allowlist, hop-by-hop redirect checks, and streaming SHA-256 | `cli` |
| `sources.rs` | Queries PyPI, PyPI mirrors, and GitHub for artifact metadata and hashes | `cli` |
| `upstream.rs` | `Upstream` trait abstracting network calls, with `Network` for live runs and `Fake` for tests | `cli` |
| `consensus.rs` | Checks whether upstream source claims and the ledger agree on an artifact hash | `cli` |
| `resolve.rs` | Plans which artifact fits within the platform floor and driver ceiling | `cli` |
| `install.rs` | Downloads, extracts, and installs a resolved plan | `cli` |
| `build.rs` | `--build`: compiles ONNX Runtime from a reviewed recipe | `cli` |
| `npu.rs` | `npu install`: fetches the NPU compiler and driver from Intel's release tarballs | `cli` |
| `prefix.rs` | Manages an installed prefix: `SHA256SUMS`, lockfile, `verify`, and atomic swap | `cli` |
| `libs.rs` | Stages shared libraries, creates symlinks, and enforces `$ORIGIN`-only search paths | `cli` |
| `elf.rs` | Reads `SONAME`, `RPATH`, and `RUNPATH` from ELF64 binaries using `object` | `cli` |
| `listing.rs`, `tempdir.rs` | Directory listings and private scratch directory handling | `cli` |
| `ci.rs` | `ci audit` and `ci discover` subcommands run by scheduled workflows | `cli` |

## Stability contract with other vino programs

`vinoAuthFace`, `vinoOpenEffects`, `vinoWhisper`, `vinoSpeak`, and `vinoPilot` invoke `ovfetch` and consume its output files. Treat the following interfaces as a stability contract:

- CLI commands, flags, and the JSON schemas emitted by `detect` and `resolve --json`.
- `SHA256SUMS` and `ovfetch.lock.json` inside an installed prefix.
- The default NPU prefix (`~/.local/lib/ovfetch-npu`), `ovfetch-preload.txt`, and the exact order of lines in it ([NPU compiler and driver](npu.md)).
- The wording of version bounds: the ceiling is the newest OpenVINO *recorded as working*, and versions above it are described as untested, never "known to fail".

## Binary size impact of crate replacements

Measured 2026-11-16 on `x86_64` glibc release builds (`cargo build --release --locked`, using the repository's release profile) in a cloud container with `rustc 1.100.0 (0ef1bc386 2026-11-09)`. Crate counts come from `cargo tree --locked -e normal --prefix none | sort -u | wc -l`, which includes `ovfetch` itself.

| Commit | Crate added | `ovfetch` bytes | Change | Crates |
|---|---|---|---|---|
| `f166b1f` | *(base, before swaps)* | 3,737,296 | | 86 |
| `77b0343` | `tempfile` | 3,746,768 | +9,472 | 92 |
| `31c541b` | `ar` | 3,756,976 | +10,208 | 93 |
| `caea6b9` | `url` | 3,981,872 | +224,896 | 131 |
| `4d90fae` | `jiff` | 4,224,184 | +242,312 | 133 |
| `4edbf37` | `object` | 4,222,712 | -1,472 | 134 |
| `b244daa` | current `main` | 4,225,848 | +3,136 | 134 |

Total delta from base (`f166b1f`) to `4edbf37`: +485,416 bytes (+13.0%).
