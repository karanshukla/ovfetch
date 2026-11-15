# Choosing a build

How `ovfetch resolve` and `ovfetch install` pick an OpenVINO version, and the
flags that change the pick.

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
`ovfetch npu install` fetches it; see [NPU compiler and driver](npu.md).

If no prebuilt build fits, `ovfetch` stops and reports which bound is blocking
it. It also refuses an NPU whose platform is not yet in its data rather than
guessing. When a reviewed recipe in `data/builds.toml` offers a newer OpenVINO
version than any fitting prebuilt, `resolve` reports the recipe. Pass `--build`
to compile and install it; see [Local builds](local-builds.md).

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

## Without ONNX Runtime

Programs that call OpenVINO directly (through its C API, the `openvino` crate, or `openvino-sys`) don't need ONNX Runtime, and so aren't held back by Intel's onnxruntime-openvino wheels:

```bash
ovfetch resolve --openvino-only
ovfetch install --openvino-only --prefix ~/.local/lib/app/openvino
```

This installs Intel's PyPI `openvino` wheel alone: the newest OpenVINO between the floor and the ceiling, through the same source agreement and ledger, and never a compile. It is the same library set `--build` takes from that wheel (runtime, C API, CPU/GPU/NPU/AUTO/HETERO plugins, ONNX frontend, TBB), 139 MB, installed in 17 seconds on a Dell XPS 13 (Core 5 320, measured 2026-10-18). Within a release, a wheel the ledger already records is preferred over a newer CPython tag; the libraries are the same build, and a recorded one installs as verified. The lock has no `artifact.onnxruntime`, and `artifact.kind` is `openvino`.
