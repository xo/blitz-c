//! Sub-resource fetching for rendered documents: an implementation of
//! [`blitz_traits::net::NetProvider`].
//!
//! This exists instead of `blitz-net` because that crate enables reqwest's
//! `native-tls` feature unconditionally — not behind a cargo feature — which
//! puts OpenSSL in the graph and `-lssl -lcrypto` on every downstream link.
//! Cargo features are additive, so no amount of `default-features = false` here
//! can subtract it; the provider has to live in this crate.
//!
//! It also means `net_timeout` and `user_agent` apply to sub-resources.
//! blitz-net ignored both: it hardcoded its own User-Agent and set no timeout.

use std::collections::HashMap;
use std::ffi::c_int;
use std::future::poll_fn;
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use blitz_traits::net::{Body, Bytes, NetHandler, NetProvider, Request};
use data_url::DataUrl;
use tokio::sync::Semaphore;

use crate::{BLITZ_ERR_NETWORK, ResolvedOptions};

/// Matches real browsers' per-origin cap of 6.
const PER_HOST_MAX_CONCURRENT: usize = 6;

type HostLimits = Arc<Mutex<HashMap<String, Arc<Semaphore>>>>;

pub(crate) struct Provider {
    client: reqwest::Client,
    /// Requests spawned but not yet delivered. The settle loop in `render`
    /// polls this to decide when assets have stopped arriving.
    pending: Arc<AtomicUsize>,
    per_host_limits: HostLimits,
}

impl Provider {
    pub(crate) fn new(options: &ResolvedOptions) -> Result<Self, (c_int, String)> {
        let client = reqwest::Client::builder()
            .timeout(options.net_timeout)
            .user_agent(&options.user_agent)
            .build()
            .map_err(|e| (BLITZ_ERR_NETWORK, format!("http client: {e}")))?;

        Ok(Self {
            client,
            pending: Arc::new(AtomicUsize::new(0)),
            per_host_limits: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Whether any spawned fetch has yet to deliver its bytes.
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.load(Ordering::SeqCst) > 0
    }
}

impl NetProvider for Provider {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let client = self.client.clone();
        let per_host_limits = self.per_host_limits.clone();
        let guard = PendingGuard::new(self.pending.clone());
        let signal = request.signal.clone();

        tokio::spawn(async move {
            let result = match signal {
                // blitz-net checked the signal on every poll of the fetch
                // future; do the same, since an AbortSignal is a bare
                // AtomicBool with no waker to await on.
                Some(signal) => {
                    let mut fut = pin!(fetch_inner(client, request, per_host_limits));
                    poll_fn(|cx| {
                        if signal.aborted() {
                            return Poll::Ready(Err("aborted".to_owned()));
                        }
                        fut.as_mut().poll(cx)
                    })
                    .await
                }
                None => fetch_inner(client, request, per_host_limits).await,
            };

            // A sub-resource that fails is not fatal — the document renders
            // without it, which is why the error is dropped here rather than
            // surfaced. The settle loop is bounded by net_timeout regardless.
            if let Ok((resolved_url, bytes)) = result {
                handler.bytes(resolved_url, bytes);
            }

            // Dropped only now, after the bytes are handed over: the settle
            // loop breaks as soon as the count reaches zero, so decrementing
            // any earlier would let it stop before this resource is registered.
            drop(guard);
        });
    }
}

/// Keeps the pending count accurate even if a fetch task panics or is dropped
/// mid-flight; a leaked increment would make the settle loop spin until its
/// deadline on every render.
struct PendingGuard(Arc<AtomicUsize>);

impl PendingGuard {
    fn new(count: Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn fetch_inner(
    client: reqwest::Client,
    request: Request,
    per_host_limits: HostLimits,
) -> Result<(String, Bytes), String> {
    match request.url.scheme() {
        "data" => {
            let url = DataUrl::process(request.url.as_str())
                .map_err(|e| format!("data url {}: {e:?}", request.url))?;
            let (bytes, _fragment) = url
                .decode_to_vec()
                .map_err(|e| format!("data url base64 {}: {e:?}", request.url))?;
            Ok((request.url.to_string(), Bytes::from(bytes)))
        }
        "file" => {
            let path = request
                .url
                .to_file_path()
                .map_err(|_| format!("bad file url: {}", request.url))?;
            let bytes =
                std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
            Ok((request.url.to_string(), Bytes::from(bytes)))
        }
        _ => fetch_http(client, request, per_host_limits).await,
    }
}

async fn fetch_http(
    client: reqwest::Client,
    request: Request,
    per_host_limits: HostLimits,
) -> Result<(String, Bytes), String> {
    let url = request.url;
    let content_type = request.content_type;

    // Held for the life of the request, so a page referencing hundreds of
    // images doesn't open hundreds of connections to one origin.
    let semaphore = {
        let mut limits = per_host_limits.lock().unwrap();
        limits
            .entry(url.host_str().unwrap_or_default().to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(PER_HOST_MAX_CONCURRENT)))
            .clone()
    };
    let _permit = semaphore
        .acquire()
        .await
        .map_err(|e| format!("per-host semaphore: {e}"))?;

    let mut req = client.request(request.method, url).headers(request.headers);
    if let Some(content_type) = content_type.as_deref() {
        req = req.header("Content-Type", content_type);
    }
    req = apply_body(req, request.body, content_type.as_deref());

    // reqwest's errors already name the URL they were for.
    let response = req.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let final_url = response.url().to_string();

    if !status.is_success() {
        return Err(format!("HTTP {status} for {final_url}"));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("body of {final_url}: {e}"))?;
    Ok((final_url, bytes))
}

fn apply_body(
    req: reqwest::RequestBuilder,
    body: Body,
    content_type: Option<&str>,
) -> reqwest::RequestBuilder {
    match body {
        Body::Bytes(bytes) => req.body(bytes),
        // Encoded here rather than with `RequestBuilder::form`, which would
        // mean enabling reqwest's `form` feature to pull in serde_urlencoded
        // for one call site. Multipart is not supported at all: it needs
        // another reqwest feature and nothing in a headless render submits one.
        Body::Form(form) if content_type == Some("application/x-www-form-urlencoded") => {
            let mut encoded = url::form_urlencoded::Serializer::new(String::new());
            for entry in form.iter() {
                encoded.append_pair(&entry.name, entry.value.as_ref());
            }
            req.body(encoded.finish())
        }
        Body::Form(_) | Body::Empty => req,
    }
}
