# Development

## The gate

CI (`.github/workflows/ci.yml`) runs these on every pull request:

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

It also builds the static musl release binary and smoke-tests it, holds line
coverage above a floor (`cargo llvm-cov`), lints the workflows with
`actionlint`, and runs `.github/tests/guard.sh` against a stub `gh`.

Tests need no NPU, no network and no root. `resolve`, `npu::plan` and `ci audit` run whole against
`upstream::fake::Fake`. The data files are compiled in, so
`cargo test` also proves each one parses, and `tests/data_invariants.rs` checks
their shape without naming a release, so a Discover PR never has to edit a
test.

## Layout

Without the default `cli` feature only the offline half is built.

| Module | What it holds | Feature |
|---|---|---|
| `data.rs` | The `data/*.toml` files, compiled in or read from `--data-dir` | always |
| `detect.rs` | The machine: Intel NPUs and GPUs from sysfs, the NPU driver and its compiler from `ldconfig` and the NPU prefix | always |
| `status.rs` | Floor, ceiling and warnings for a machine, from the data alone | always |
| `version.rs` | `Ver`, a dotted numeric version | always |
| `net.rs` | HTTPS to the host allowlist, redirects checked hop by hop, streaming sha256 | `cli` |
| `sources.rs` | PyPI, its mirrors and GitHub: what each claims about a file | `cli` |
| `upstream.rs` | The `Upstream` trait: every network question, with `Network` as the live answer and a `Fake` for tests | `cli` |
| `consensus.rs` | Whether the sources' claims and the ledger agree on a hash | `cli` |
| `resolve.rs` | The plan: which artifact fits the floor and ceiling | `cli` |
| `install.rs` | Download, unzip and install a plan | `cli` |
| `build.rs` | `--build`: compile ONNX Runtime from a recipe | `cli` |
| `npu.rs` | `npu install`: the compiler and driver from Intel's release | `cli` |
| `prefix.rs` | An installed prefix: `SHA256SUMS`, the lock, `verify`, the atomic swap | `cli` |
| `libs.rs` | Staged libraries: symlink names, the `$ORIGIN`-only search path check | `cli` |
| `elf.rs` | Soname, RPATH and RUNPATH out of an ELF64 file, as the loader sees it (layouts from `object`) | `cli` |
| `listing.rs`, `tempdir.rs` | Directory listings; a private scratch directory | `cli` |
| `ci.rs` | `ci audit` and `ci discover`, run by the scheduled workflows | `cli` |

## The contract with other vino programs

vinoAuthFace, vinoOpenEffects, vinoWhisper, vinoSpeak and vinoPilot shell out to
`ovfetch` and read what it writes. Treat these as a contract:

- the commands and flags, and the JSON `detect` and `resolve --json` print;
- `SHA256SUMS` and `ovfetch.lock.json` in an installed prefix;
- the default NPU prefix `~/.local/lib/ovfetch-npu`, `ovfetch-preload.txt`, and
  the order of its lines ([NPU compiler and driver](npu.md));
- the wording of the bounds: the ceiling is the newest OpenVINO *recorded as
  working*, and anything above it is untested, never "known to fail".

## What the crate swaps cost

Measured 2026-11-16: x86_64 glibc release builds (`cargo build --release --locked`, the repo's own release profile) made in a cloud container with `rustc 1.100.0 (0ef1bc386 2026-11-09)`. Crate count is `cargo tree --locked -e normal --prefix none | sort -u | wc -l`, which includes `ovfetch` itself.

| Commit | Swapped in | `ovfetch` bytes | Change | Crates |
|---|---|---|---|---|
| `f166b1f` | (base, before the swaps) | 3,737,296 | | 86 |
| `77b0343` | `tempfile` | 3,746,768 | +9,472 | 92 |
| `31c541b` | `ar` | 3,756,976 | +10,208 | 93 |
| `caea6b9` | `url` | 3,981,872 | +224,896 | 131 |
| `4d90fae` | `jiff` | 4,224,184 | +242,312 | 133 |
| `4edbf37` | `object` | 4,222,712 | -1,472 | 134 |
| `b244daa` | current `main` | 4,225,848 | +3,136 | 134 |

Base to `4edbf37`: +485,416 bytes (+13.0%).
