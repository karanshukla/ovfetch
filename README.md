# ovfetch

`ovfetch` finds and installs the OpenVINO-enabled ONNX Runtime build that fits
an Intel NPU. It checks artifact hashes against multiple independent sources
and a built-in ledger before installing anything.

For example, `ovfetch resolve` reports a plan without downloading or installing
anything:

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
its driver, and what Intel has published. `resolve --json` provides the same
plan as machine-readable output for scripts and AI agents.

## Quick start

```bash
cargo install ovfetch --locked
ovfetch detect
ovfetch resolve
sudo ovfetch install --prefix /usr/local/lib/face-auth/openvino
ovfetch verify --prefix /usr/local/lib/face-auth/openvino
```

`detect` works offline. `resolve` checks hashes but does not download anything.
`install` downloads and installs into the specified prefix; it does not modify
the system's OpenVINO, driver, or compiler. If the prefix already contains the
resolved build, installation is skipped. Use `--force` to reinstall.

## Choosing a build

`ovfetch` prefers prebuilt builds and selects the newest one within two bounds:

| Bound | How it is determined |
|---|---|
| **Floor** | The NPU's PCI ID maps to the first Intel NPU driver release verified for that platform and the OpenVINO version paired with it. A human-tested floor can override this value. |
| **Ceiling** | The installed NPU driver maps to the newest OpenVINO known to work with it. This is Intel's pairing for that driver release unless a newer version has been measured. Versions above the ceiling are untested, not known to fail. |

Without an NPU, there are no bounds, so `ovfetch` selects the newest known
build.

The bounds are deliberately cautious. Measurements on Wildcat Lake on
2026-09-28 found that drivers 1.32.0 and 1.35.0 could compile and run a small
test model on every OpenVINO version from 2025.0 through 2026.4. The full
whisper-small.en pipeline was also tested on 2026.4; older versions were not
tested with a real model.

One failure was caused by a missing compiler, not an OpenVINO version mismatch:
Fedora's driver 1.32.0 RPM does not include the compiler, and
`compile_model` returns `ZE_RESULT_ERROR_UNSUPPORTED_FEATURE` without it.
Because the compiler library name changed between driver versions 1.32 and
1.35, `detect` checks for the library loaded by the installed driver.

If no prebuilt build fits, `ovfetch` stops and reports which bound is blocking
it. It also refuses an NPU whose platform is not yet in its data rather than
guessing. When a reviewed recipe in `data/builds.toml` offers a newer OpenVINO
version than any fitting prebuilt, `resolve` reports the recipe. Pass `--build`
to compile and install it; see [Local builds](#local-builds).

Useful options:

- `--min-openvino VERSION` sets a higher minimum OpenVINO version.
- `--ignore-driver` allows a version newer than the driver's known pairing.
  Use this only if you accept the risk.
- `--allow-unverified` lets `install` accept an artifact that sources agree on
  but the ledger has not recorded.
- `--allow-downgrade` lets `install` replace a newer version already installed
  in the prefix.

## Local builds

Intel's `onnxruntime-openvino` wheels can lag behind OpenVINO itself. As of
2026-10-12, the newest wheel bundled OpenVINO 2025.4.1, while OpenVINO was at
2026.4.1. Running different OpenVINO releases on the same NPU can cause a
device fault that resets the NPU for every process using it. Local builds let
`ovfetch` compile ONNX Runtime against a newer OpenVINO while keeping version
selection and provenance in one place.

```bash
ovfetch resolve --build
ovfetch install --build --prefix ~/.local/lib/app/ort
```

The first command shows the recipe and its pinned inputs without building.
The second compiles and installs the selected build.

Recipes in `data/builds.toml` pin each input. Before compiling, `install
--build` checks:

| Input | Check |
|---|---|
| ONNX Runtime source | The recipe's tag must resolve to its pinned commit. `resolve` checks the tag with GitHub, and the cloned source must have the same commit at `HEAD`. Local changes to the checkout are refused. |
| OpenVINO | The PyPI `openvino` wheel, including its headers, CMake configuration, and libraries, is checked against the same sources and ledger as other artifacts. This ensures the build uses the same OpenVINO files as Python programs on the machine. |
| Build flags | The flags are part of the reviewed recipe. |

For a local build, the plan's `sha256` covers the recipe inputs, not the
compiled output: output can differ between compilers. The lock records the
compiler and CMake versions used, and `SHA256SUMS` covers the installed files,
so `verify` works as it does for prebuilt builds. Installed libraries must
search only `$ORIGIN` for dependencies, preventing them from loading libraries
from the build tree later.

The build tree is used to compile and install the software. For that reason,
`--build` refuses a build directory that is not owned by the current user or
that another user can write to. It also runs Git with system, global, and hook
configuration ignored.

ONNX Runtime's CMake configuration downloads some dependencies directly, so
those downloads are outside `ovfetch`'s host allowlist. CMake checks them
against SHA-1 hashes in the pinned source's `cmake/deps.txt`.

Building requires Git, CMake, Ninja, Python 3, and a C++ compiler. On a Dell
XPS 13 (Core 5 320), a build with `--jobs 3` took 17 minutes (measured
2026-10-12) and used several gigabytes under `~/.cache/ovfetch/build`. Use
`--build-dir` to change the build directory. Source checkouts and the OpenVINO
wheel are cached, but the checkout is re-checked and the wheel re-hashed and
re-extracted on each run. Each build starts clean because compiled objects
from earlier runs cannot be verified. By default, `--jobs` uses half the
available CPUs; ONNX Runtime compilation can use substantial memory.

## Artifact verification

Before installation, `ovfetch` checks that the artifact hash is trustworthy:

1. **Independent sources agree.** PyPI and two independent mirrors report the
   wheel's SHA-256 hash. At least two different hosts must respond, and every
   responding source must agree. A disagreement aborts the install and shows
   each source's claim.
2. **The ledger matches.** `data/ledger.toml` records each artifact's hash the
   first time it is seen and is compiled into the binary. A changed hash is
   rejected even if all current sources agree.
3. **The downloaded bytes match.** `ovfetch` downloads from PyPI, falling back
   to a mirror only when PyPI is unreachable. It hashes the bytes as they
   arrive and rejects a mismatch.
4. **The connection is restricted.** Downloads use HTTPS to a fixed list of
   hosts. Hosts are checked again after redirects, and TLS roots are compiled
   in through rustls.

An artifact that all sources agree on but that is not in the ledger is
*unverified*. `install` rejects it unless you pass `--allow-unverified`.

Mirrors help detect a tampered CDN edge or a bad mirror, but they copy PyPI and
cannot detect a compromise of PyPI itself. The ledger provides protection
against that case.

## Using ovfetch as a library

The offline functionality is also available as a library. Without the default
`cli` feature, the crate has no network dependencies (only `anyhow`, `serde`,
and `toml`) and does not download anything:

```toml
ovfetch = { version = "0.2", default-features = false }
```

```rust
let data = ovfetch::data::Data::load(None)?;
let machine = ovfetch::detect::machine();
let status = ovfetch::status::status(&machine, &data);
// status.warnings: missing NPU driver or compiler, an old driver, or an unknown NPU
```

## Keeping the data current

Scheduled workflows maintain the version and platform data:

- **Discover** runs weekly. It records new `onnxruntime-openvino` wheels after
  they have been public for seven days and links each to Intel's matching
  `intel/onnxruntime` release. It also records new NPU driver releases, their
  OpenVINO pairing and asset hashes, and new NPU PCI IDs from the kernel's
  `ivpu` driver. Discover opens a pull request; changes reach users only after
  review and a release. It also flags when PyPI starts publishing provenance
  for these wheels.
- **Guard** runs on every pull request. It blocks changes to source, data,
  dependencies, or CI from anyone but the owner. It runs from the copy on the
  default branch, so a pull request cannot change the check to pass.
- **Audit** runs daily. It rechecks every ledger hash against every source and
  downloads a random sample from a random mirror. A detected change opens an
  issue.

## Installing

Install from crates.io:

```bash
cargo install ovfetch --locked
```

Alternatively, download a static binary from
[Releases](https://github.com/karanshukla/ovfetch/releases) and verify it as
described below. The release binary is the stronger option: it has signed
build provenance, while `cargo install` compiles the crates served by crates.io.

## Verifying a release

Release binaries are static (musl) and include signed build provenance:

```bash
gh attestation verify ovfetch-x86_64-linux --repo karanshukla/ovfetch
```

Release tags cannot be moved or deleted, and published releases are immutable.

## Known limits

- Only Intel's `onnxruntime-openvino` prebuilt wheels are supported. As of
  2026-10-12, the newest wheel bundles OpenVINO 2025.4.1. A reviewed local
  build recipe may support a newer version; without a fitting prebuilt or
  recipe, `ovfetch` refuses to guess.
- Floors derived from driver notes are only as reliable as Intel's "verified
  on" tables.
- Linux x86_64 only, where Intel publishes these builds.

## License

MIT.
