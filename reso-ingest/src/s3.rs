//! Object storage backends: S3-compatible buckets and Google Cloud Storage (feature `s3`)
//!
//! [`S3StateStore`] keeps ingest state (checkpoints, key sets, leases, media indexes) as objects
//! under a prefix in a bucket, so several workers can share it. It works with AWS S3, Cloudflare R2,
//! MinIO and RustFS through the S3 API, and with Google Cloud Storage through its own API
//! ([`Provider::Gcs`]), using the `object_store` crate (rustls, no OpenSSL). Conditional writes
//! are real HTTP preconditions: on S3, `create` is a put with `If-None-Match: *` and `replace` a
//! put with `If-Match` on the ETag; on GCS both use `x-goog-if-generation-match` on the object
//! generation, and a versioned delete is a `DELETE` with the same precondition. So a lease is held
//! by exactly one worker. [`S3FileSink`] uploads finished output
//! files to the same kind of bucket.
//!
//! GCS through its S3 interoperability endpoint is refused: it accepts the S3 precondition headers
//! and ignores them (#115, #130).

use std::fmt;
use std::sync::Arc;

use futures::TryStreamExt;
use object_store::aws::{AmazonS3, AmazonS3Builder, AmazonS3ConfigKey};
use object_store::gcp::{GcpCredentialProvider, GoogleCloudStorageBuilder};
use object_store::path::Path as ObjectPath;
use object_store::signer::{HeaderName, HeaderValue, Method, SignedUrlOptions, Signer};
use object_store::{
    ClientOptions, Error as ObjectError, ObjectMeta, ObjectStore, ObjectStoreExt, PutMode,
    PutPayload, UpdateVersion, WriteMultipart,
};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::IngestError;
use crate::sink::FileSink;
use crate::state::{StateStore, StoreFuture, Versioned};

/// An error with its causes, `outer: cause: …` (reqwest says only `builder error` at the top
/// when, for example, there is no CA bundle; the reason is further down). A cause whose text the
/// outer message already contains is not repeated.
fn chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

/// Which API a bucket is reached through
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Provider {
    /// The S3 API: AWS S3, Cloudflare R2, MinIO, RustFS. Versions are ETags.
    #[default]
    S3,
    /// Google Cloud Storage's own API. Versions are object generations, and a versioned delete is
    /// a conditional `DELETE` on the server.
    Gcs,
}

/// Where a bucket is and how to reach it
///
/// Credentials are not part of the options: they come from the standard environment, so they
/// never sit in a configuration file. For S3: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
/// `AWS_SESSION_TOKEN`, `AWS_REGION`, `AWS_ENDPOINT`, and web-identity or instance credentials
/// where the platform provides them. For GCS: `GOOGLE_SERVICE_ACCOUNT` (path of a service-account
/// JSON key), `GOOGLE_SERVICE_ACCOUNT_KEY` (the key itself), `GOOGLE_APPLICATION_CREDENTIALS`, or
/// the instance metadata server on Google Cloud.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct S3Options {
    /// The API: S3 (default) or GCS
    pub provider: Provider,
    /// The bucket
    pub bucket: String,
    /// Key prefix inside the bucket; empty for the bucket root
    pub prefix: String,
    /// Endpoint URL of a non-AWS S3 store (R2, MinIO, RustFS); `None` for AWS. With GCS, the base
    /// URL of a test server; `None` for Google's
    pub endpoint: Option<String>,
    /// S3 region; `None` takes `AWS_REGION` or `AWS_DEFAULT_REGION` (R2 and MinIO accept `auto` or
    /// any). Not used with GCS
    pub region: Option<String>,
    /// Allow a plain-`http://` endpoint (a local MinIO); off by default
    pub allow_http: bool,
    /// S3 only: delete with one `DELETE` per object instead of the multi-object `POST ?delete`,
    /// for stores without multi-delete; off by default
    pub single_delete: bool,
}

/// A built client, with what a conditional delete needs
struct Built {
    store: Arc<dyn ObjectStore>,
    versions: Versions,
}

/// The GCS endpoint as an S3 interoperability URL (any scheme, with or without a path)
fn is_gcs_interop(endpoint: &str) -> bool {
    let host = endpoint
        .split("://")
        .nth(1)
        .unwrap_or(endpoint)
        .split(['/', ':'])
        .next()
        .unwrap_or("");
    host.eq_ignore_ascii_case("storage.googleapis.com")
        || host
            .to_ascii_lowercase()
            .ends_with(".storage.googleapis.com")
}

impl S3Options {
    /// Options for `bucket` under `prefix`, everything else from the environment
    pub fn new(bucket: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            prefix: prefix.into(),
            ..Self::default()
        }
    }

    /// Build the object store client for these options
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the options or the environment cannot make a client (no
    /// region or credentials, an invalid endpoint).
    fn build(&self) -> Result<Built, IngestError> {
        let error = |message: String| IngestError::Output {
            path: self.location(""),
            message,
        };
        if self.provider == Provider::Gcs {
            let mut builder = GoogleCloudStorageBuilder::from_env().with_bucket_name(&self.bucket);
            if let Some(endpoint) = &self.endpoint {
                builder = builder.with_base_url(endpoint);
            }
            if self.allow_http {
                builder = builder.with_client_options(ClientOptions::new().with_allow_http(true));
            }
            let gcs = Arc::new(builder.build().map_err(|e| error(chain(&e)))?);
            let delete = GcsDelete {
                credentials: Arc::clone(gcs.credentials()),
                base_url: self
                    .endpoint
                    .clone()
                    .unwrap_or_else(|| GCS_BASE_URL.to_string()),
                bucket: self.bucket.clone(),
                http: reqwest::Client::new(),
            };
            return Ok(Built {
                store: gcs,
                versions: Versions::Generation(Some(Arc::new(delete))),
            });
        }
        if self.endpoint.as_deref().is_some_and(is_gcs_interop) {
            return Err(error(
                "GCS through its S3 interoperability endpoint ignores conditional writes, so leases \
                 and run ids would not be safe; use the GCS backend instead (--gcs-bucket, \
                 Provider::Gcs)"
                    .to_string(),
            ));
        }
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(&self.bucket);
        if let Some(endpoint) = &self.endpoint {
            builder = builder.with_endpoint(endpoint);
        }
        if let Some(region) = &self.region {
            builder = builder.with_region(region);
        }
        if self.allow_http {
            builder = builder.with_allow_http(true);
        }
        if self.single_delete {
            builder = builder.with_config(AmazonS3ConfigKey::DisableBulkDelete, "true");
        }
        let store = Arc::new(builder.build().map_err(|e| error(chain(&e)))?);
        let delete = S3Delete {
            signer: Arc::clone(&store),
            http: reqwest::Client::new(),
        };
        Ok(Built {
            store,
            versions: Versions::ETag(Some(Arc::new(delete))),
        })
    }

    fn location(&self, key: &str) -> String {
        let scheme = match self.provider {
            Provider::S3 => "s3",
            Provider::Gcs => "gs",
        };
        let prefix = self.prefix.trim_matches('/');
        match (prefix.is_empty(), key.is_empty()) {
            (true, true) => format!("{scheme}://{}", self.bucket),
            (true, false) => format!("{scheme}://{}/{key}", self.bucket),
            (false, true) => format!("{scheme}://{}/{prefix}", self.bucket),
            (false, false) => format!("{scheme}://{}/{prefix}/{key}", self.bucket),
        }
    }
}

/// Google's GCS endpoint (XML API), the default base URL
const GCS_BASE_URL: &str = "https://storage.googleapis.com";

/// What the version of a stored value is
#[derive(Clone)]
enum Versions {
    /// The ETag (S3, and the in-memory store); with the client for conditional deletes, or `None`
    /// for HEAD-then-DELETE (the in-memory store)
    ETag(Option<Arc<S3Delete>>),
    /// The object generation (GCS); with the client for conditional deletes, or `None` for
    /// HEAD-then-DELETE (tests)
    Generation(Option<Arc<GcsDelete>>),
}

impl Versions {
    fn of(&self, meta: &ObjectMeta) -> Option<String> {
        match self {
            Versions::ETag(_) => meta.e_tag.clone(),
            Versions::Generation(_) => meta.version.clone(),
        }
    }

    fn update(&self, version: &str) -> PutMode {
        let version = Some(version.to_string());
        PutMode::Update(match self {
            Versions::ETag(_) => UpdateVersion {
                e_tag: version,
                version: None,
            },
            Versions::Generation(_) => UpdateVersion {
                e_tag: None,
                version,
            },
        })
    }
}

/// A `DELETE` with `x-goog-if-generation-match`, which `object_store` does not offer: the
/// generation check and the delete are one step on the server, so a lease cannot be deleted after
/// another worker took it over
struct GcsDelete {
    credentials: GcpCredentialProvider,
    base_url: String,
    bucket: String,
    http: reqwest::Client,
}

impl GcsDelete {
    /// `Ok(true)` when deleted, `Ok(false)` when missing or at another generation
    async fn delete(&self, path: &ObjectPath, generation: &str) -> Result<bool, String> {
        let token = self
            .credentials
            .get_credential()
            .await
            .map_err(|e| chain(&e))?;
        let mut url = reqwest::Url::parse(&self.base_url).map_err(|e| e.to_string())?;
        url.path_segments_mut()
            .map_err(|()| format!("{}: not a base URL", self.base_url))?
            .pop_if_empty()
            .push(&self.bucket)
            .extend(path.parts().map(|p| p.as_ref().to_string()));
        let response = self
            .http
            .delete(url)
            .bearer_auth(&token.bearer)
            .header("x-goog-if-generation-match", generation)
            .send()
            .await
            .map_err(|e| chain(&e))?;
        delete_answer(response).await
    }
}

/// What a conditional `DELETE` answered: `Ok(true)` when deleted, `Ok(false)` when the object is
/// missing or at another version (404, 412)
async fn delete_answer(response: reqwest::Response) -> Result<bool, String> {
    let status = response.status();
    if status.is_success() {
        return Ok(true);
    }
    if status == reqwest::StatusCode::NOT_FOUND
        || status == reqwest::StatusCode::PRECONDITION_FAILED
    {
        return Ok(false);
    }
    let body = response.text().await.unwrap_or_default();
    let body: String = body.chars().take(300).collect();
    Err(format!("DELETE returned {status}: {body}"))
}

/// How long a presigned conditional `DELETE` URL stays valid; it is sent at once
const SIGNED_DELETE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// A `DELETE` with `If-Match` on the ETag, which `object_store` does not offer (#133): the ETag
/// check and the delete are one step on the server, so a lease cannot be deleted after another
/// worker renewed it. The request is a presigned URL from `object_store`'s signer with `If-Match`
/// bound into the signature, so it uses the same credentials, endpoint and addressing style as
/// every other request
struct S3Delete {
    signer: Arc<AmazonS3>,
    http: reqwest::Client,
}

impl S3Delete {
    /// `Ok(true)` when deleted, `Ok(false)` when missing or at another ETag
    async fn delete(&self, path: &ObjectPath, etag: &str) -> Result<bool, String> {
        let if_match = HeaderName::from_static("if-match");
        let value = HeaderValue::from_str(etag).map_err(|e| format!("ETag {etag:?}: {e}"))?;
        let options = SignedUrlOptions::default().with_signed_header(if_match, value.clone());
        let url = self
            .signer
            .signed_url_opts(Method::DELETE, path, SIGNED_DELETE_TTL, &options)
            .await
            .map_err(|e| chain(&e))?;
        let response = self
            .http
            .delete(url)
            .header(reqwest::header::IF_MATCH, value)
            .send()
            .await
            // The URL carries a signature: keep it out of messages
            .map_err(|e| chain(&e.without_url()))?;
        delete_answer(response).await
    }
}

/// A [`StateStore`] in a bucket (S3-compatible or GCS): one object per key under a prefix
///
/// The version of a value is its ETag on S3 and its generation on GCS.
/// [`create`](StateStore::create) and [`replace`](StateStore::replace) are atomic on the server
/// (HTTP preconditions); the bucket must support them (AWS S3 since 2024, R2, MinIO, RustFS and
/// native GCS do). [`delete`](StateStore::delete) with a version is a conditional `DELETE` too:
/// `If-Match` on the ETag on S3 (a presigned request, since `object_store` has no conditional
/// delete, #133), `x-goog-if-generation-match` on GCS. State is small and read rarely, so there is
/// no caching.
///
/// Confirmed against AWS S3 and RustFS (#115) and native GCS (#130).
///
/// # Examples
///
/// ```no_run
/// # fn demo() -> Result<(), reso_ingest::IngestError> {
/// use reso_ingest::{S3Options, S3StateStore};
///
/// // Credentials come from AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY
/// let store = S3StateStore::new(&S3Options::new("my-bucket", "reso/state"))?;
/// # let _ = store; Ok(()) }
/// ```
#[derive(Clone)]
pub struct S3StateStore {
    store: Arc<dyn ObjectStore>,
    prefix: Option<ObjectPath>,
    label: String,
    versions: Versions,
}

impl fmt::Debug for S3StateStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3StateStore")
            .field("location", &self.label)
            .finish()
    }
}

impl S3StateStore {
    /// A store for the bucket and prefix in `options`
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when no client can be built (see [`S3Options`]). Nothing is sent
    /// until the first read or write.
    pub fn new(options: &S3Options) -> Result<Self, IngestError> {
        let built = options.build()?;
        let mut store = Self::with_store(built.store, &options.prefix, options.location(""))?;
        store.versions = built.versions;
        Ok(store)
    }

    /// A store over any `object_store` implementation whose versions are ETags (tests use the
    /// in-memory one)
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when `prefix` is not a valid object path.
    pub fn with_store(
        store: Arc<dyn ObjectStore>,
        prefix: &str,
        label: String,
    ) -> Result<Self, IngestError> {
        let prefix = prefix.trim_matches('/');
        let prefix = if prefix.is_empty() {
            None
        } else {
            Some(ObjectPath::parse(prefix).map_err(|e| IngestError::Output {
                path: label.clone(),
                message: format!("invalid prefix: {e}"),
            })?)
        };
        Ok(Self {
            store,
            prefix,
            label,
            versions: Versions::ETag(None),
        })
    }

    /// Check that the store honours conditional writes (#131)
    ///
    /// Leases, run-id reservations and the sink's never-replace rule rely on the store refusing a
    /// create of an existing object (`If-None-Match: *`) and a replace with a stale version
    /// (`If-Match`), and a delete with a stale version leaving the object in place. Some
    /// S3-compatible stores accept those headers and ignore them (GCS through its S3
    /// interoperability endpoint does, #115), and then two workers can hold one lease without any
    /// error. This writes a throwaway object `.probe/<pid>-<nanos>`, tries to create it again, to
    /// replace it and to delete it with a wrong version, and deletes it: up to three PUTs and two
    /// DELETEs. `sync` and `serve` run it once at start-up.
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when the second create, the stale replace or the stale delete
    /// succeeds, or when the store cannot be reached.
    pub async fn check_conditional_writes(&self) -> Result<(), IngestError> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let key = format!(".probe/{}-{nanos}", std::process::id());
        if !self.create(&key, b"probe").await? {
            return Err(self.error(&key, "the probe object already exists; try again"));
        }
        // A version no object has, in the form the store uses
        let stale = match self.versions {
            Versions::ETag(_) => "\"0\"",
            Versions::Generation(_) => "1",
        };
        let ignored = if self.create(&key, b"probe").await? {
            Some("a create of an existing object (If-None-Match: *) succeeded")
        } else if self.replace(&key, b"probe", stale).await? {
            Some("a replace with a stale version (If-Match) succeeded")
        } else if self.delete(&key, Some(stale)).await? {
            Some("a delete with a stale version (If-Match) succeeded")
        } else {
            None
        };
        let deleted = self.delete(&key, None).await;
        if let Some(what) = ignored {
            return Err(IngestError::Output {
                path: self.label.clone(),
                message: format!(
                    "the store does not honour conditional writes ({what}), so leases and run ids \
                     would not be safe; use a store that does (AWS S3, R2, MinIO, RustFS, or GCS \
                     through --gcs-bucket), or pass --s3-unsafe-no-conditional-writes if only one \
                     worker ever uses this bucket"
                ),
            });
        }
        deleted.map(|_| ())
    }

    fn path(&self, key: &str) -> Result<ObjectPath, IngestError> {
        let key_path = ObjectPath::parse(key).map_err(|e| self.error(key, chain(&e)))?;
        Ok(match &self.prefix {
            Some(prefix) => prefix.parts().chain(key_path.parts()).collect(),
            None => key_path,
        })
    }

    fn error(&self, key: &str, e: impl fmt::Display) -> IngestError {
        IngestError::Output {
            path: self.location(key),
            message: e.to_string(),
        }
    }
}

impl StateStore for S3StateStore {
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<Versioned>> {
        Box::pin(async move {
            let path = self.path(key)?;
            let result = match self.store.get(&path).await {
                Ok(r) => r,
                Err(ObjectError::NotFound { .. }) => return Ok(None),
                Err(e) => return Err(self.error(key, chain(&e))),
            };
            let version = self.versions.of(&result.meta).ok_or_else(|| {
                self.error(
                    key,
                    "the store returned no ETag or generation, so versions are unavailable",
                )
            })?;
            let bytes = result
                .bytes()
                .await
                .map_err(|e| self.error(key, chain(&e)))?;
            Ok(Some(Versioned {
                bytes: bytes.to_vec(),
                version,
            }))
        })
    }

    fn put<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let path = self.path(key)?;
            self.store
                .put(&path, PutPayload::from(bytes.to_vec()))
                .await
                .map(|_| ())
                .map_err(|e| self.error(key, chain(&e)))
        })
    }

    fn create<'a>(&'a self, key: &'a str, bytes: &'a [u8]) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            let path = self.path(key)?;
            let payload = PutPayload::from(bytes.to_vec());
            match self
                .store
                .put_opts(&path, payload, PutMode::Create.into())
                .await
            {
                Ok(_) => Ok(true),
                Err(ObjectError::AlreadyExists { .. } | ObjectError::Precondition { .. }) => {
                    Ok(false)
                }
                Err(e) => Err(self.error(key, chain(&e))),
            }
        })
    }

    fn replace<'a>(
        &'a self,
        key: &'a str,
        bytes: &'a [u8],
        version: &'a str,
    ) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            let path = self.path(key)?;
            let payload = PutPayload::from(bytes.to_vec());
            match self
                .store
                .put_opts(&path, payload, self.versions.update(version).into())
                .await
            {
                Ok(_) => Ok(true),
                // A changed ETag is a failed precondition; a value deleted meanwhile is NotFound
                // on some stores and a precondition failure on others
                Err(ObjectError::Precondition { .. } | ObjectError::NotFound { .. }) => Ok(false),
                Err(e) => Err(self.error(key, chain(&e))),
            }
        })
    }

    fn delete<'a>(&'a self, key: &'a str, version: Option<&'a str>) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            let path = self.path(key)?;
            match (version, &self.versions) {
                (Some(want), Versions::Generation(Some(gcs))) => {
                    return gcs
                        .delete(&path, want)
                        .await
                        .map_err(|e| self.error(key, e));
                }
                (Some(want), Versions::ETag(Some(s3))) => {
                    return s3.delete(&path, want).await.map_err(|e| self.error(key, e));
                }
                _ => {}
            }
            let head = match self.store.head(&path).await {
                Ok(h) => h,
                Err(ObjectError::NotFound { .. }) => return Ok(false),
                Err(e) => return Err(self.error(key, chain(&e))),
            };
            if let Some(want) = version {
                if self.versions.of(&head).as_deref() != Some(want) {
                    return Ok(false);
                }
            }
            match self.store.delete(&path).await {
                Ok(()) => Ok(true),
                Err(ObjectError::NotFound { .. }) => Ok(false),
                Err(e) => Err(self.error(key, chain(&e))),
            }
        })
    }

    fn list(&self) -> StoreFuture<'_, Vec<String>> {
        Box::pin(async move {
            let metas: Vec<_> = self
                .store
                .list(self.prefix.as_ref())
                .try_collect()
                .await
                .map_err(|e| self.error("", chain(&e)))?;
            let skip = self.prefix.as_ref().map_or(0, |p| p.parts().count());
            Ok(metas
                .into_iter()
                .map(|m| {
                    m.location
                        .parts()
                        .skip(skip)
                        .map(|p| p.as_ref().to_string())
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .collect())
        })
    }

    fn location(&self, key: &str) -> String {
        if key.is_empty() {
            self.label.clone()
        } else {
            format!("{}/{key}", self.label.trim_end_matches('/'))
        }
    }

    fn get_to_file<'a>(&'a self, key: &'a str, dest: &'a Path) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            let path = self.path(key)?;
            let result = match self.store.get(&path).await {
                Ok(r) => r,
                Err(ObjectError::NotFound { .. }) => return Ok(false),
                Err(e) => return Err(self.error(key, chain(&e))),
            };
            let io = |e: std::io::Error| self.error(key, format!("{}: {e}", dest.display()));
            let mut file = tokio::fs::File::create(dest).await.map_err(io)?;
            let mut stream = result.into_stream();
            while let Some(chunk) = stream
                .try_next()
                .await
                .map_err(|e| self.error(key, chain(&e)))?
            {
                file.write_all(&chunk).await.map_err(io)?;
            }
            file.flush().await.map_err(io)?;
            Ok(true)
        })
    }

    fn put_from_file<'a>(&'a self, key: &'a str, src: &'a Path) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let path = self.path(key)?;
            let io = |e: std::io::Error| self.error(key, format!("{}: {e}", src.display()));
            let mut file = tokio::fs::File::open(src).await.map_err(io)?;
            let size = file.metadata().await.map_err(io)?.len();
            if size <= SINGLE_PUT_MAX {
                let mut bytes = Vec::with_capacity(size as usize);
                file.read_to_end(&mut bytes).await.map_err(io)?;
                return self
                    .store
                    .put(&path, PutPayload::from(bytes))
                    .await
                    .map(|_| ())
                    .map_err(|e| self.error(key, chain(&e)));
            }
            put_multipart_from(self.store.as_ref(), &path, &mut file, src)
                .await
                .map_err(|e| self.error(key, e))
        })
    }
}

/// Upload the rest of `file` (read from `local`) to `path` in parts; the object appears, replacing
/// any old one, only when the upload completes. An error aborts the upload.
async fn put_multipart_from(
    store: &dyn ObjectStore,
    path: &ObjectPath,
    file: &mut tokio::fs::File,
    local: &Path,
) -> Result<(), String> {
    let upload = store.put_multipart(path).await.map_err(|e| chain(&e))?;
    let mut writer = WriteMultipart::new_with_chunk_size(upload, PART_SIZE);
    let mut buf = vec![0u8; PART_SIZE];
    loop {
        let n = match file.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                let _ = writer.abort().await;
                return Err(format!("{}: {e}", local.display()));
            }
        };
        if n == 0 {
            break;
        }
        writer.write(&buf[..n]);
        if let Err(e) = writer.wait_for_capacity(4).await {
            let _ = writer.abort().await;
            return Err(chain(&e));
        }
    }
    writer.finish().await.map(|_| ()).map_err(|e| chain(&e))
}

/// Files up to this size go up in one conditional `PUT`; larger ones use a multipart upload
const SINGLE_PUT_MAX: u64 = 32 * 1024 * 1024;
/// Part size of a multipart upload (S3 needs at least 5 MiB per part)
const PART_SIZE: usize = 16 * 1024 * 1024;

/// A [`FileSink`] that uploads finished data files to a bucket (S3-compatible or GCS)
///
/// `put_file` returns when the store confirms the object. A small file is one `PUT` with
/// `If-None-Match: *` (on GCS `x-goog-if-generation-match: 0`), so an existing name is never replaced, even by a racing worker. A large file
/// is a multipart upload after a head check (the object appears only when the upload completes; a
/// race between two workers on one run id is not caught for these, but run ids are unique per
/// run). The trait is synchronous, so the async client runs through `block_in_place`: use it from
/// a multi-threaded Tokio runtime (the binary's).
///
/// Confirmed against AWS S3 and RustFS (#115) and native GCS (#130).
#[derive(Clone)]
pub struct S3FileSink {
    store: Arc<dyn ObjectStore>,
    prefix: Option<ObjectPath>,
    label: String,
}

impl fmt::Debug for S3FileSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3FileSink")
            .field("location", &self.label)
            .finish()
    }
}

impl S3FileSink {
    /// A sink for the bucket and prefix in `options`
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when no client can be built (see [`S3Options`]).
    pub fn new(options: &S3Options) -> Result<Self, IngestError> {
        Self::with_store(
            options.build()?.store,
            &options.prefix,
            options.location(""),
        )
    }

    /// A sink over any `object_store` implementation (tests use the in-memory one)
    ///
    /// # Errors
    ///
    /// [`IngestError::Output`] when `prefix` is not a valid object path.
    pub fn with_store(
        store: Arc<dyn ObjectStore>,
        prefix: &str,
        label: String,
    ) -> Result<Self, IngestError> {
        // Same key layout as the state store
        let keys = S3StateStore::with_store(Arc::clone(&store), prefix, label.clone())?;
        Ok(Self {
            store,
            prefix: keys.prefix,
            label,
        })
    }

    fn error(&self, name: &str, e: impl fmt::Display) -> IngestError {
        IngestError::Output {
            path: format!("{}/{name}", self.label.trim_end_matches('/')),
            message: e.to_string(),
        }
    }

    fn path(&self, name: &str) -> Result<ObjectPath, IngestError> {
        let key_path = ObjectPath::parse(name).map_err(|e| self.error(name, chain(&e)))?;
        Ok(match &self.prefix {
            Some(prefix) => prefix.parts().chain(key_path.parts()).collect(),
            None => key_path,
        })
    }

    async fn upload(&self, name: &str, local: &Path) -> Result<(), IngestError> {
        let path = self.path(name)?;
        let io = |e: std::io::Error| self.error(name, format!("{}: {e}", local.display()));
        let mut file = tokio::fs::File::open(local).await.map_err(io)?;
        let size = file.metadata().await.map_err(io)?.len();
        if size <= SINGLE_PUT_MAX {
            let mut bytes = Vec::with_capacity(size as usize);
            file.read_to_end(&mut bytes).await.map_err(io)?;
            return match self
                .store
                .put_opts(&path, PutPayload::from(bytes), PutMode::Create.into())
                .await
            {
                Ok(_) => Ok(()),
                Err(ObjectError::AlreadyExists { .. } | ObjectError::Precondition { .. }) => {
                    Err(self.exists_error(name))
                }
                Err(e) => Err(self.error(name, chain(&e))),
            };
        }
        if self.exists_async(name).await? {
            return Err(self.exists_error(name));
        }
        put_multipart_from(self.store.as_ref(), &path, &mut file, local)
            .await
            .map_err(|e| self.error(name, e))
    }

    fn exists_error(&self, name: &str) -> IngestError {
        self.error(
            name,
            "object already exists; another run may share this run id",
        )
    }

    async fn exists_async(&self, name: &str) -> Result<bool, IngestError> {
        match self.store.head(&self.path(name)?).await {
            Ok(_) => Ok(true),
            Err(ObjectError::NotFound { .. }) => Ok(false),
            Err(e) => Err(self.error(name, chain(&e))),
        }
    }
}

/// Run an async call from synchronous code inside a multi-threaded Tokio runtime
fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

impl FileSink for S3FileSink {
    fn put_file(&self, name: &str, local: &Path) -> Result<(), IngestError> {
        block_on(self.upload(name, local))
    }

    fn exists(&self, name: &str) -> Result<bool, IngestError> {
        block_on(self.exists_async(name))
    }

    fn location(&self, name: &str) -> String {
        format!("{}/{name}", self.label.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    fn store(prefix: &str) -> S3StateStore {
        S3StateStore::with_store(Arc::new(InMemory::new()), prefix, "s3://test".to_string())
            .unwrap()
    }

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl fmt::Display for Layer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|e| e as _)
        }
    }

    #[test]
    fn errors_name_their_causes_once() {
        let e = Layer(
            "Generic S3 error: builder error",
            Some(Box::new(Layer(
                "builder error",
                Some(Box::new(Layer("no CA certificates found", None))),
            ))),
        );
        assert_eq!(
            chain(&e),
            "Generic S3 error: builder error: no CA certificates found"
        );
    }

    #[tokio::test]
    async fn conditional_writes_follow_the_etag() {
        let s = store("reso/state");
        assert_eq!(s.get("a.json").await.unwrap(), None);
        assert!(s.create("a.json", b"one").await.unwrap());
        assert!(!s.create("a.json", b"two").await.unwrap());
        let v1 = s.get("a.json").await.unwrap().unwrap();
        assert_eq!(v1.bytes, b"one");
        assert!(s.replace("a.json", b"two", &v1.version).await.unwrap());
        assert!(!s.replace("a.json", b"three", &v1.version).await.unwrap());
        assert!(!s.delete("a.json", Some(&v1.version)).await.unwrap());
        let v2 = s.get("a.json").await.unwrap().unwrap();
        assert_eq!(v2.bytes, b"two");
        assert!(s.delete("a.json", Some(&v2.version)).await.unwrap());
        assert!(!s.delete("a.json", None).await.unwrap());
        assert!(!s.replace("a.json", b"x", &v2.version).await.unwrap());
    }

    #[tokio::test]
    async fn lists_keys_relative_to_the_prefix() {
        let inner = Arc::new(InMemory::new());
        let s = S3StateStore::with_store(inner.clone(), "/p/q/", "s3://b/p/q".to_string()).unwrap();
        let other =
            S3StateStore::with_store(inner, "p/other", "s3://b/p/other".to_string()).unwrap();
        s.put("x.json", b"1").await.unwrap();
        s.put("media/mls@1.json", b"2").await.unwrap();
        other.put("y.json", b"3").await.unwrap();
        let mut keys = s.list().await.unwrap();
        keys.sort();
        assert_eq!(keys, ["media/mls@1.json", "x.json"]);
        assert_eq!(
            s.get("media/mls@1.json").await.unwrap().unwrap().bytes,
            b"2"
        );
        assert_eq!(s.location("x.json"), "s3://b/p/q/x.json");
        assert_eq!(other.list().await.unwrap(), ["y.json"]);
    }

    #[tokio::test]
    async fn only_one_of_two_workers_creates_a_lock() {
        let s = Arc::new(store(""));
        let tasks: Vec<_> = (0..8)
            .map(|i| {
                let s = s.clone();
                tokio::spawn(async move { s.create("feed.lock", format!("w{i}").as_bytes()).await })
            })
            .collect();
        let mut won = 0;
        for t in tasks {
            if t.await.unwrap().unwrap() {
                won += 1;
            }
        }
        assert_eq!(won, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sink_uploads_once_and_never_replaces() {
        let inner = Arc::new(InMemory::new());
        let sink = S3FileSink::with_store(inner.clone(), "out/", "s3://b/out".to_string()).unwrap();
        let dir = std::env::temp_dir().join(format!("reso-s3-sink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.ndjson");
        std::fs::write(&file, b"{\"a\":1}\n").unwrap();
        assert!(!sink.exists("run1/a.ndjson").unwrap());
        sink.put_file("run1/a.ndjson", &file).unwrap();
        assert!(sink.exists("run1/a.ndjson").unwrap());
        let err = sink.put_file("run1/a.ndjson", &file).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        let got = inner
            .get(&ObjectPath::from("out/run1/a.ndjson"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(&got[..], b"{\"a\":1}\n");
        assert!(sink.put_file("x", &dir.join("missing")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sink_uploads_large_files_in_parts() {
        let inner = Arc::new(InMemory::new());
        let sink = S3FileSink::with_store(inner.clone(), "", "s3://b".to_string()).unwrap();
        let dir = std::env::temp_dir().join(format!("reso-s3-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.ndjson");
        let len = SINGLE_PUT_MAX as usize + 5_000_001;
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file, &data).unwrap();
        sink.put_file("big.ndjson", &file).unwrap();
        let got = inner
            .get(&ObjectPath::from("big.ndjson"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert!(got[..] == data[..]);
        assert!(sink.put_file("big.ndjson", &file).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn versions_are_etags_on_s3_and_generations_on_gcs() {
        let meta = ObjectMeta {
            location: ObjectPath::from("k"),
            last_modified: Default::default(),
            size: 1,
            e_tag: Some("\"etag\"".into()),
            version: Some("1795654814258332".into()),
        };
        let generation = Versions::Generation(None);
        assert_eq!(Versions::ETag(None).of(&meta).as_deref(), Some("\"etag\""));
        assert_eq!(generation.of(&meta).as_deref(), Some("1795654814258332"));
        let PutMode::Update(v) = Versions::ETag(None).update("\"etag\"") else {
            panic!("not an update")
        };
        assert_eq!((v.e_tag.as_deref(), v.version), (Some("\"etag\""), None));
        let PutMode::Update(v) = generation.update("17") else {
            panic!("not an update")
        };
        assert_eq!((v.e_tag, v.version.as_deref()), (None, Some("17")));
    }

    #[tokio::test]
    async fn gcs_delete_sends_the_generation_and_reads_the_answer() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        for (generation, status) in [("1", 204), ("2", 412), ("3", 404), ("4", 503)] {
            Mock::given(method("DELETE"))
                .and(path("/bkt/p/state/media/mls@1.json"))
                .and(header("authorization", "Bearer test-token"))
                .and(header("x-goog-if-generation-match", generation))
                .respond_with(ResponseTemplate::new(status).set_body_string("busy"))
                .mount(&server)
                .await;
        }
        let delete = GcsDelete {
            credentials: Arc::new(object_store::StaticCredentialProvider::new(
                object_store::gcp::GcpCredential {
                    bearer: "test-token".into(),
                },
            )),
            base_url: format!("{}/", server.uri()),
            bucket: "bkt".into(),
            http: reqwest::Client::new(),
        };
        let p = ObjectPath::from("p/state/media/mls@1.json");
        assert!(delete.delete(&p, "1").await.unwrap());
        assert!(!delete.delete(&p, "2").await.unwrap());
        assert!(!delete.delete(&p, "3").await.unwrap());
        let err = delete.delete(&p, "4").await.unwrap_err();
        assert!(err.contains("503") && err.contains("busy"), "{err}");
    }

    #[test]
    fn gcs_through_the_s3_endpoint_is_refused() {
        for endpoint in [
            "https://storage.googleapis.com",
            "https://STORAGE.googleapis.com/",
            "storage.googleapis.com:443",
            "https://bkt.storage.googleapis.com",
        ] {
            assert!(is_gcs_interop(endpoint), "{endpoint}");
            let o = S3Options {
                endpoint: Some(endpoint.into()),
                region: Some("auto".into()),
                ..S3Options::new("b", "")
            };
            let err = S3StateStore::new(&o).unwrap_err().to_string();
            assert!(err.contains("--gcs-bucket"), "{err}");
        }
        for endpoint in [
            "http://localhost:9100",
            "https://x.r2.cloudflarestorage.com",
        ] {
            assert!(!is_gcs_interop(endpoint), "{endpoint}");
        }
    }

    #[test]
    fn gcs_options_name_a_gs_url() {
        let o = S3Options {
            provider: Provider::Gcs,
            ..S3Options::new("b", "pre")
        };
        assert_eq!(o.location("k.json"), "gs://b/pre/k.json");
    }

    #[test]
    fn options_name_the_bucket_without_secrets() {
        let o = S3Options::new("b", "/pre/");
        assert_eq!(o.location(""), "s3://b/pre");
        assert_eq!(o.location("k.json"), "s3://b/pre/k.json");
        assert_eq!(S3Options::new("b", "").location("k"), "s3://b/k");
    }

    #[tokio::test]
    async fn files_stream_to_and_from_the_store_in_one_put_or_in_parts() {
        let dir = std::env::temp_dir().join(format!("reso-s3-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = store("p");
        let (src, dest) = (dir.join("src"), dir.join("dest"));
        assert!(!s.get_to_file("k.keys.ndjson", &dest).await.unwrap());
        // Over SINGLE_PUT_MAX, so a multipart upload of three parts
        for size in [10, SINGLE_PUT_MAX as usize + 5] {
            let bytes: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            std::fs::write(&src, &bytes).unwrap();
            s.put_from_file("k.keys.ndjson", &src).await.unwrap();
            assert!(s.get_to_file("k.keys.ndjson", &dest).await.unwrap());
            assert!(std::fs::read(&dest).unwrap() == bytes, "size {size}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
