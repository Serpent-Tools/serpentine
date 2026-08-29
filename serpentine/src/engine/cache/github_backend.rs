//! a `CacheBackend` for github actions.

use std::io::Write;
use std::task::{Poll, ready};

use base64::Engine;
use futures_util::future::BoxFuture;
use miette::{Context, IntoDiagnostic};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWrite;

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
const BLOCK_SIZE: usize = 1024 * 1024 * 64; // 64 MiB

/// The amount of concurrent blob uploads
const CONCURRENT_UPLOADS: usize = 8; // value actions/cache uses 

/// A caching backend for github action cache service, using their undocummented api that everyone
/// uses :P.
pub struct GithubActionsBackend {
    /// The http client to use for requests
    cache_client: reqwest::Client,
    /// The base url to send requests to.
    base_url: Box<str>,
    /// The version string to scope everything under.
    ///
    /// Must be 64 bytes long
    version: Box<str>,
}

impl GithubActionsBackend {
    /// Whether the actions cache service is reachable from the current environment.
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

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {token}")
                .parse()
                .wrap_internal("Token is not a valid header value")?,
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json"
                .parse()
                .wrap_internal("constant content type header value isnt ascii?!")?,
        );

        let client = reqwest::ClientBuilder::new()
            .default_headers(headers)
            .user_agent(USER_AGENT)
            .build()
            .wrap_internal("Failed to build reqwest client.")?;

        Ok(Self {
            cache_client: client,
            base_url: base_url.into(),
            version,
        })
    }

    /// Convert a hash into the cache key to use.
    fn hash_to_key(key: CacheHash) -> String {
        let key = base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(key.0);
        format!("serpentine-{key}")
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

impl CacheBackend for GithubActionsBackend {
    fn read_key(&self, key: CacheHash) -> BoxFuture<'_, Option<BoxedReader>> {
        Box::pin(async { todo!() })
    }

    fn write_key(&self, key: CacheHash) -> BoxFuture<'_, Option<BoxedWriter>> {
        Box::pin(async move {
            log::debug!("Attempting to create cache entry.");
            let create_request = CreateCacheEntry {
                key: Self::hash_to_key(key),
                version: &self.version,
            };

            log::trace!("sending {create_request:#?}");

            let response = self
                .cache_client
                .post(format!("{}/CreateCacheEntry", self.base_url))
                .json(&create_request)
                .send()
                .await
                .inspect_err(|err| log::error!("{err}"))
                .ok()?;

            if !response.status().is_success() {
                log::error!("Got status: {}", response.status());
                if let Ok(body) = response.text().await {
                    log::error!("{body}");
                }
                return None;
            }

            let response: CreateCacheEntryResponse = response
                .json()
                .await
                .inspect_err(|err| log::error!("{err}"))
                .ok()?;

            log::trace!("got response: {response:#?}");

            if !response.ok {
                log::error!("Got error from response: {}", response.message);
                return None;
            }

            let signed_upload_url = response.signed_upload_url;

            todo!()
        })
    }

    fn get_data_cache(&self) -> BoxFuture<'_, Option<BoxedReader>> {
        Box::pin(async { todo!() })
    }

    fn get_data_cache_writer(&self) -> BoxFuture<'_, miette::Result<BoxedWriter>> {
        Box::pin(async { todo!() })
    }
}

/// A writer for the azure blob storage, that buffers input and commits on shutdown.
struct AzureBlobWriter {
    /// the buffer of bytes of the next block to write
    buffer: Box<[u8]>,
    /// The length of filled bytes in the buffer
    buffer_filled: usize,
    /// The inflight futures uploading blocks .
    upload_futures: tokio::task::JoinSet<Result<(), std::io::Error>>,
}

impl AzureBlobWriter {
    /// Create a new azure blob writer that writes to the given pre-signed url.
    fn new() -> Self {
        Self {
            buffer: vec![0; BLOCK_SIZE].into_boxed_slice(),
            buffer_filled: 0,
            upload_futures: tokio::task::JoinSet::new(),
        }
    }
}

impl AsyncWrite for AzureBlobWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        log::trace!("Attempting to write {buf:?} to azure");
        let this = &mut *self;

        // Poll the join set to ensure any done futures get drop
        // surfaced.
        //
        // Its safe for us to not be actively polling this at every possible moment because `JoinSet` always drives futures in the background.
        // We only need to poll the joinset to clean out space for more futures and registering
        // wakers for when we are waiting on the set to clear up.
        match this.upload_futures.poll_join_next(cx) {
            Poll::Ready(Some(Err(err))) => {
                log::error!("{err}");
                return Poll::Ready(Err(std::io::Error::other(err)));
            }
            Poll::Ready(Some(Ok(Err(err)))) => {
                log::error!("{err}");
                return Poll::Ready(Err(err));
            }

            Poll::Pending => {
                log::trace!("Join set pending");
                // Unless we are at the cap its safe for us to continue to the `Ready` branch
                if this.upload_futures.len() >= CONCURRENT_UPLOADS {
                    log::debug!("Concurrency bound reached on upload, pending.");
                    // The poll above, since it returned pending, registers wakers.
                    return Poll::Pending;
                }
            }
            // Since its ready then a item was just poped or it was empty so its always safe for us
            // to (as in stays under the cap) to register one more waker
            Poll::Ready(_) => {
                log::debug!("block upload done.");
            }
        }

        let written = this
            .buffer
            .get_mut(this.buffer_filled..)
            .and_then(|mut slice| slice.write(buf).ok())
            .unwrap_or(0);
        this.buffer_filled = this.buffer_filled.saturating_add(written);

        if this.buffer_filled == this.buffer.len() {
            log::debug!("Buffer filled");
            todo!("Send block");
        }

        log::trace!("wrote {written} bytes from caller.");
        Poll::Ready(Ok(written))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        todo!()
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        ready!(Self::poll_flush(self, cx))?;

        todo!()
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
