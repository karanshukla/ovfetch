# Changelog

All notable changes to ovfetch are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0] - 2026-10-24

Breaking for library users: `detect::Machine` has a new field, `npu_prefix`.

### Added

- `ovfetch npu install`: Intel's NPU compiler, or the user-mode driver and compiler, from a linux-npu-driver release into a prefix (default `~/.local/lib/ovfetch-npu`), checked against the ledger, with no dpkg and no root. It writes `ovfetch-preload.txt`, the libraries an app loads in order before OpenVINO opens the NPU. This replaces the `dpkg-deb -x` and `sudo install` steps for Fedora's compiler-less 1.32.0 driver.
- `detect` counts the libraries in `~/.local/lib/ovfetch-npu`: a driver there replaces the host's, and a compiler there counts as found when the driver can reach it. `detect::machine_with` takes another prefix, or none.

### Fixed

- `install` no longer hangs forever on a DNS lookup that never answers: lookups time out after 10 seconds, and response bodies after 20 minutes.
- `install` stages the new build next to the prefix and swaps it in with renames, so a failed copy no longer leaves a half-replaced prefix.

## [0.5.0] - 2026-10-18

Breaking for library users: `resolve::resolve`, `Artifact` and `Kind` change (see Changed).

### Added

- `--openvino-only` on `resolve` and `install`: Intel's `openvino` wheel alone, for programs that call OpenVINO without ONNX Runtime. It takes the newest OpenVINO that fits the device and never compiles, so they get 2026.4.1 in seconds instead of a 17-minute `--build`.
- `ci discover` records `openvino` wheels from 2025.4 on.

### Changed

- `resolve::resolve` takes a `Want` (`OnnxRuntime { allow_build }` or `OpenvinoOnly`) instead of `allow_build`.
- `Artifact::onnxruntime` is an `Option`, absent from the lock for an OpenVINO-only install; `Kind` gains `Openvino`.

## [0.4.1] - 2026-10-13

### Fixed

- A `--build` install now has the unversioned `libopenvino_c.so` and `libopenvino.so` (and `libfoo.so` for every versioned library). The openvino wheel a build installs from ships only sonames, so anything linking OpenVINO directly, such as the `openvino` crate's build script, could not find it.

## [0.4.0] - 2026-10-12

### Added

- `--build` on `resolve` and `install`: compile ONNX Runtime from a reviewed recipe in `data/builds.toml` when it offers a newer OpenVINO than every prebuilt that fits. The source is pinned by commit and OpenVINO is the PyPI wheel, checked like any other artifact. Without `--build`, `resolve` names the newer recipe in a warning.
- The first recipe: ONNX Runtime 1.24.1 with OpenVINO 2026.4.1.
- `install` refuses any library whose RPATH or RUNPATH reaches outside `$ORIGIN`.
- `ci audit` checks each recipe's tag still points at its pinned commit and its OpenVINO wheel is in the ledger.

### Changed

- `resolve::resolve` takes an `allow_build` argument, and `install::Options` gains `build_dir` and `jobs`.

## [0.3.0] - 2026-10-03

### Security

- `install` creates its temporary directory private and new.
- A hash quorum now counts distinct hosts, so one host listed twice no longer counts as two.
- Downloads are capped in size, sonames are checked, and the `Authorization` header is scoped to its own host.
- `discover` no longer persists checkout credentials.

### Documentation

- Refer to vinoAuthFace instead of Gaze.

## [0.2.2] - 2026-09-28

### Added

- SONAME links for every installed library; `install` is skipped when the prefix is already current.

## [0.2.1] - 2026-09-28

### Added

- A library target; the offline half builds without the network crates.

### Changed

- `ci discover` prints nothing when nothing is new.

## [0.2.0] - 2026-09-28

### Changed

- Pessimistic driver bounds come from measurement, and the compiler the driver loads is detected.

### Security

- `verify` fails on files ovfetch did not write, and `install` refuses such a prefix.

## [0.1.1] - 2026-09-27

### Changed

- Ported to ureq 3, following redirects by hand, and added the guard check.

## [0.1.0] - 2026-09-27

### Added

- Initial release: resolve, verify and install prebuilt ONNX Runtime and OpenVINO for Intel NPUs.

[Unreleased]: https://github.com/karanshukla/ovfetch/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/karanshukla/ovfetch/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/karanshukla/ovfetch/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/karanshukla/ovfetch/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/karanshukla/ovfetch/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/karanshukla/ovfetch/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/karanshukla/ovfetch/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/karanshukla/ovfetch/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/karanshukla/ovfetch/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/karanshukla/ovfetch/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/karanshukla/ovfetch/releases/tag/v0.1.0
