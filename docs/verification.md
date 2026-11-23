# Verification

This page describes how `ovfetch` verifies artifacts before installing them, how to verify `ovfetch` releases, and the automated workflows that maintain its ledger and compatibility data.

## Artifact verification

Before installing an artifact, `ovfetch` verifies its integrity in four steps:

1. **Independent sources agree.** PyPI and two independent mirrors report the wheel's SHA-256 hash. At least two different hosts must respond, and every responding source must agree. Any disagreement aborts the install and displays each source's claim.
2. **The ledger matches.** `data/ledger.toml` records each artifact's hash the first time it is seen and is compiled into the binary. A changed hash is rejected even if all current online sources agree.
3. **The downloaded bytes match.** `ovfetch` downloads from PyPI, falling back to a mirror only when PyPI is unreachable. It computes the SHA-256 hash while streaming bytes and rejects any mismatch.
4. **Connections are restricted.** Downloads use HTTPS against a fixed host allowlist. Hosts are re-verified after redirects, and TLS root certificates are compiled into the binary via `rustls`.

An artifact that all sources agree on but that is not yet in the ledger is marked *unverified*. `install` rejects unverified artifacts unless you pass `--allow-unverified`.

Mirrors catch a tampered CDN edge or a compromised mirror, though because mirrors replicate PyPI they cannot detect a compromise of PyPI itself. The compiled-in ledger protects against post-release tampering on PyPI.

## Verifying a release

Release binaries are statically linked (musl) and ship with signed build provenance:

```bash
gh attestation verify ovfetch-x86_64-linux --repo karanshukla/ovfetch
```

Release tags cannot be moved or deleted, and published releases are immutable.

## Keeping the data current

Three GitHub Actions workflows maintain the version and platform data:

- **Discover** (weekly): Records new `onnxruntime-openvino` wheels after they have been public for seven days and links each wheel to Intel's matching `intel/onnxruntime` release. It also tracks `openvino` wheels from `2025.4` onward, records new NPU driver releases along with their OpenVINO pairing and asset hashes, and tracks new NPU PCI IDs from the Linux kernel's `ivpu` driver. Discover opens a pull request; changes reach users only after manual review and a release. It also detects when PyPI begins publishing provenance attestations for `onnxruntime-openvino` wheels.
- **Guard** (every PR): Blocks changes to source code, data files, dependencies, or CI configuration from anyone other than the repository owner. It runs from the default branch's copy, so a pull request cannot alter the check to bypass it.
- **Audit** (daily): Re-checks every ledger hash against every upstream source and downloads a random sample from a random mirror. Any mismatch opens a public issue. A `--build` recipe whose GitHub repository is no longer served (checked 2026-11-23: `microsoft/onnxruntime` returned 404) is reported as unavailable, not as a mismatch, and does not open one; a tag deleted from a repository that is still there does.
