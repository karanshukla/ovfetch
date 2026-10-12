# Changelog

All notable changes to ovfetch are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/karanshukla/ovfetch/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/karanshukla/ovfetch/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/karanshukla/ovfetch/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/karanshukla/ovfetch/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/karanshukla/ovfetch/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/karanshukla/ovfetch/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/karanshukla/ovfetch/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/karanshukla/ovfetch/releases/tag/v0.1.0
