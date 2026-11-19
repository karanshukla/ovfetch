# ovfetch

`ovfetch` finds and installs the OpenVINO-enabled ONNX Runtime build that fits an Intel NPU. Before installing anything, it checks artifact hashes against multiple independent sources and a compiled-in ledger.

`ovfetch resolve` reports an installation plan without downloading or installing files:

```text
$ ovfetch resolve --min-openvino 2025.1
floor:    2025.1 (--min-openvino)
ceiling:  2026.4 (newest known to compile on the installed NPU driver)
openvino: 2025.4.1 (onnxruntime-openvino 1.24.1, Verified)
download: https://files.pythonhosted.org/packages/.../onnxruntime_openvino-1.24.1-cp313-cp313-manylinux_2_28_x86_64.whl
sha256:   2c3bb73e68ac27f4891af8a595c1faf574ec68b772e6583c90a0b997a1822782
```

(The download URL is shortened above for readability.)

`ovfetch` was built because [vinoAuthFace](https://github.com/karanshukla/vinoAuthFace) needs ONNX Runtime with the OpenVINO execution provider, and the right version depends on the NPU model, its driver, and what Intel has published. Pass `--json` to `resolve` for machine-readable output suited to scripts and automation.

## Requirements

- Linux `x86_64` (the platform Intel publishes these builds for).
- An Intel NPU is optional. Without an NPU present, no hardware bounds apply and `ovfetch` selects the newest known build.

## Install

Install from crates.io:

```bash
cargo install ovfetch --locked
```

Alternatively, download the static binary from [Releases](https://github.com/karanshukla/ovfetch/releases). The release binary includes signed build provenance (whereas `cargo install` compiles crates served by crates.io) and can be verified with:

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

- `detect` runs completely offline.
- `resolve` verifies hashes against upstream sources without downloading artifacts.
- `install` downloads and installs into `--prefix` without modifying the system's OpenVINO, driver, or compiler. If the prefix already contains the resolved build, installation is skipped; pass `--force` to reinstall.

## Commands

| Command | Description |
|---|---|
| `detect` | Prints the detected NPU, GPU, driver, compiler, and matching compatibility data as JSON. Runs offline. |
| `resolve` | Selects a matching build and verifies its hash across sources. Pass `--json` for machine-readable output. |
| `install --prefix DIR` | Resolves, downloads, verifies, and installs a build. Pass `--openvino-only` to omit ONNX Runtime, or `--build` to compile from a reviewed recipe. |
| `verify --prefix DIR` | Re-hashes an installed prefix against the `SHA256SUMS` file written at install time. |
| `npu install` | Fetches the NPU compiler omitted by a distro's driver package, or installs a full user-mode driver without root. |

Run `ovfetch <command> --help` to list all flags.

## How a build is chosen

`ovfetch` selects the newest prebuilt artifact that fits between two bounds:

- **Floor:** The oldest OpenVINO version verified for the NPU's platform.
- **Ceiling:** The newest OpenVINO version recorded as working with the installed NPU driver. Versions above the ceiling are untested (never assumed to fail).

If no build fits between those bounds, or if the NPU is not yet in `ovfetch`'s database, `ovfetch` stops and reports which bound blocked selection.

An artifact is installed only when PyPI and its mirrors agree on the SHA-256 hash, the compiled-in ledger matches that hash, and the downloaded bytes verify against it.

## Documentation

| Page | Contents |
|---|---|
| [Choosing a build](docs/choosing-a-build.md) | Floor and ceiling rules, supporting measurements, override flags, and OpenVINO without ONNX Runtime |
| [Local builds](docs/local-builds.md) | `--build`: pinned recipe inputs, pre-compile checks, and build requirements |
| [NPU compiler and driver](docs/npu.md) | `npu install`, the NPU prefix layout, and loading `ovfetch-preload.txt` from apps |
| [Verification](docs/verification.md) | Source agreement, the ledger, verifying releases, and the Discover, Guard, and Audit workflows |
| [Using ovfetch as a library](docs/library.md) | Offline detection crate usage with zero network dependencies |
| [Development](docs/development.md) | CI checks, module layout, and the stability contract with other `vino*` apps |
| [SECURITY.md](SECURITY.md) | Threat model and vulnerability reporting |

## Limitations

- Prebuilt ONNX Runtime support is limited to Intel's `onnxruntime-openvino` wheels. As of 2026-10-12, the newest wheel bundles OpenVINO 2025.4.1. A reviewed local build recipe can provide a newer version; if neither a prebuilt wheel nor a recipe fits, `ovfetch` exits with an error.
- Platform floors derived from driver release notes are only as accurate as Intel's "verified on" tables.
- Only Linux `x86_64` is supported.
- `npu install` supports releases distributed as per-distro tarballs (`1.22.0` and newer). It does not install the Level Zero loader or firmware, and it verifies tarballs against the ledger digest rather than checking package `.asc` signatures.

## License

MIT.
