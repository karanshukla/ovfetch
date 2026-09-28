# Security

ovfetch downloads native libraries that other programs then load, often as root (Gaze's `gazed`, PAM modules). A compromise here is a compromise of everything downstream, so the bar is higher than for a normal CLI.

## Reporting

Use [private vulnerability reporting](https://github.com/karanshukla/ovfetch/security/advisories/new). Do not open a public issue for anything exploitable.

An **Audit** issue labelled `audit-failure` is public by design: it means an upstream hash changed or sources disagree, which users need to know about immediately.

## Threat model

| Threat | Defence |
|---|---|
| Tampered mirror or CDN edge | Every reachable source must agree on the sha256, quorum of two. One disagreement aborts. |
| Upstream artifact replaced after release | The ledger pins each hash on first sight and ships compiled in. A changed hash aborts, and the daily audit opens an issue. |
| Malicious new upstream release | A release only enters the ledger after 7 days in public, through a reviewed PR that links each wheel to Intel's matching intel/onnxruntime release. Until then it is *unverified* and `install` refuses it by default. |
| DNS spoofing / MITM | HTTPS only, rustls with compiled-in roots, fixed host allowlist re-checked after redirects. |
| Downloaded bytes differ from what the index claims | Bytes are hashed while streaming and must match the agreed hash. The audit also spot-downloads from random mirrors. |
| Local tampering after install | `ovfetch verify` re-hashes the prefix against the `SHA256SUMS` written at install. |
| Compromised ovfetch release | Tags are immutable, releases are immutable, binaries carry signed build provenance (`gh attestation verify`). |
| Stolen crates.io token | There is none: crates.io uses trusted publishing from the release workflow, and that job waits for the maintainer's approval in the `crates-io` environment. |
| PR that weakens the checks (hosts, mirrors, hash agreement, ledger, CI) | The `guard` check fails any PR from someone other than the owner that touches `src/`, `data/`, `.github/`, dependencies, or this file. It runs from main's copy (`pull_request_target`) and never executes PR code, so a PR cannot edit it to pass. Discover's bot PRs may only add ledger entries, never change or remove one. Only the owner's ruleset bypass can merge a flagged PR. |
| Compromised dependency or action | Crates from crates.io only (cargo-deny), `Cargo.lock` enforced, every action pinned to a commit, Dependabot waits 7 days and nothing auto-merges. |

## What it does not defend against

- **A compromise at PyPI itself that goes unnoticed for 7 days.** The ledger is trust-on-first-use and every mirror copies PyPI, so they would all agree. The cooldown and the review against Intel's release notes are the only checks at that point. Intel's wheels carry no PyPI provenance attestation (checked 2026-09-27), so nothing independent can prove Intel's pipeline built them; Discover flags the day that changes.
- **Intel shipping a malicious build.** ovfetch verifies that you got what Intel published, not that what Intel published is safe.
- **A compromised maintainer account.** Rulesets and review slow this down; they do not stop someone holding the keys.
