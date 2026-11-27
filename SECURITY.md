# Security

## Reporting a vulnerability

Please do not open a public issue. Use GitHub's private reporting: **Security → Report a vulnerability** on
this repository. Include the affected version, how to reproduce it and the impact. Expect a first reply
within a week; this is a one-person project with no SLA (see [Support](docs/OPERATIONS.md#support)).

Supported versions: the latest `reso-client` release and `main` of `reso-ingest`.

## Supply chain

Checked in CI on every pull request and push to `main` ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)):

- `cargo audit` against the RustSec advisory database (ignores in [`.cargo/audit.toml`](.cargo/audit.toml), each with
  its reason and issue).
- `cargo deny` ([`deny.toml`](deny.toml)): permissive licences only, crates.io as the only source, no wildcard
  versions, advisories for direct dependencies.
- `cargo semver-checks` on `reso-client` against its last release (see [`docs/STABILITY.md`](docs/STABILITY.md)).

New dependencies need the owner's go-ahead ([`AGENTS.md`](AGENTS.md)). The container image is a static binary on
`scratch` with no shell or package manager; its only C code is the `ring` and `aws-lc` crypto libraries.
SBOMs and signed tags and artifacts are planned in
[#158](https://github.com/jeremeybingham/reso-rs/issues/158).
