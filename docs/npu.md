# NPU compiler and driver

`ovfetch npu install` puts Intel's NPU compiler, and when needed its user-mode
driver, into a prefix an app can load from. Other programs depend on what this
page describes: the default prefix `~/.local/lib/ovfetch-npu`, the
`ovfetch-preload.txt` file, and the order of its lines.

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

## How detect finds the compiler

`detect` reads the driver's own binary for the compiler library names it
carries, then checks that those libraries are on the linker path or in the
prefix. The names changed between releases:

| Driver | What its binary names | Measured |
|---|---|---|
| 1.32.0 | `libnpu_driver_compiler.so` only | 2026-09-28 |
| 1.35.0 | The OpenVINO compiler loader first (stored split in the binary, as `libopenvino_intel_npu_compiler_l`), which maps `libopenvino_intel_npu_compiler.so` in turn; then the 1.32 name | 2026-09-28 |

On 1.35.0 the driver opens `libopenvino_intel_npu_compiler_loader.so` by
absolute path from its own directory, not by soname (measured 2026-10-24 with
`LD_DEBUG`). A copy anywhere else, preloaded or on `LD_LIBRARY_PATH`, is never
found, which is why that route only counts when the driver is in the prefix
too.
