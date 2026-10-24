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
`ovfetch npu install` fetches it; see [NPU compiler and driver](#npu-compiler-and-driver).

If no prebuilt build fits, `ovfetch` stops and reports which bound is blocking
it. It also refuses an NPU whose platform is not yet in its data rather than
guessing. When a reviewed recipe in `data/builds.toml` offers a newer OpenVINO
version than any fitting prebuilt, `resolve` reports the recipe. Pass `--build`
to compile and install it; see [Local builds](#local-builds).

Useful options:

- `--openvino-only` installs OpenVINO without ONNX Runtime; see
  [Without ONNX Runtime](#without-onnx-runtime).
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

## Without ONNX Runtime

Programs that call OpenVINO directly (through its C API, the `openvino` crate, or `openvino-sys`) don't need ONNX Runtime, and so aren't held back by Intel's onnxruntime-openvino wheels:

```bash
ovfetch resolve --openvino-only
ovfetch install --openvino-only --prefix ~/.local/lib/app/openvino
```

This installs Intel's PyPI `openvino` wheel alone: the newest OpenVINO between the floor and the ceiling, through the same source agreement and ledger, and never a compile. It is the same library set `--build` takes from that wheel (runtime, C API, CPU/GPU/NPU/AUTO/HETERO plugins, ONNX frontend, TBB), 139 MB, installed in 17 seconds on a Dell XPS 13 (Core 5 320, measured 2026-10-18). Within a release, a wheel the ledger already records is preferred over a newer CPython tag; the libraries are the same build, and a recorded one installs as verified. The lock has no `artifact.onnxruntime`, and `artifact.kind` is `openvino`.

Building requires Git, CMake, Ninja, Python 3, and a C++ compiler. On a Dell
XPS 13 (Core 5 320), a build with `--jobs 3` took 17 minutes (measured
2026-10-12) and used several gigabytes under `~/.cache/ovfetch/build`. Use
`--build-dir` to change the build directory. Source checkouts and the OpenVINO
wheel are cached, but the checkout is re-checked and the wheel re-hashed and
re-extracted on each run. Each build starts clean because compiled objects
from earlier runs cannot be verified. By default, `--jobs` uses half the
available CPUs; ONNX Runtime compilation can use substantial memory.

## NPU compiler and driver

Distros package the NPU's user-mode driver without its compiler, or not at all. `npu install` takes them from Intel's linux-npu-driver release instead, with no dpkg and no root:

```bash
ovfetch npu install            # into ~/.local/lib/ovfetch-npu, which detect also reads
ovfetch detect                 # npu_compiler: true
```

The release tarball is checked against the ledger (GitHub is its only source, so an unrecorded one needs `--allow-unverified`). The packages are unpacked in-process, and only their `lib*.so*` files are kept. The prefix gets `SHA256SUMS`, so `ovfetch verify` works on it.

| Host driver | What is installed |
|---|---|
| Finds its compiler | Nothing. |
| 1.32 and older (looks up `libnpu_driver_compiler.so` by soname) | The compiler from the same release. |
| 1.35 and newer | The driver and compiler from the same release, replacing the host's for apps that load them. The driver opens `libopenvino_intel_npu_compiler_loader.so` from its own directory by absolute path, so a compiler added anywhere else is never found. |
| None | The newest release verified on this NPU's platform, or `--driver`. Needs the host's Level Zero loader (`libze_loader.so.1`, from the distro's level-zero package). |

`--full` installs the driver even when the compiler alone would do. `--driver` picks a release; without `--full` it must match the host's.

The prefix's libraries are not on the linker path, so apps load them themselves. `ovfetch-preload.txt` lists them, one per line, in load order: absolute paths for the prefix's files, a soname (`libze_loader.so.1`) for the host's loader. `dlopen` each with `RTLD_GLOBAL` after OpenVINO's own libraries are loaded (the compiler links against `libtbb.so.12`, which OpenVINO brings) and before OpenVINO first opens the NPU. When the driver later asks for those libraries by soname, the dynamic linker returns the copies already loaded. In Python:

```python
import ctypes, openvino as ov
for line in open(f"{prefix}/ovfetch-preload.txt"):
    ctypes.CDLL(line.strip(), mode=ctypes.RTLD_GLOBAL)
core = ov.Core()
```

Order matters: a driver loaded before the Level Zero loader crashed on its first call into it (driver 1.35.0, measured 2026-10-24). Starting the app with `LD_LIBRARY_PATH=<prefix>` works too. On a Dell XPS 13 (Wildcat Lake, kernel driver from Fedora), OpenVINO 2026.4.1 compiled and ran a test model on the NPU with each of these (measured 2026-10-24):

- the 1.32.0 driver, no compiler beside it, plus the preloaded 1.32.0 compiler (this is Fedora's 1.32.0 RPM case; without the compiler, `ZE_RESULT_ERROR_UNSUPPORTED_FEATURE`)
- a full 1.35.0 install
- a full 1.32.0 install

The kernel driver and firmware stay the host's: firmware is loaded from `/lib/firmware`, which needs root.

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
  `intel/onnxruntime` release. It also tracks `openvino` wheels from 2025.4 and
  records new NPU driver releases, their OpenVINO pairing and asset hashes, and
  new NPU PCI IDs from the kernel's `ivpu` driver. Discover opens a pull
  request; changes reach users only after review and a release. It also flags
  when PyPI starts publishing provenance for the `onnxruntime-openvino` wheels.
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

- Prebuilt ONNX Runtime support is limited to Intel's `onnxruntime-openvino`
  wheels. As of
  2026-10-12, the newest wheel bundles OpenVINO 2025.4.1. A reviewed local
  build recipe may support a newer version; without a fitting prebuilt or
  recipe, `ovfetch` refuses to guess.
- Floors derived from driver notes are only as reliable as Intel's "verified
  on" tables.
- Linux x86_64 only, where Intel publishes these builds.
- `npu install` reads releases shipped as per-distro tarballs (1.22.0 and
  newer). It does not fetch the Level Zero loader or firmware, and it does
  not check the packages' `.asc` signatures; the ledger digest is the check.

## License

MIT.
