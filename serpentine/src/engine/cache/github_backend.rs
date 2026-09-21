//! a `CacheBackend` for github actions.

// NOTE: The request structs use the protobuf field names, according to the spec a parser should
// accept both field names and lowercamelCase names, but certain implementations only accept the
// field names.
//
// Our response structs accept both.

use std::io;
use std::task::{Poll, ready};
use std::time::Duration;

use base64::Engine;
use futures_util::future::BoxFuture;
use futures_util::{FutureExt, TryFutureExt, TryStreamExt};
use miette::{Context, IntoDiagnostic};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWrite;
use tokio_util::io::StreamReader;

use crate::engine::cache::{CacheBackend, CacheHash};
use crate::engine::{BoxedReader, BoxedWriter, WrapInternal};

/// The env var the actions runner sets to the base url of its cache service.
const RESULTS_URL_VAR: &str = "ACTIONS_RESULTS_URL";

/// The env var holding the auth token.
const TOKEN_VAR: &str = "ACTIONS_RUNTIME_TOKEN";

/// The rest of the base url appended after `RESULTS_URL_VAR`
const BASE_URL: &str = "twirp/github.actions.results.api.v1.CacheService";

/// The user agent to use
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

/// The size of each block
///
/// Azure has a max block count of 50,000, for the 10GB repo limit of gha that works out to 200KB
/// blocks. In other words as long as blocks are bigger than that is impossible to hit the block limit
/// and stay within the repo limit.
const BLOCK_SIZE: usize = 1024 * 1024 * 32; // 32 MiB

/// The minimum size for a block, except the last one, if `flush` is called while the buffer is
/// smaller than this it should not write the block.
///
/// While github actions (azure) accepts writes of any size (up to a max), some third party S3
/// backed replacements (such as blacksmith) require a minimum block size.
///
/// NOTE: This is technically a violation of flushes protocol, as it says any buffered data should
/// reach its destination, but flush is mostly used for one directional channels to just keep memory
/// usage down (in a bi-direction call response flush not writing given bytes would be bad.).
const MINIMUM_BLOCK_SIZE: usize = 1024 * 1024 * 5; // 5 MiB

/// The amount of concurrent blob uploads
const CONCURRENT_UPLOADS: usize = 4; // value actions/cache uses 

/// The azure version to use
const AZURE_VERSION: &str = "2023-11-03";

/// The prefix to use for cache entries for serpentine
const CACHE_PREFIX: &str = "serpentine-";

/// A trait for easier handling and logging request errors
trait HandleError {
    /// The result of `handle_error`
    type Output;

    /// Handle the given error.
    async fn handle_error(self) -> Self::Output;
}

impl<F> HandleError for F
where
    F: Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    type Output = Result<reqwest::Response, std::io::Error>;

    async fn handle_error(self) -> Self::Output {
        match self.await {
            Err(err) => {
                log::error!("{err})");
                Err(std::io::Error::other(err))
            }
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    Ok(response)
                } else {
                    match response.text().await {
                        Ok(body) => {
                            log::error!("{status}: {body}");
                            Err(std::io::Error::other(body))
                        }
                        Err(err) => {
                            log::error!("{status}: {err}");
                            Err(std::io::Error::other(err))
                        }
                    }
                }
            }
        }
    }
}

/// A caching backend for github action cache service, using their undocumented api that everyone
/// uses :P.
#[derive(Clone)]
pub struct GithubActionsBackend {
    /// The http client to use for github
    github_client: reqwest::Client,
    /// The http client to use for azure
    azure_client: reqwest::Client,
    /// The base url to send requests to.
    base_url: Box<str>,
    /// The version string to scope everything under.
    ///
    /// Must be 64 bytes long
    version: Box<str>,
}

impl GithubActionsBackend {
    /// Whether the actions cache service is reachable from the current environment.
    #[inline]
    pub fn available() -> bool {
        std::env::var_os(RESULTS_URL_VAR).is_some()
    }

    /// Crate a github actions cache backend from the env variables passed by the actions runner.
    ///
    /// Version is a (presumed ascii?) 64 bytes long string to scope under.
    pub fn new(version: Box<str>) -> miette::Result<Self> {
        log::info!("Saving caches to github actions");

        debug_assert_eq!(version.len(), 64, "Version field must be 64 bytes");

        let mut base_url = std::env::var(RESULTS_URL_VAR).into_diagnostic().wrap_err(
            "Github actions cache env var not found, is this running in github actions?",
        )?;
        base_url.push_str(BASE_URL);

        let token = std::env::var(TOKEN_VAR).into_diagnostic().wrap_err(
            "Github actions cache env var not found, is this running in github actions?",
        )?;

        let mut github_headers = reqwest::header::HeaderMap::new();
        github_headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {token}")
                .parse()
                .wrap_internal("Token is not a valid header value")?,
        );
        github_headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json"
                .parse()
                .wrap_internal("constant content type header value isnt ascii?!")?,
        );

        let github_client = reqwest::ClientBuilder::new()
            .default_headers(github_headers)
            .user_agent(USER_AGENT)
            .read_timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(30))
            .build()
            .wrap_internal("Failed to build reqwest client.")?;

        let mut azure_headers = reqwest::header::HeaderMap::new();
        azure_headers.insert(
            "x-ms-version",
            AZURE_VERSION
                .parse()
                .wrap_internal("Token is not a valid header value")?,
        );

        let azure_client = reqwest::ClientBuilder::new()
            .default_headers(azure_headers)
            .user_agent(USER_AGENT)
            .read_timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(30))
            .build()
            .wrap_internal("Failed to build azure client")?;

        Ok(Self {
            github_client,
            azure_client,
            base_url: base_url.into(),
            version,
        })
    }

    /// Get a reader for the given github cache key (or any specified by the restore keys)
    async fn get_reader_for_github_key(
        &self,
        key: Box<str>,
        restore_keys: Vec<Box<str>>,
    ) -> Option<BoxedReader> {
        log::debug!(
            "Attempting to read cache entry for {key} (with restore kets {restore_keys:?})."
        );

        let request = GetCacheEntryDownloadURL {
            key,
            version: self.version.clone(),
            restore_keys,
        };
        let response = self
            .github_client
            .post(format!("{}/GetCacheEntryDownloadURL", self.base_url))
            .json(&request)
            .send()
            .handle_error()
            .await
            .ok()?;

        if !response.status().is_success() {
            log::error!("Got status: {}", response.status());
            if let Ok(body) = response.text().await {
                log::error!("{body}");
            }
            return None;
        }

        let response: GetCacheEntryDownloadURLResponse = response
            .json()
            .await
            .inspect_err(|err| log::error!("{err}"))
            .ok()?;

        if !response.ok {
            log::debug!("Cache miss");
            return None;
        }
        log::debug!("Matched on key {}", response.matched_key);

        let url = response.signed_download_url;
        let stream = self
            .azure_client
            .get(&*url)
            .send()
            .handle_error()
            .await
            .ok()?
            .bytes_stream()
            .map_err(io::Error::other);
        let reader = StreamReader::new(stream);

        Some(BoxedReader::new(reader))
    }

    /// Create the cache entry for the given key and return a writer for it.
    async fn create_writer_for_github_key(&self, key: String) -> miette::Result<BoxedWriter> {
        log::debug!("Attempting to create cache entry for {key}.");
        let create_request = CreateCacheEntry {
            key: key.clone(),
            version: &self.version,
        };

        log::trace!("sending {create_request:#?}");

        let response = self
            .github_client
            .post(format!("{}/CreateCacheEntry", self.base_url))
            .json(&create_request)
            .send()
            .handle_error()
            .await
            .into_diagnostic()?;

        if !response.status().is_success() {
            log::error!("Got status: {}", response.status());
            if let Ok(body) = response.text().await {
                log::error!("{body}");
                return Err(miette::miette!("{body}"));
            }
            return Err(miette::miette!("Unknown error"));
        }

        let response: CreateCacheEntryResponse = response
            .json()
            .await
            .wrap_internal("Failed to read response")?;

        log::trace!("got response: {response:#?}");

        if !response.ok {
            log::error!("Got error from response: {}", response.message);
            return Err(miette::miette!("{}", response.message));
        }

        let signed_upload_url = response.signed_upload_url;

        Ok(BoxedWriter::new(AzureBlobWriter::new(
            self.clone(),
            signed_upload_url,
            key.into_boxed_str(),
        )))
    }

    /// Convert a hash into the cache key to use.
    #[inline]
    fn hash_to_key(key: CacheHash) -> String {
        let key = base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(key.0);
        format!("{CACHE_PREFIX}{key}")
    }
}

/// Create a new cache entry
#[derive(Clone, Debug, Serialize)]
struct CreateCacheEntry<'version> {
    /// The key to create the entry under
    key: String,
    /// The version to create the key under.
    version: &'version str,
}

/// The response to `CreateCacheEntry`
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateCacheEntryResponse {
    /// Was is successful?
    #[serde(alias = "ok")]
    ok: bool,
    /// the url to upload bytes to
    #[serde(default)]
    #[serde(alias = "signed_upload_url")]
    signed_upload_url: Box<str>,
    /// Message for when we get a error
    #[serde(default)]
    #[serde(alias = "message")]
    message: Box<str>,
}

/// Serialize a `u64` as a string, as per the twirp protocol.
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "The required signature for serde"
)]
#[inline]
fn u64_as_str<S: serde::Serializer>(value: &u64, ser: S) -> Result<S::Ok, S::Error> {
    ser.collect_str(value)
}

/// Finalize the cache upload on the github side.
#[derive(Clone, Debug, Serialize)]
struct FinalizeCacheEntryUpload {
    /// The key to store commit
    key: Box<str>,
    /// The amount of bytes that were written.
    #[serde(serialize_with = "u64_as_str")]
    size_bytes: u64,
    /// The version string to use.
    version: Box<str>,
}

/// Get the url to download requests from
#[derive(Clone, Debug, Serialize)]
struct GetCacheEntryDownloadURL {
    /// The exact key to try restoring.
    key: Box<str>,
    /// The version string to look for.
    version: Box<str>,
    /// The restore prefixes to also look for
    restore_keys: Vec<Box<str>>,
}

/// The response to `GetCacheEntryDownloadUrl`
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
struct GetCacheEntryDownloadURLResponse {
    /// Was it successful
    #[serde(alias = "ok")]
    ok: bool,
    /// The url to download from
    #[serde(alias = "signed_download_url")]
    signed_download_url: Box<str>,
    /// the key that was matched
    #[serde(alias = "matched_key")]
    matched_key: Box<str>,
}

impl CacheBackend for GithubActionsBackend {
    fn read_key(&self, key: CacheHash) -> BoxFuture<'_, Option<BoxedReader>> {
        let key = Self::hash_to_key(key);
        Box::pin(self.get_reader_for_github_key(key.into_boxed_str(), vec![]))
    }

    fn write_key(&self, key: CacheHash) -> BoxFuture<'_, Option<BoxedWriter>> {
        let key = Self::hash_to_key(key);
        Box::pin(self.create_writer_for_github_key(key).map(Result::ok))
    }
}

/// A writer for the azure blob storage, that buffers input and commits on shutdown.
struct AzureBlobWriter {
    /// The github cache client, used so clients setup and other fields can be easially used from
    /// this writer
    github_client: GithubActionsBackend,
    /// the buffer of bytes of the next block to write
    buffer: Vec<u8>,
    /// The inflight futures uploading blocks .
    upload_futures: tokio::task::JoinSet<Result<(), std::io::Error>>,
    /// Total amount of bytes written so far, or specifically the amount of bytes queued to be
    /// written so far. up to `CONCURRENT_UPLOADS` * `BLOCK_SIZE` bytes might still be in flight.
    bytes_written: u64,
    /// The signed url to upload blocks to
    url: Box<str>,
    /// Counter for generating block ids.
    block_id_counter: u16, // max value is always 50k per azure limits.
    /// The github side key to commit under
    github_key: Box<str>,
    /// The shutdown state
    shutdown_state: Option<BlobWriterShutdownState>,
}

/// The state of the shutdown machinery    
enum BlobWriterShutdownState {
    /// Currently flushing
    Flush,
    /// Currently performing azure commit request
    CommittingAzure(BoxFuture<'static, Result<reqwest::Response, std::io::Error>>),
    /// Currently performing github commit request
    CommittingGithub(BoxFuture<'static, Result<reqwest::Response, std::io::Error>>),
    /// Shutdown is done
    Done,
}

impl AzureBlobWriter {
    /// Create a new azure blob writer that writes to the given pre-signed url.
    fn new(github_client: GithubActionsBackend, url: Box<str>, key: Box<str>) -> Self {
        Self {
            github_client,
            buffer: Vec::with_capacity(BLOCK_SIZE),
            upload_futures: tokio::task::JoinSet::new(),
            bytes_written: 0,
            url,
            block_id_counter: 0,
            github_key: key,
            shutdown_state: None,
        }
    }

    /// Generate the block id for the given index
    ///
    /// While azure (and github actions cache) supports any consistent length block id,
    /// certain third party implementation specifically want (and use 'metadata' from) the exact
    /// `block_id` format used by `actions/cache`.
    ///
    /// Namely a 48 byte ascii string encoded into base64, consisting of a uuidv4 and then a 0
    /// padded index for the rest.
    fn generate_block_id(index: u16) -> Box<str> {
        let prefix = uuid::Uuid::nil().hyphenated().to_string();
        let index = format!("{index:0>12}"); // 48 - 36 (uuid size) = 12

        let raw_id = format!("{prefix}{index}");
        log::trace!("Raw block id: {raw_id}");
        let id = base64::prelude::BASE64_STANDARD_NO_PAD.encode(&raw_id);
        log::trace!("base64 block id: {id}");
        id.into_boxed_str()
    }

    /// Returns the base64 encoding of the next block id to use, incrementing the internal counter
    /// to ensure the next block id is unique.
    fn get_next_block_id(&mut self) -> Box<str> {
        let index = self.block_id_counter;
        self.block_id_counter = self.block_id_counter.saturating_add(1);
        Self::generate_block_id(index)
    }

    /// Return a iterator of all block ids used (in order)
    #[inline]
    fn get_block_ids(&self) -> impl Iterator<Item = Box<str>> {
        (0..self.block_id_counter).map(Self::generate_block_id)
    }

    /// schedule the current buffer for upload to azure.
    ///
    /// This will clear the internal buffer and spawn a future on the `JoinSet`
    fn upload_buffer(&mut self) {
        log::trace!("Starting upload of {} bytes", self.buffer.len());

        if self.buffer.is_empty() {
            log::warn!("Upload called on empty buffer");
            debug_assert!(
                false,
                "upload_buffer should not be called when buffer is empty"
            );
            return;
        }

        let buffer = std::mem::replace(&mut self.buffer, Vec::with_capacity(BLOCK_SIZE));
        self.bytes_written = self.bytes_written.saturating_add(buffer.len() as u64);

        let block_id = self.get_next_block_id();
        let upload_future = self
            .github_client
            .azure_client
            .put(&*self.url)
            .query(&[("comp", "block")])
            .query(&[("blockid", block_id)])
            .body(buffer)
            .send()
            .handle_error()
            .map_ok(|_response| ());
        self.upload_futures.spawn(upload_future);
    }
}

impl AsyncWrite for AzureBlobWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        log::trace!("Attempting to write {} bytes to azure", buf.len());

        if self.shutdown_state.is_some() {
            return Poll::Ready(Err(std::io::Error::other(
                "Cannot write after shutdown started.",
            )));
        }

        // Poll the join set to ensure any done futures get drop
        // surfaced.
        //
        // Its safe for us to not be actively polling self at every possible moment because `JoinSet` always drives futures in the background.
        // We only need to poll the joinset to clean out space for more futures and registering
        // wakers for when we are waiting on the set to clear up.
        match self.upload_futures.poll_join_next(cx) {
            Poll::Ready(Some(Err(err))) => {
                log::error!("{err:?}");
                return Poll::Ready(Err(std::io::Error::other(err)));
            }
            Poll::Ready(Some(Ok(Err(err)))) => {
                log::error!("{err:?}");
                return Poll::Ready(Err(std::io::Error::other(err)));
            }

            Poll::Pending => {
                log::trace!("Join set pending");
                // Unless we are at the cap its safe for us to continue to the `Ready` branch
                if self.upload_futures.len() >= CONCURRENT_UPLOADS {
                    log::debug!("Concurrency bound reached on upload, pending.");
                    // The poll above, since it returned pending, registers wakers.
                    return Poll::Pending;
                }
            }
            // Since its ready then a item was just popped or it was empty so its always safe for us
            // to (as in stays under the cap) to register one more waker
            Poll::Ready(value) => {
                if value.is_some() {
                    log::debug!("block upload done.");
                } else {
                    log::debug!("No inflight uploads.");
                }
            }
        }

        let written = buf.len().min(BLOCK_SIZE.saturating_sub(self.buffer.len()));
        let Some(chunk) = buf.get(..written) else {
            return Poll::Ready(Err(std::io::Error::other(
                "buffer bookkeeping invariant violated",
            )));
        };
        self.buffer.extend_from_slice(chunk);

        if self.buffer.len() >= BLOCK_SIZE {
            log::debug!("Buffer filled");
            self.upload_buffer();
        }

        log::trace!("wrote {written} bytes from caller.");

        debug_assert!(
            written > 0 || buf.is_empty(),
            "Returning invalid poll_write result for what we did. (written = 0 only valid for a empty buffer or writer closed)"
        );

        Poll::Ready(Ok(written))
    }

    // For `poll_flush` and `poll_shutdown` the logic is much simpler, we use a loop to re-poll our
    // sub futures until they return Pending (via `ready!`) or we are fully done.
    //
    // This is extremely important because if a sub future returns a `Ready`, but we arent done yet
    // (i.e `Pending`) we cant return `Pending` yet because a waker might not be registered. In
    // general, unless we call wakers our self, we can only return `Pending` if we have seen a
    // `Pending` from another future. (same holds for `poll_write` in fact, but there we also want
    // to return `Ready` in some cases where we got a `Pending`)

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        log::trace!("polling flush blob uploader");

        if self.buffer.len() >= MINIMUM_BLOCK_SIZE {
            log::debug!("Flushing buffer");
            self.upload_buffer();
        }

        loop {
            match ready!(self.upload_futures.poll_join_next(cx)) {
                // All futures finished.
                None => return Poll::Ready(Ok(())),
                // Consumed one future, loop to maybe drain the rest.
                Some(Ok(Ok(()))) => {}
                // Errors
                Some(Err(err)) => {
                    log::error!("{err:?}");
                    return Poll::Ready(Err(std::io::Error::other(err)));
                }
                Some(Ok(Err(err))) => {
                    log::error!("{err:?}");
                    return Poll::Ready(Err(std::io::Error::other(err)));
                }
            }
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        const ID_OPEN: &str = "<Latest>";
        const ID_CLOSE: &str = "</Latest>";
        const MESSAGE_OPEN: &str = r#"<?xml version="1.0" encoding="utf-8"?><BlockList>"#;
        const MESSAGE_CLOSE: &str = "</BlockList>";

        log::trace!("Polling shutdown blob uploader");

        loop {
            match &mut self.shutdown_state {
                None => self.shutdown_state = Some(BlobWriterShutdownState::Flush),
                Some(BlobWriterShutdownState::Flush) => {
                    if !self.buffer.is_empty() {
                        log::debug!("Writing final part");
                        self.upload_buffer();
                    }

                    ready!(self.as_mut().poll_flush(cx))?;

                    log::debug!("All bytes flushed, committing to azure");
                    let mut request_body = String::with_capacity(
                        const { MESSAGE_OPEN.len() + MESSAGE_CLOSE.len() }.saturating_add(
                            usize::from(self.block_id_counter)
                                // 2 bytes base64 = 4
                                .saturating_mul(const { ID_OPEN.len() + 4 + ID_CLOSE.len() }),
                        ),
                    );
                    request_body.push_str(MESSAGE_OPEN);
                    for block_id in self.get_block_ids() {
                        request_body.push_str(ID_OPEN);
                        request_body.push_str(&block_id);
                        request_body.push_str(ID_CLOSE);
                    }
                    request_body.push_str(MESSAGE_CLOSE);

                    let commit_future = self
                        .github_client
                        .azure_client
                        .put(&*self.url)
                        .query(&[("comp", "blocklist")])
                        .header(reqwest::header::CONTENT_TYPE, "application/xml")
                        .body(request_body)
                        .send()
                        .handle_error();
                    let commit_future = Box::pin(commit_future);
                    self.shutdown_state =
                        Some(BlobWriterShutdownState::CommittingAzure(commit_future));
                }
                Some(BlobWriterShutdownState::CommittingAzure(future)) => {
                    ready!(future.poll_unpin(cx))?;

                    log::debug!("Azure commit done, committing to github.");
                    let request = FinalizeCacheEntryUpload {
                        key: std::mem::take(&mut self.github_key),
                        version: std::mem::take(&mut self.github_client.version),
                        size_bytes: self.bytes_written,
                    };
                    let commit_future = self
                        .github_client
                        .github_client
                        .post(format!(
                            "{}/FinalizeCacheEntryUpload",
                            self.github_client.base_url
                        ))
                        .json(&request)
                        .send()
                        .handle_error();
                    self.shutdown_state = Some(BlobWriterShutdownState::CommittingGithub(
                        Box::pin(commit_future),
                    ));
                }
                Some(BlobWriterShutdownState::CommittingGithub(future)) => {
                    ready!(future.poll_unpin(cx))?;

                    self.shutdown_state = Some(BlobWriterShutdownState::Done);

                    log::debug!("Github commit done");
                }
                Some(BlobWriterShutdownState::Done) => {
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

#[cfg(test)]
#[cfg(feature = "_test_gha")]
#[expect(clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    crate::test_well_behaved_cache!(
        GithubActionsBackend::new(
            format!("{:032x}{:032x}", uuid::Uuid::new_v4().as_u128(), 0u128).into_boxed_str()
        )
        .expect("Failed to create github cache backend")
    );
}
