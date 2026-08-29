//! a `CacheBackend` for github actions.

use std::io;
use std::task::{Poll, ready};
use std::time::Duration;

use base64::Engine;
use futures_util::future::BoxFuture;
use futures_util::{FutureExt, TryStreamExt};
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

/// The amount of concurrent blob uploads
const CONCURRENT_UPLOADS: usize = 4; // value actions/cache uses 

/// The azure version to use
const AZURE_VERSION: &str = "2023-11-03";

/// The prefix to use for cache entries for serpentine
const CACHE_PREFIX: &str = "serpentine-";

/// The prefix to use for the data cache
const DATA_CACHE_PREFIX: &str = "serpentine-data";

/// A caching backend for github action cache service, using their undocummented api that everyone
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
    /// Version is a (persumed ascii?) 64 bytes long string to scope under.
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

    /// Get a reader for the given github cache key (or any specificed by the restore keys)
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
            .await
            .inspect_err(|err| log::error!("{err:?}"))
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
            .await
            .and_then(reqwest::Response::error_for_status)
            .inspect_err(|err| log::error!("{err:?}"))
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
            .await
            .wrap_internal("Failed to create cache entry")?;

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
#[serde(rename_all = "camelCase")]
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
    /// Was is sucessfull?
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

/// Serailize a `u64` as a string, as per the twirp protocol.
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
    /// Was it sucessfull
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

    fn get_data_cache(&self) -> BoxFuture<'_, Option<BoxedReader>> {
        Box::pin(
            self.get_reader_for_github_key(
                DATA_CACHE_PREFIX.into(),
                vec![DATA_CACHE_PREFIX.into()],
            ),
        )
    }

    fn get_data_cache_writer(&self) -> BoxFuture<'_, miette::Result<BoxedWriter>> {
        Box::pin(
            self.create_writer_for_github_key(format!(
                "{DATA_CACHE_PREFIX}{}",
                uuid::Uuid::new_v4()
            )),
        )
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
    upload_futures: tokio::task::JoinSet<Result<(), reqwest::Error>>,
    /// Total amount of bytes written so far, or specifically the amount of bytes queued to be
    /// written so far. up to `CONCURRENT_UPLOADS` * `BLOCK_SIZE` bytes might still be in flight.
    bytes_written: u64,
    /// The signed url to upload blocks to
    url: Box<str>,
    /// Counter for generting block ids.
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
    CommitingAzure(BoxFuture<'static, Result<reqwest::Response, reqwest::Error>>),
    /// Currently performing github commit request
    CommitingGithub(BoxFuture<'static, Result<reqwest::Response, reqwest::Error>>),
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

    /// Returns the base64 encoding of the next block id to use, incrementing the internal counter
    /// to ensure the next block id is unique.
    fn get_next_block_id(&mut self) -> Box<str> {
        // Azure requires these to always be the same *pre-encoded* size.

        let raw_id = self.block_id_counter.to_le_bytes();
        self.block_id_counter = self.block_id_counter.saturating_add(1);

        let id = base64::prelude::BASE64_STANDARD.encode(raw_id);
        id.into_boxed_str()
    }

    /// Return a iterator of all block ids used (in order)
    #[inline]
    fn get_block_ids(&self) -> impl Iterator<Item = Box<str>> {
        (0..self.block_id_counter).map(|raw_id| {
            let raw_id = raw_id.to_le_bytes();
            let id = base64::prelude::BASE64_STANDARD.encode(raw_id);
            id.into_boxed_str()
        })
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
            .map(|response| {
                response
                    .and_then(reqwest::Response::error_for_status)
                    .map(|_response| ())
            });
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
            // Since its ready then a item was just poped or it was empty so its always safe for us
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

        if !self.buffer.is_empty() {
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
                    ready!(self.as_mut().poll_flush(cx))?;

                    log::debug!("All bytes flushed, commiting to azure");
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
                        .send();
                    let commit_future = Box::pin(commit_future);
                    self.shutdown_state =
                        Some(BlobWriterShutdownState::CommitingAzure(commit_future));
                }
                Some(BlobWriterShutdownState::CommitingAzure(future)) => {
                    ready!(future.poll_unpin(cx))
                        .and_then(reqwest::Response::error_for_status)
                        .map_err(io::Error::other)?;

                    log::debug!("Azure commit done, commiting to github.");
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
                        .send();
                    self.shutdown_state = Some(BlobWriterShutdownState::CommitingGithub(Box::pin(
                        commit_future,
                    )));
                }
                Some(BlobWriterShutdownState::CommitingGithub(future)) => {
                    ready!(future.poll_unpin(cx))
                        .and_then(reqwest::Response::error_for_status)
                        .map_err(io::Error::other)?;
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
