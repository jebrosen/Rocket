use std::convert::TryInto;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::{future::BoxFuture, stream::StreamExt};
use tokio::io::AsyncRead;

use crate::{Manifest, Request};
use crate::response::{Body, Response};
use crate::http::hyper::{self, header, Bytes, HttpBody};
use crate::ext::{AsyncReadExt, IntoBytesStream};

/// Utility data structure for keeping a Response with the Request it might borrow data from
struct Inner {
    rocket: Arc<Manifest>,
    // 'request' borrows from 'rocket'
    request: Option<Request<'static>>,
    // 'response' borrows from 'request'
    response: Option<Response<'static>>,
    // 'stream' borrows from 'request'
    stream: Option<IntoBytesStream<Pin<Box<dyn AsyncRead + Send>>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Drop in the correct order
        self.stream = None;
        self.response = None;
        self.request = None;
    }
}

pub struct ReqResPair(Box<Inner>);

pub enum PayloadKind {
    Empty,
    ReqRes(ReqResPair),
}

// Safety: No methods on PayloadKind take `&Self`
unsafe impl Sync for PayloadKind { }

impl ReqResPair {
    pub fn new(rocket: Arc<Manifest>) -> Self {
        Self(Box::new(Inner {
            rocket,
            request: None,
            response: None,
            stream: None,
        }))
    }

    pub fn try_set_request<F, E>(&mut self, f: F) -> Result<(), E>
    where
        F: for<'r> FnOnce(&'r Manifest) -> Result<Request<'r>, E>
    {
        assert!(self.0.response.is_none(), "try_set_request was called after set_response");

        let req = f(&self.0.rocket)?;

        // Safety: This structure keeps an Arc<Rocket> (containing the data in 'r) alive for longer than req.
        self.0.request = Some(unsafe { std::mem::transmute::<Request<'_>, Request<'static>>(req) });

        Ok(())
    }

    pub async fn set_response<F>(&mut self, f: F)
    where
        F: for<'a, 'r> FnOnce(&'a Manifest, &'r mut Request<'a>) -> BoxFuture<'r, Response<'r>>
    {
        assert!(self.0.request.is_some(), "set_response was called before try_set_request");
        // Setting a second response would require dropping the first one
        assert!(self.0.response.is_none(), "set_response was called twice");

        // Safety: Shortening this lifetime is safe becuase Request is covariant over its lifetime parameter
        let req = unsafe { std::mem::transmute::<&mut Request<'static>, &mut Request<'_>>(self.0.request.as_mut().unwrap()) };
        let res = f(&self.0.rocket, req).await;

        // Safety: Response will definitely be dropped before request, and the request will never move.
        self.0.response = Some(unsafe { std::mem::transmute::<Response<'_>, Response<'static>>(res) });
    }

    pub fn into_hyper_response(mut self: Self) -> Result<hyper::Response<PayloadKind>, std::io::Error> {
        assert!(self.0.response.is_some(), "into_hyper_response was called before set_response");

        let response_mut = self.0.response.as_mut().unwrap();

        let mut hyp_res = hyper::Response::builder();
        hyp_res = hyp_res.status(response_mut.status().code);

        for header in response_mut.headers().iter() {
            let name = header.name.as_str();
            let value = header.value.as_bytes();
            hyp_res = hyp_res.header(name, value);
        }

        let payload;

        match response_mut.take_body() {
            None => {
                hyp_res = hyp_res.header(header::CONTENT_LENGTH, "0");
                payload = PayloadKind::Empty;
            }
            Some(body) => {
                let (body, chunk_size) = match body {
                    Body::Chunked(body, chunk_size) => {
                        (body, chunk_size.try_into().expect("u64 -> usize overflow"))
                    }
                    Body::Sized(body, size) => {
                        hyp_res = hyp_res.header(header::CONTENT_LENGTH, size.to_string());
                        (body, 4096_usize)
                    }
                };

                // Safety: This structure keeps 'request' alive longer than 'stream'
                let fake_static_body: Pin<Box<dyn AsyncRead + Send + 'static>> = unsafe {
                    std::mem::transmute::<Pin<Box<dyn AsyncRead + Send + '_>>, Pin<Box<dyn AsyncRead + Send + 'static>>>(body)
                };
                self.0.stream = Some(fake_static_body.into_bytes_stream(chunk_size));
                payload = PayloadKind::ReqRes(self)
            }
        };

        hyp_res.body(payload).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    }
}

impl HttpBody for PayloadKind {
    type Data = Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

    fn poll_data(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        match *self {
            PayloadKind::Empty => Poll::Ready(None),
            PayloadKind::ReqRes(ref mut req_res) => {
                let stream = req_res.0.stream.as_mut()
                    .expect("internal invariant broken: PayloadKind.stream was None");
//                // TODO: benchmark/justify
//                // Safety: PayloadKind can only be constructed by the same method that sets stream to Some
//                    .unwrap_or_else(|| unsafe { std::hint::unreachable_unchecked() });
                stream.poll_next_unpin(cx).map(|optres| optres.map(|res| res.map_err(|e| e.into())))
            }
        }
    }

    fn poll_trailers(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<Option<hyper::HeaderMap>, Self::Error>> {
        Poll::Ready(Ok(None))
    }
}
