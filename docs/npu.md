# NPU compiler and driver

`ovfetch npu install` installs Intel's NPU compiler—and, when needed, its user-mode driver—into a local prefix that applications can load from. Other `vino*` apps depend on the layout documented here: the default prefix (`~/.local/lib/ovfetch-npu`), the `ovfetch-preload.txt` file, and the exact order of lines in it.

Many Linux distributions ship the NPU user-mode driver without its compiler, or do not package the driver at all. `npu install` fetches these libraries from Intel's `linux-npu-driver` release tarballs without `dpkg` or root privileges:

```bash
ovfetch npu install            # installs into ~/.local/lib/ovfetch-npu, which detect also reads
ovfetch detect                 # npu_compiler: true
```

The release tarball is verified against the ledger (because GitHub is its only source, an unrecorded release requires `--allow-unverified`). Packages are unpacked in-process and only `lib*.so*` files are kept. The prefix includes a `SHA256SUMS` file so `ovfetch verify` works on it.

## What gets installed

| Host driver | Action taken by `npu install` |
|---|---|
| Compiler already found | Nothing is installed. |
| `1.32` and older (loads `libnpu_driver_compiler.so` by soname) | Installs the compiler from the matching driver release. |
| `1.35` and newer | Installs both the driver and compiler from the same release, replacing the host driver for apps that preload the prefix. Because driver `1.35+` opens `libopenvino_intel_npu_compiler_loader.so` by absolute path from its own directory, a standalone compiler in another directory cannot be found. |
| None installed | Installs the newest release verified for this NPU's platform (or the release specified by `--driver`). Requires the host's Level Zero loader (`libze_loader.so.1`, from the distro's `level-zero` package). |

- Pass `--full` to install both the driver and compiler even when the compiler alone would suffice.
- Pass `--driver` to select a specific release. Without `--full`, it must match the host driver's version.

The kernel driver and firmware remain managed by the host, since firmware is loaded from `/lib/firmware` and requires root privileges.

## Preloading the prefix from an app

Because the prefix's libraries are not on the system linker search path, applications load them explicitly. `ovfetch-preload.txt` lists each library in required load order: absolute paths for libraries inside the prefix, and a soname (`libze_loader.so.1`) for the host's Level Zero loader.

Load each entry with `dlopen` using `RTLD_GLOBAL` **after** OpenVINO's own libraries are loaded (the NPU compiler links against `libtbb.so.12`, which OpenVINO provides) and **before** OpenVINO initializes the NPU. When the driver later requests those libraries by soname, the dynamic linker returns the copies already loaded in process memory.

Example in Python:

```python
import ctypes, openvino as ov
for line in open(f"{prefix}/ovfetch-preload.txt"):
    ctypes.CDLL(line.strip(), mode=ctypes.RTLD_GLOBAL)
core = ov.Core()
```

**Load order matters:** loading the NPU driver before the Level Zero loader caused a crash on its first call into the loader (driver `1.35.0`, measured 2026-10-24). Launching the app with `LD_LIBRARY_PATH=<prefix>` also works.

### Verified combinations

On a Dell XPS 13 (Wildcat Lake, Fedora kernel driver), OpenVINO `2026.4.1` compiled and ran a test model on the NPU with each of the following setups (measured 2026-10-24):

- Host `1.32.0` driver without a host compiler, paired with the preloaded `1.32.0` compiler (Fedora's `1.32.0` RPM configuration; without the compiler, compilation fails with `ZE_RESULT_ERROR_UNSUPPORTED_FEATURE`).
- A full `1.35.0` prefix install.
- A full `1.32.0` prefix install.

## How detect finds the compiler

`detect` inspects the installed driver binary for the compiler library names it references, then verifies whether those libraries exist on the linker path or in the prefix. These names changed between driver releases:

| Driver | Library names referenced in binary | Measured |
|---|---|---|
| `1.32.0` | `libnpu_driver_compiler.so` only | 2026-09-28 |
| `1.35.0` | The OpenVINO compiler loader first (stored split in the binary as `libopenvino_intel_npu_compiler_l`), which loads `libopenvino_intel_npu_compiler.so`, followed by the `1.32` name | 2026-09-28 |

On `1.35.0`, the driver opens `libopenvino_intel_npu_compiler_loader.so` by absolute path relative to its own directory rather than by soname (measured 2026-10-24 with `LD_DEBUG`). Any copy placed elsewhere—whether preloaded or on `LD_LIBRARY_PATH`—is ignored, which is why `detect` only counts a prefix compiler for `1.35.0+` when the driver itself is also in the prefix.
