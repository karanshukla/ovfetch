# Security

`ovfetch` downloads native libraries that downstream programs load into memory, sometimes as root (such as PAM modules like vinoAuthFace's `face-auth`). Because a compromise in `ovfetch` affects every application that relies on it, its verification checks are strict.

## Reporting

Report vulnerabilities through [private vulnerability reporting](https://github.com/karanshukla/ovfetch/security/advisories/new). Do not open a public issue for anything exploitable.

An **Audit** issue labelled `audit-failure` is public by design: it indicates that an upstream hash changed or that sources disagree, which users need to know immediately.

## Threat model

| Threat | Mitigation |
|---|---|
| Tampered mirror or CDN edge | Every reachable source must agree on the SHA-256 hash, with a quorum of at least two sources. Any disagreement aborts the operation. |
| Upstream artifact replaced after release | The ledger pins each hash when first recorded and is compiled into the binary. A changed hash aborts installation, and the daily audit opens an issue. |
| Malicious new upstream release | A release enters the ledger only after 7 days in public, via a reviewed PR linking each wheel to Intel's matching `intel/onnxruntime` release. Until then it is marked *unverified* and `install` rejects it by default. |
| DNS spoofing or MITM | All downloads use HTTPS with `rustls` compiled-in root certificates and a fixed host allowlist that is re-checked after redirects. |
| Downloaded bytes differ from index metadata | Bytes are hashed while streaming and must match the agreed SHA-256 hash. The daily audit also spot-downloads samples from random mirrors. |
| Local tampering after install | `ovfetch verify` re-hashes the prefix against the `SHA256SUMS` file written at install time and fails on any unexpected file. If an install target already contains unexpected files, `install` aborts without deleting them. |
| Compromised `ovfetch` release | Git tags and published releases are immutable, and release binaries include signed build provenance (`gh attestation verify`). |
| Stolen crates.io token | Crates.io publishing uses trusted publishing from the release workflow instead of a stored API token, and the publish job requires maintainer approval in the `crates-io` environment. |
| PR that weakens checks (hosts, mirrors, hash agreement, ledger, CI) | The `guard` check fails any PR from someone other than the repository owner that modifies `src/`, `data/`, `.github/`, dependencies, or this file. It runs from `main`'s copy (`pull_request_target`) without executing PR code, so a PR cannot edit it to pass. Discover's bot PRs may only add ledger entries, never modify or remove existing ones. Only the owner's ruleset bypass can merge a flagged PR. |
| Compromised dependency or GitHub Action | Crates are restricted to crates.io (`cargo-deny`), `Cargo.lock` is enforced, every GitHub Action is pinned to a commit SHA, Dependabot waits 7 days before opening PRs, and nothing auto-merges. |

## Out of scope

- **A compromise of PyPI itself that goes unnoticed for 7 days.** The ledger is trust-on-first-use and mirrors replicate PyPI, so all sources would agree on a compromised package. The 7-day cooldown and manual review against Intel's release notes are the only defenses in that window. Intel's wheels do not carry PyPI provenance attestations (checked 2026-09-27), so nothing independent can prove Intel's CI built them; the Discover workflow flags the day that changes.
- **Intel shipping a malicious build.** `ovfetch` verifies that downloaded files match what Intel published; it cannot guarantee that Intel's builds are free of defects or malicious code.
- **A compromised maintainer account.** Repository rulesets and review slow down unauthorized changes, but cannot stop someone who holds maintainer credentials.
