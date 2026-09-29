# ovfetch

Tell it "install the OpenVINO build this machine's Intel NPU needs" and it works out which one, downloads it, and refuses to install anything whose hash it cannot get independent sources to agree on.

```
$ ovfetch resolve --min-openvino 2025.1
floor:    2025.1 (--min-openvino)
ceiling:  2026.4 (newest known to compile on the installed NPU driver)
openvino: 2025.4.1 (onnxruntime-openvino 1.24.1, Verified)
download: https://files.pythonhosted.org/packages/.../onnxruntime_openvino-1.24.1-cp313-cp313-manylinux_2_28_x86_64.whl
sha256:   2c3bb73e68ac27f4891af8a595c1faf574ec68b772e6583c90a0b997a1822782
```

It exists because [vinoAuthFace](https://github.com/karanshukla/vinoAuthFace) needs ONNX Runtime with the OpenVINO execution provider, and the answer to "which version" depends on the NPU, its driver, and what Intel has actually published. Working that out by hand every time is the job this replaces, for people and for AI agents alike (`resolve --json` is the API).

## What it picks

Two bounds, and the newest prebuilt build between them:

| Bound | Where it comes from |
|---|---|
| **Floor** | The NPU's PCI ID, mapped to the first Intel NPU driver release verified on that platform and the OpenVINO it paired with. A human-tested floor can be pinned over it. |
| **Ceiling** | The installed NPU driver: the newest OpenVINO known to work with it, which is Intel's pairing for that release unless a newer one has been measured. Past it is untested rather than known to fail, and ovfetch stays below it anyway. |

No NPU, no bounds: the newest build wins.

Both bounds are deliberately pessimistic. Measured 2026-09-28 on Wildcat Lake, drivers 1.32.0 and 1.35.0 each compiled and ran a small test model on every OpenVINO from 2025.0 to 2026.4, and whisper-small.en's full pipeline on 2026.4, so neither bound has been seen to fail; the older versions were not tried with a real model. What did fail was a driver without its compiler: Fedora's 1.32.0 rpm ships none, and every `compile_model` then returns `ZE_RESULT_ERROR_UNSUPPORTED_FEATURE`, which looks like a version mismatch and is not one. `detect` checks for the compiler library the installed driver actually loads, since the name changed between 1.32 and 1.35.

**It never compiles anything.** If no prebuilt build fits, it stops and says which bound is in the way. An NPU that is newer than the data (no known floor) is refused too, rather than guessed at.

It installs into its own prefix and never touches the distro's OpenVINO, driver, or compiler. It will not replace a newer install in that prefix with an older one unless you pass `--allow-downgrade`.

## Usage

```bash
ovfetch detect                                   # NPU/GPU, driver, compiler, platform bounds; offline
ovfetch resolve [--json]                         # what it would install, hashes checked, nothing downloaded
sudo ovfetch install --prefix /usr/local/lib/face-auth/openvino    # download, verify, install
ovfetch verify --prefix /usr/local/lib/face-auth/openvino          # re-hash an install against its SHA256SUMS
# install skips the download when the prefix already holds the resolved build; --force reinstalls
```

`--min-openvino 2026.2` overrides the floor. `--ignore-driver` lifts the ceiling, at your own risk.

## How it decides a hash is trustworthy

1. **PyPI plus two independent mirrors** each state the wheel's sha256. At least two must answer, and every one that answers must agree. One disagreement aborts the install and prints every source's claim.
2. **The ledger** (`data/ledger.toml`) records the hash of every artifact the first time it was seen, and is compiled into the binary. A published file's hash must never change, so a mismatch aborts even when every source agrees.
3. **The bytes** come from pypi.org, falling back to a mirror only if PyPI is unreachable, and are hashed as they stream. They must match the agreed hash.
4. **The network** is HTTPS-only to a fixed list of hosts, re-checked after redirects. TLS roots are compiled in (rustls), so a spoofed DNS answer still needs a valid certificate for the real host.

Something every source agrees on but the ledger has never recorded is *unverified*, and `install` refuses it without `--allow-unverified`.

The mirrors catch a tampered CDN edge or a bad mirror, but they copy PyPI, so they cannot catch a compromise at PyPI itself. The ledger is what covers that.

## As a library

The offline half is a library too. Without the default `cli` feature it builds with no network crates (anyhow, serde, and toml only) and never downloads anything:

```toml
ovfetch = { version = "0.2", default-features = false }
```

```rust
let data = ovfetch::data::Data::load(None)?;
let machine = ovfetch::detect::machine();
let status = ovfetch::status::status(&machine, &data);
// status.warnings: missing NPU driver or compiler, a driver older than the platform needs, an NPU the data does not know
```

## Keeping the data current

Two scheduled workflows, so nobody maintains version tables by hand:

- **Discover** (weekly) records new onnxruntime-openvino wheels once they have been public for 7 days, each linked to Intel's matching intel/onnxruntime release so review means checking it lines up with a real release. It also flags the day PyPI starts publishing provenance for them, and records new NPU driver releases with their OpenVINO pairing and asset hashes, and new NPU PCI IDs from the kernel's `ivpu` driver. It opens a PR. Nothing reaches users until that PR is reviewed and a release is cut.
- **Guard** (every PR) fails a PR from anyone but the owner that touches source, data, dependencies, or CI. It runs from main's copy, so a PR cannot edit it to pass, and only the owner can merge past it.
- **Audit** (daily) re-checks every ledger hash against every source and spot-downloads a random few from a random mirror. Any change opens an issue.

## Installing

```bash
cargo install ovfetch --locked
```

Or download the static binary from [Releases](https://github.com/karanshukla/ovfetch/releases) and verify it (below). The binary is the stronger option: its provenance is signed, while `cargo install` compiles whatever crates.io serves.

## Verifying a release

Release binaries are static (musl) and carry signed build provenance:

```bash
gh attestation verify ovfetch-x86_64-linux --repo karanshukla/ovfetch
```

Release tags cannot be moved or deleted, and releases are immutable once published.

## Known limits

- Only Intel's `onnxruntime-openvino` wheels are installed. As of 2026-09-27 the newest bundles OpenVINO 2025.4.1, so a platform with a 2026.x floor (Wildcat Lake) gets a refusal until Intel ships a newer one.
- Floors derived from driver notes are only as good as Intel's "verified on" tables.
- Linux x86_64 only, which is where Intel publishes these builds.

## License

MIT.
