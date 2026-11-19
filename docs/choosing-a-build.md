# Choosing a build

This page explains how `ovfetch resolve` and `ovfetch install` select an OpenVINO version and which flags override that selection.

`ovfetch` prefers prebuilt artifacts and selects the newest build within two bounds:

| Bound | How it is determined |
|---|---|
| **Floor** | The NPU's PCI ID maps to the first Intel NPU driver release verified for that platform and the OpenVINO version paired with it. A human-tested floor can override this value. |
| **Ceiling** | The installed NPU driver maps to the newest OpenVINO version recorded as working with it. This defaults to Intel's pairing for that driver release unless a newer version has been measured. Versions above the ceiling are untested (never assumed to fail). |

Without an NPU, no bounds apply, so `ovfetch` selects the newest known build.

## Measurements and bounds

These bounds are conservative. Measurements on Wildcat Lake on 2026-09-28 showed that drivers `1.32.0` and `1.35.0` compiled and ran a small test model across every OpenVINO version from `2025.0` through `2026.4`. The full `whisper-small.en` pipeline was also tested on `2026.4`; older versions were not tested with a full production model.

One observed failure came from a missing compiler rather than an OpenVINO version mismatch: Fedora's `1.32.0` driver RPM omits the NPU compiler, causing `compile_model` to return `ZE_RESULT_ERROR_UNSUPPORTED_FEATURE`. Because the compiler library name changed between driver versions `1.32` and `1.35`, `detect` checks specifically for the library loaded by the installed driver. Run `ovfetch npu install` to fetch the missing compiler; see [NPU compiler and driver](npu.md).

If no prebuilt build fits within the bounds, `ovfetch` stops and reports which bound blocked selection. It also stops if the NPU's platform is not yet listed in its data. When a reviewed recipe in `data/builds.toml` provides a newer OpenVINO version than any matching prebuilt wheel, `resolve` reports the recipe. Pass `--build` to compile and install it; see [Local builds](local-builds.md).

## Override flags

- `--openvino-only`: Install OpenVINO without ONNX Runtime; see [Without ONNX Runtime](#without-onnx-runtime).
- `--min-openvino VERSION`: Raise the minimum OpenVINO version floor.
- `--ignore-driver`: Allow an OpenVINO version newer than the installed driver's recorded pairing. Use this only if you accept the compatibility risk.
- `--allow-unverified`: Allow `install` to accept an artifact that upstream sources agree on even though it is not yet recorded in the ledger.
- `--allow-downgrade`: Allow `install` to replace a newer version already present in the prefix.

## Without ONNX Runtime

Programs that call OpenVINO directly (through its C API, the `openvino` crate, or `openvino-sys`) do not need ONNX Runtime and are not limited by Intel's `onnxruntime-openvino` wheel releases:

```bash
ovfetch resolve --openvino-only
ovfetch install --openvino-only --prefix ~/.local/lib/app/openvino
```

This installs Intel's PyPI `openvino` wheel directly—selecting the newest OpenVINO release between the floor and ceiling with the same source-agreement and ledger verification, and without compiling from source. It provides the same library set that `--build` extracts from the wheel (runtime, C API, CPU/GPU/NPU/AUTO/HETERO plugins, ONNX frontend, and TBB): 139 MB, installing in 17 seconds on a Dell XPS 13 (Core 5 320, measured 2026-10-18).

Within a given OpenVINO release, `ovfetch` prefers a wheel already recorded in the ledger over a newer CPython tag; both wheels contain identical native libraries, and the recorded wheel installs as verified. In the resulting lockfile, `artifact.onnxruntime` is omitted and `artifact.kind` is `openvino`.
