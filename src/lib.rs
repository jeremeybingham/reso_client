// src/lib.rs

//! Client for [RESO Web API](https://www.reso.org/reso-web-api/) servers (OData 4.0)
//!
//! `reso-client` talks to one server: it builds OData queries, follows server-driven paging,
//! replicates resources the RESO-standard way, authenticates with a bearer token or OAuth2 client
//! credentials, retries and paces requests, parses `$metadata`, and downloads files such as
//! `Media` photos. Records come back as raw [`serde_json::Value`]s: resource and field names are
//! strings, so any server schema works.
//!
//! Pulling many servers into files is the job of the `reso-ingest` crate in the same repository.
//! The crate is pre-1.0; breaking changes are listed in the changelog.
//!
//! # Quick start
//!
//! ```no_run
//! use reso_client::{QueryBuilder, ResoClient};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // RESO_BASE_URL plus RESO_TOKEN, or RESO_CLIENT_ID, RESO_CLIENT_SECRET and RESO_TOKEN_URI
//!     let client = ResoClient::from_env()?;
//!
//!     let query = QueryBuilder::new("Property")
//!         .filter("City eq 'Austin' and ListPrice gt 500000")
//!         .select(&["ListingKey", "City", "ListPrice"])
//!         .order_by("ListPrice", "desc")
//!         .top(10)
//!         .build()?;
//!
//!     // The raw OData response: { "value": [...], "@odata.nextLink": "..." }
//!     let results = client.execute(&query).await?;
//!     for record in results["value"].as_array().into_iter().flatten() {
//!         println!("{} {}", record["ListingKey"], record["ListPrice"]);
//!     }
//!     Ok(())
//! }
//! ```
//!
//! # Configuration
//!
//! [`ResoClient::from_env`] reads `RESO_BASE_URL`, then `RESO_TOKEN` or the OAuth2 variables
//! (`RESO_CLIENT_ID`, `RESO_CLIENT_SECRET`, `RESO_TOKEN_URI`, optional `RESO_SCOPE` and
//! `RESO_CREDENTIAL_TRANSPORT`), and the optional `RESO_TIMEOUT`, `RESO_MAX_RETRIES`,
//! `RESO_RATE_PER_SECOND`, `RESO_RATE_PER_HOUR`, `RESO_RATE_PER_DAY`, `RESO_BYTES_PER_HOUR`,
//! `RESO_BYTES_PER_DAY` and `RESO_DATASET_ID`
//! (Bridge Interactive only). The same settings in code:
//!
//! ```no_run
//! # use std::time::Duration;
//! use reso_client::{ClientConfig, ClientCredentials, RateLimit, ResoClient, RetryPolicy};
//!
//! let credentials =
//!     ClientCredentials::new("https://auth.mls.example/token", "client-id", "client-secret");
//! let config = ClientConfig::with_auth("https://api.mls.example/odata", credentials)
//!     .with_timeout(Duration::from_secs(60))
//!     .with_retry(RetryPolicy::new(4))
//!     .with_rate_limit(RateLimit::default().per_second(2));
//! let client = ResoClient::with_config(config)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! With client credentials the token is fetched on first use, renewed after 90% of its lifetime
//! and once after a 401. Retries ([`RetryPolicy`]) and rate limits ([`RateLimit`]) are off by
//! default. Tokens and secrets are never logged and are redacted from `Debug` output.
//!
//! # Paging through a result set
//!
//! The server picks the page size and sends a next link while records remain.
//! [`ResoClient::execute_page`] returns the first [`Page`] and [`ResoClient::next_page`] the
//! next one, or `None` after the last. Links in the body (`@odata.nextLink`), a `next` header
//! and a `Link` header are all read.
//!
//! ```no_run
//! # use reso_client::{ResoClient, QueryBuilder};
//! # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
//! let query = QueryBuilder::new("Property").filter("StandardStatus eq 'Active'").build()?;
//! let mut page = client.execute_page(&query).await?;
//! let mut total = page.records.len();
//! while let Some(next) = client.next_page(&page).await? {
//!     total += next.records.len();
//!     page = next;
//! }
//! println!("{total} records");
//! # Ok(())
//! # }
//! ```
//!
//! # Replication
//!
//! [`StandardReplication`] walks a resource by `ModificationTimestamp` and key. Save
//! [`ReplicationRun::checkpoint`] after each batch and pass it to
//! [`StandardReplication::from_checkpoint`] to continue later. [`StandardReplication::expand`]
//! sends `$expand` on every page, for servers that serve related records (such as `Media`) only
//! inline.
//!
//! ```no_run
//! # use reso_client::{ResoClient, StandardReplication};
//! # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
//! let mut run = client.replicate(StandardReplication::new("Property", "ListingKey").page_size(1000));
//! while let Some(batch) = run.next_batch().await? {
//!     println!("{} records, up to {:?}", batch.len(), run.checkpoint());
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Bridge Interactive's `/replication` endpoint is a separate vendor mode:
//! [`ReplicationQueryBuilder`] and [`ResoClient::execute_replication`].
//!
//! # Counts and metadata
//!
//! ```no_run
//! # use reso_client::{ResoClient, QueryBuilder};
//! # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
//! // A count with the records (or with .top(0) for the count alone)
//! let query = QueryBuilder::new("Property").with_count().top(0).build()?;
//! let total = client.execute(&query).await?["@odata.count"].as_u64();
//!
//! // What the server offers: entity sets, keys, properties, enums
//! let model = client.fetch_metadata_model().await?;
//! for name in model.resource_names() {
//!     println!("{name}");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! [`ResoClient::execute_count`] sends `$count=true&$top=0` and reads `@odata.count`, so it
//! needs only `$count=true` support, not a `/Resource/$count` route.
//!
//! # File downloads
//!
//! [`ResoClient::downloader`] returns a [`Downloader`] for files such as a `Media` record's
//! `MediaURL`. It shares the client's token but has its own [`RetryPolicy`], [`RateLimit`] and
//! timeout ([`DownloadConfig`]), so downloads do not use up the API's budget. Each download is a
//! conditional GET written to `<dest>.part` and renamed when complete; pass the [`Validators`]
//! from the last download to get [`DownloadOutcome::NotModified`] for an unchanged file. The token
//! goes only to the API's own host unless [`SendToken`] says otherwise.
//!
//! ```no_run
//! # use reso_client::{DownloadConfig, DownloadOutcome, ResoClient, Validators};
//! # async fn example(client: &ResoClient) -> Result<(), Box<dyn std::error::Error>> {
//! let downloader = client.downloader(DownloadConfig::default())?;
//! let url = "https://cdn.mls.example/photo/1.jpg";
//! match downloader.download_to(url, "media/1".as_ref(), &Validators::default()).await? {
//!     DownloadOutcome::Saved(file) => println!("{} bytes, {:?}", file.bytes, file.etag),
//!     DownloadOutcome::NotModified => println!("unchanged"),
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Errors
//!
//! Every call returns [`ResoError`], so a bad token (`Unauthorized`), a rate limit
//! (`RateLimited`), a server-side OData error (`ODataError`) and a network fault (`Network`) can
//! be told apart; [`ResoError::Io`] covers files that cannot be written. The enum is
//! `#[non_exhaustive]`.
//!
//! # Growing types
//!
//! Configuration and result structs (`ClientConfig`, `RateLimit`, `ClientStats`, the
//! [`Metadata`] types and others) are `#[non_exhaustive]` so that new fields are not breaking
//! changes. Their fields stay public to read and assign, but outside this crate they are built
//! with constructors and builders rather than struct literals:
//!
//! ```
//! # use reso_client::{RateLimit, Validators};
//! let limit = RateLimit::default().per_second(2).per_hour(7_200);
//! let mut validators = Validators::default();
//! validators.etag = Some("\"v1\"".into());
//! # let _ = (limit, validators);
//! ```
//!
//! # Server support
//!
//! `$count`, `$expand`, `$apply` and the paths servers accept vary. Tests run against mock
//! servers and RESO's reference server; vendor servers are not yet confirmed.
//!
//! # Resources
//!
//! - [RESO Web API](https://www.reso.org/reso-web-api/)
//! - [RESO Data Dictionary](https://www.reso.org/data-dictionary/)
//! - [OData 4.0](https://www.odata.org/documentation/)

pub mod auth;
pub mod client;
pub mod download;
pub mod error;
pub mod metadata;
pub mod network;
pub mod paging;
pub mod queries;
pub mod replication;
pub mod retry;
pub mod standard_replication;

// Re-export main types for convenience
pub use auth::{Auth, ClientCredentials, CredentialTransport};
pub use client::{ClientConfig, ResoClient};
pub use download::{
    DownloadConfig, DownloadOutcome, DownloadTarget, Downloader, SavedFile, SendToken, Validators,
};
pub use error::{ResoError, Result};
pub use metadata::{
    EntitySet, EntityType, EnumMember, EnumType, Metadata, NavigationProperty, Property,
};
pub use network::NetworkOptions;
pub use paging::Page;
pub use queries::{Query, QueryBuilder, ReplicationQuery, ReplicationQueryBuilder};
pub use replication::ReplicationResponse;
pub use retry::{count_requests, ClientStats, RateLimit, RateUsage, RetryPolicy, WindowUsage};
pub use standard_replication::{Checkpoint, ReplicationRun, StandardReplication};

// Re-export serde_json for convenience
pub use serde_json::Value as JsonValue;
