# Verification

What `ovfetch` checks before it installs an artifact, how to check `ovfetch`
itself, and the workflows that keep its data honest.

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

## Verifying a release

Release binaries are static (musl) and include signed build provenance:

```bash
gh attestation verify ovfetch-x86_64-linux --repo karanshukla/ovfetch
```

Release tags cannot be moved or deleted, and published releases are immutable.

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
