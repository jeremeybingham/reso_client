# Server compatibility

What has been run against which RESO Web API server, and what is known only from the vendor's public documentation.
A row moves to **verified** only after the checks in [#76](https://github.com/jeremeybingham/reso_client/issues/76)
pass against that server; access is tracked in [#32](https://github.com/jeremeybingham/reso_client/issues/32).

Status values:

- **verified**: the #76 checks were run against the server and passed (date and version given).
- **local only**: run against a copy of the server on this machine, not a hosted service.
- **docs only**: configuration taken from the vendor's public documentation; never run.
- **unknown**: nothing checked yet.

| Server | Status | Evidence | How to configure | Notes |
|---|---|---|---|---|
| RESO reference server (tools.reso.org, Data Dictionary 2.0 and 2.1) | local only | [#35](https://github.com/jeremeybingham/reso_client/issues/35), 2026-11: full `sync`, repeat run, interrupted run resumed, changed and deleted records, `Media`; `live_reference_server` tests | Defaults; `type = "bearer"` with any token | No `/replication`, no `/Resource/$count`; `$top` cap 2000 |
| Bridge Interactive | unknown | Examples are written against its public `actris_ref` dataset, but none has been run (no credentials) | `dataset_id`; vendor mode `/replication` (`ReplicationQuery`) | `actris_ref` has no `$count`, `$apply`, `$expand` or replication |
| MLS Grid (API 2.0) | docs only | Public docs read 2026-11-24 ([link](https://docs.mlsgrid.com/api-documentation/api-version-2.0)) | `type = "bearer"`; `filter = "OriginatingSystemName eq '…'"`; `deletes = { flag_field = "MlgCanView" }`; `[server.limits]` 2/s, 7200/h, 40000/day, 4 GB/h; `sync --media` (single-use URLs, 1 h) | Media only by `$expand`; 5000 per page, 1000 with `$expand` |
| Trestle (CoreLogic) | docs only | Public docs read 2026-11-24 ([link](https://trestle-documentation.corelogic.com/web-api/)) | OAuth2 client credentials; deletes by key comparison (`--deletes`); `per_second = 3` keeps 180/min; `media` command | `$top` max 1000 (300000 key-only) |
| Spark (FBS) | unknown | | | |
| Others (Paragon, Rapattoni, CoreLogic Matrix, ...) | unknown | | | |

When a server is checked, add the date, the `reso-client`/`reso-ingest` version or commit, the #76 sections run and
anything that differed from the standard, and link the issue comment with the full results. Do not commit a vendor's
`$metadata` or data without its permission.
