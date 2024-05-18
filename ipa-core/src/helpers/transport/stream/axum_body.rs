use std::{
    pin::Pin,
    task::{Context, Poll},
};

use axum::body::Body;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use pin_project::pin_project;
use tokio_stream::wrappers::ReceiverStream;

use crate::error::BoxError;

/// This struct is a simple wrapper so that both in-memory-infra and real-world-infra have a
/// unified interface for streams consumed by transport layer.
#[pin_project]
pub struct WrappedAxumBodyStream(#[pin] Body);

impl WrappedAxumBodyStream {

    pub(super) fn new(inner: Body) -> Self {
        Self(inner)
    }

    #[must_use]
    pub fn empty() -> Self {
        Self::new(Body::empty())
    }
}

impl Stream for WrappedAxumBodyStream {
    type Item = Result<Bytes, BoxError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().0.into_data_stream().poll_next_unpin(cx).map_err(|e| BoxError::from(e))
    }
}

// Note that it is possible (although unlikely) that `from_body` panics.
#[cfg(any(test, feature = "test-fixture"))]
impl<Buf: Into<bytes::Bytes>> From<Buf> for WrappedAxumBodyStream {
    fn from(buf: Buf) -> Self {
        Self::new(Body::from(buf.into()))
    }
}

impl WrappedAxumBodyStream {
    pub fn from_receiver_stream(receiver: Box<ReceiverStream<Result<Bytes, BoxError>>>) -> Self {
        Self::new(Body::from_stream(receiver))
    }
}

#[cfg(feature = "real-world-infra")]
#[async_trait::async_trait]
impl<S>  axum::extract::FromRequest<S> for WrappedAxumBodyStream
where
    S: Send + Sync,
{
    type Rejection = crate::net::Error;

    async fn from_request(req: axum::extract::Request, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::new(Body::from(req.into_body())))
    }
}
