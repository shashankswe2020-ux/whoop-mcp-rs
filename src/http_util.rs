//! Small helpers for hyper-based HTTP servers.

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{Response, StatusCode};
use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;

/// Response body used by all servers.
pub type Body = BoxBody<Bytes, Infallible>;

/// Complete (non-streaming) body.
pub fn full(data: impl Into<Bytes>) -> Body {
    Full::new(data.into()).boxed()
}

/// Empty body.
pub fn empty() -> Body {
    full(Bytes::new())
}

/// Build a response with a status, headers, and body.
pub fn response(status: u16, headers: &[(&str, &str)], body: Body) -> Response<Body> {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(body)
        .unwrap_or_else(|_| Response::new(empty()))
}

/// JSON response with `Content-Type` and `Content-Length`.
pub fn json_response(
    status: u16,
    value: &serde_json::Value,
    extra: &[(&str, &str)],
) -> Response<Body> {
    let body = crate::js::stringify(value);
    let length = body.len().to_string();
    let mut headers = vec![
        ("Content-Type", "application/json"),
        ("Content-Length", length.as_str()),
    ];
    headers.extend_from_slice(extra);
    response(status, &headers, full(body))
}

/// Streaming body fed by an unbounded channel of chunks.
///
/// The stream ends when all senders drop or when an empty chunk is sent.
pub struct ChannelBody {
    rx: mpsc::UnboundedReceiver<Bytes>,
    done: bool,
}

impl ChannelBody {
    /// Create a body and its sender.
    pub fn channel() -> (mpsc::UnboundedSender<Bytes>, Body) {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, Self { rx, done: false }.boxed())
    }
}

impl http_body::Body for ChannelBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
        if self.done {
            return Poll::Ready(None);
        }
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(chunk)) if !chunk.is_empty() => {
                Poll::Ready(Some(Ok(http_body::Frame::data(chunk))))
            }
            Poll::Ready(_) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done
    }
}

/// A body that holds a guard until the body is dropped (response finished or aborted).
pub struct GuardedBody<G: Send + 'static> {
    inner: Body,
    _guard: G,
}

impl<G: Send + Sync + Unpin + 'static> GuardedBody<G> {
    /// Attach `guard` to `inner`.
    pub fn wrap(inner: Body, guard: G) -> Body {
        Self {
            inner,
            _guard: guard,
        }
        .boxed()
    }
}

impl<G: Send + Sync + Unpin + 'static> http_body::Body for GuardedBody<G> {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Infallible>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// Parse the request target against `http://{host}` like `new URL(req.url, base)`.
pub fn request_url(uri: &hyper::Uri, host: &str) -> Option<url::Url> {
    let base = url::Url::parse(&format!("http://{host}")).ok()?;
    let target = uri.path_and_query().map_or("/", |pq| pq.as_str());
    base.join(target).ok()
}

/// First value of a query parameter (`URLSearchParams.get`).
pub fn query_param(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}
