# Local builds

`--build` compiles ONNX Runtime against a newer OpenVINO release than Intel's prebuilt wheels bundle, using a reviewed recipe from `data/builds.toml`.

Intel's `onnxruntime-openvino` wheels can lag behind OpenVINO itself. As of 2026-10-12, the newest wheel bundled OpenVINO `2025.4.1`, while OpenVINO was at `2026.4.1`. Running different OpenVINO releases concurrently on the same NPU can trigger a device fault that resets the NPU for every process using it. Local builds let `ovfetch` compile ONNX Runtime against a newer OpenVINO release while keeping version selection and provenance checks in one place.

```bash
ovfetch resolve --build
ovfetch install --build --prefix ~/.local/lib/app/ort
```

`resolve --build` displays the recipe and its pinned inputs without compiling. `install --build` compiles and installs the selected build.

## Recipe checks

Recipes in `data/builds.toml` pin every build input. Before compiling, `install --build` verifies:

| Input | Verification |
|---|---|
| ONNX Runtime source | The recipe's Git tag must resolve to its pinned commit. `resolve` checks the tag via GitHub, and the cloned checkout must have the exact pinned commit at `HEAD`. Dirty checkouts with local modifications are rejected. |
| OpenVINO | The PyPI `openvino` wheel (including headers, CMake config, and libraries) is verified against the same upstream sources and ledger as other artifacts, ensuring the build uses the same OpenVINO libraries as Python applications on the machine. |
| Build flags | Compiler and CMake flags are fixed in the reviewed recipe. |

## Verification and isolation

- **Hashing:** For a local build, the plan's `sha256` covers the recipe inputs rather than the compiled output, since binaries can vary across compilers. The lockfile records the compiler and CMake versions used, and `SHA256SUMS` records the installed files so `ovfetch verify` works the same as for prebuilt installations.
- **Library search paths:** Installed libraries must search only `$ORIGIN` for dependencies so they cannot load libraries from the build tree later.
- **Build directory safety:** `--build` rejects build directories not owned by the current user or writable by other users, and runs Git with system, global, and hook configurations disabled.
- **CMake sub-downloads:** ONNX Runtime's CMake configuration downloads some third-party dependencies directly outside `ovfetch`'s host allowlist. CMake verifies those files against SHA-1 hashes in the pinned source's `cmake/deps.txt`.

## Build requirements

Compiling requires Git, CMake, Ninja, Python 3, and a C++ compiler.

- **Time and disk usage:** On a Dell XPS 13 (Core 5 320), a build with `--jobs 3` took 17 minutes (measured 2026-10-12) and used several gigabytes under `~/.cache/ovfetch/build`. Use `--build-dir` to choose a different build directory.
- **Caching:** Source checkouts and the OpenVINO wheel are cached, though the checkout is re-verified and the wheel is re-hashed and re-extracted on each run. Compilation itself always starts from a clean directory because object files from earlier runs cannot be verified.
- **Parallelism:** By default, `--jobs` uses half of the available CPU cores because compiling ONNX Runtime consumes substantial memory.
