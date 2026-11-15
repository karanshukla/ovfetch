# Local builds

`--build` compiles ONNX Runtime against a newer OpenVINO than Intel's wheels
bundle, from a reviewed recipe in `data/builds.toml`.

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

## What a build needs

Building requires Git, CMake, Ninja, Python 3, and a C++ compiler. On a Dell
XPS 13 (Core 5 320), a build with `--jobs 3` took 17 minutes (measured
2026-10-12) and used several gigabytes under `~/.cache/ovfetch/build`. Use
`--build-dir` to change the build directory. Source checkouts and the OpenVINO
wheel are cached, but the checkout is re-checked and the wheel re-hashed and
re-extracted on each run. Each build starts clean because compiled objects
from earlier runs cannot be verified. By default, `--jobs` uses half the
available CPUs; ONNX Runtime compilation can use substantial memory.
