# ovfetch

`ovfetch` finds and installs the OpenVINO-enabled ONNX Runtime build that fits
an Intel NPU. It checks artifact hashes against multiple independent sources
and a built-in ledger before installing anything.

`ovfetch resolve` reports a plan without downloading or installing anything:

```text
$ ovfetch resolve --min-openvino 2025.1
floor:    2025.1 (--min-openvino)
ceiling:  2026.4 (newest known to compile on the installed NPU driver)
openvino: 2025.4.1 (onnxruntime-openvino 1.24.1, Verified)
download: https://files.pythonhosted.org/packages/.../onnxruntime_openvino-1.24.1-cp313-cp313-manylinux_2_28_x86_64.whl
sha256:   2c3bb73e68ac27f4891af8a595c1faf574ec68b772e6583c90a0b997a1822782
```

The URL is shortened here for readability. `ovfetch` exists because
[vinoAuthFace](https://github.com/karanshukla/vinoAuthFace) needs ONNX Runtime
with the OpenVINO execution provider. Which version to use depends on the NPU,
its driver, and what Intel has published. `resolve --json` gives the same plan
as machine-readable output for scripts and AI agents.

## Requirements

Linux x86_64, where Intel publishes these builds. No NPU is needed to run it:
without one there are no bounds, and it picks the newest known build.

## Install

```bash
cargo install ovfetch --locked
```

Or download the static binary from
[Releases](https://github.com/karanshukla/ovfetch/releases). The release binary
is the stronger option: it has signed build provenance, while `cargo install`
compiles the crates served by crates.io.

```bash
gh attestation verify ovfetch-x86_64-linux --repo karanshukla/ovfetch
```

## Quick start

```bash
ovfetch detect
ovfetch resolve
sudo ovfetch install --prefix /usr/local/lib/face-auth/openvino
ovfetch verify --prefix /usr/local/lib/face-auth/openvino
```

`detect` works offline. `resolve` checks hashes but does not download anything.
`install` downloads and installs into the prefix; it does not modify the
system's OpenVINO, driver, or compiler. If the prefix already holds the
resolved build, installation is skipped. Use `--force` to reinstall.

## Commands

| Command | What it does |
|---|---|
| `detect` | Prints the NPU, GPU, driver and compiler it finds, and what the data says about them, as JSON. Offline. |
| `resolve` | Picks the build and checks its hash with every source. `--json` for scripts. |
| `install --prefix DIR` | Resolves, downloads, verifies and installs. `--openvino-only` leaves ONNX Runtime out; `--build` compiles it from a reviewed recipe. |
| `verify --prefix DIR` | Re-hashes a prefix against the `SHA256SUMS` written at install. |
| `npu install` | Fetches the NPU compiler a distro's driver package left out, or a whole user-mode driver, with no root. |

`ovfetch <command> --help` lists every flag.

## How a build is chosen

`ovfetch` picks the newest prebuilt build between two bounds. The **floor** is
the oldest OpenVINO verified for the NPU's platform. The **ceiling** is the
newest OpenVINO known to work with the installed NPU driver. The ceiling is
deliberately cautious: versions above it are untested, not known to fail. If
nothing fits, or the NPU is not in its data yet, `ovfetch` stops and says which
bound is in the way instead of guessing.

An artifact is installed only when PyPI and its mirrors agree on the hash, the
compiled-in ledger agrees with them, and the downloaded bytes match.

## Documentation

| Page | What is in it |
|---|---|
| [Choosing a build](docs/choosing-a-build.md) | The floor and ceiling, the measurements behind them, the flags that override them, OpenVINO without ONNX Runtime |
| [Local builds](docs/local-builds.md) | `--build`: what a recipe pins, what is checked before compiling, what it needs |
| [NPU compiler and driver](docs/npu.md) | `npu install`, the prefix, and how an app loads `ovfetch-preload.txt` |
| [Verification](docs/verification.md) | Source agreement, the ledger, verifying a release, the Discover, Guard and Audit workflows |
| [Using ovfetch as a library](docs/library.md) | Offline detection with no network dependencies |
| [Development](docs/development.md) | The CI gate, the module layout, the contract with other vino programs |
| [SECURITY.md](SECURITY.md) | Threat model and how to report a vulnerability |

## Known limits

- Prebuilt ONNX Runtime support is limited to Intel's `onnxruntime-openvino`
  wheels. As of 2026-10-12, the newest wheel bundles OpenVINO 2025.4.1. A
  reviewed local build recipe may support a newer version; without a fitting
  prebuilt or recipe, `ovfetch` refuses to guess.
- Floors derived from driver notes are only as reliable as Intel's "verified
  on" tables.
- Linux x86_64 only.
- `npu install` reads releases shipped as per-distro tarballs (1.22.0 and
  newer). It does not fetch the Level Zero loader or firmware, and it does
  not check the packages' `.asc` signatures; the ledger digest is the check.

## License

MIT.
