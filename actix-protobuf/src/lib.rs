//! Protobuf payload extractor for Actix Web.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, nonstandard_style)]
#![warn(future_incompatible)]

use std::{
    fmt,
    future::Future,
    ops::{Deref, DerefMut},
    pin::Pin,
    task::{self, Poll},
};

use actix_web::{
    body::BoxBody,
    dev::Payload,
    error::PayloadError,
    http::header::{CONTENT_LENGTH, CONTENT_TYPE},
    web::BytesMut,
    Error, FromRequest, HttpMessage, HttpRequest, HttpResponse, HttpResponseBuilder, Responder,
    ResponseError,
};
use derive_more::Display;
use futures_util::{
    future::{FutureExt as _, LocalBoxFuture},
    stream::StreamExt as _,
};
use prost::{DecodeError as ProtoBufDecodeError, EncodeError as ProtoBufEncodeError, Message};

#[derive(Debug, Display)]
pub enum ProtoBufPayloadError {
    /// Payload size is bigger than 256k
    #[display(fmt = "Payload size is bigger than 256k")]
    Overflow,

    /// Content type error
    #[display(fmt = "Content type error")]
    ContentType,

    /// Serialize error
    #[display(fmt = "ProtoBuf serialize error: {}", _0)]
    Serialize(ProtoBufEncodeError),

    /// Deserialize error
    #[display(fmt = "ProtoBuf deserialize error: {}", _0)]
    Deserialize(ProtoBufDecodeError),

    /// Payload error
    #[display(fmt = "Error that occur during reading payload: {}", _0)]
    Payload(PayloadError),
}

impl ResponseError for ProtoBufPayloadError {
    fn error_response(&self) -> HttpResponse {
        match *self {
            ProtoBufPayloadError::Overflow => HttpResponse::PayloadTooLarge().into(),
            _ => HttpResponse::BadRequest().into(),
        }
    }
}

impl From<PayloadError> for ProtoBufPayloadError {
    fn from(err: PayloadError) -> ProtoBufPayloadError {
        ProtoBufPayloadError::Payload(err)
    }
}

impl From<ProtoBufDecodeError> for ProtoBufPayloadError {
    fn from(err: ProtoBufDecodeError) -> ProtoBufPayloadError {
        ProtoBufPayloadError::Deserialize(err)
    }
}

pub struct ProtoBuf<T: Message>(pub T);

impl<T: Message> Deref for ProtoBuf<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Message> DerefMut for ProtoBuf<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T: Message> fmt::Debug for ProtoBuf<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProtoBuf: {:?}", self.0)
    }
}

impl<T: Message> fmt::Display for ProtoBuf<T>
where
    T: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

pub struct ProtoBufConfig {
    limit: usize,
}

impl ProtoBufConfig {
    /// Change max size of payload. By default max size is 256Kb
    pub fn limit(&mut self, limit: usize) -> &mut Self {
        self.limit = limit;
        self
    }
}

impl Default for ProtoBufConfig {
    fn default() -> Self {
        ProtoBufConfig { limit: 262_144 }
    }
}

impl<T> FromRequest for ProtoBuf<T>
where
    T: Message + Default + 'static,
{
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self, Error>>;

    #[inline]
    fn from_request(req: &HttpRequest, payload: &mut Payload) -> Self::Future {
        let limit = req
            .app_data::<ProtoBufConfig>()
            .map(|c| c.limit)
            .unwrap_or(262_144);
        ProtoBufMessage::new(req, payload)
            .limit(limit)
            .map(move |res| match res {
                Err(e) => Err(e.into()),
                Ok(item) => Ok(ProtoBuf(item)),
            })
            .boxed_local()
    }
}

impl<T: Message + Default> Responder for ProtoBuf<T> {
    type Body = BoxBody;

    fn respond_to(self, _: &HttpRequest) -> HttpResponse {
        let mut buf = Vec::new();
        match self.0.encode(&mut buf) {
            Ok(()) => HttpResponse::Ok()
                .content_type("application/protobuf")
                .body(buf),
            Err(err) => HttpResponse::from_error(Error::from(ProtoBufPayloadError::Serialize(err))),
        }
    }
}

pub struct ProtoBufMessage<T: Message + Default> {
    limit: usize,
    length: Option<usize>,
    stream: Option<Payload>,
    err: Option<ProtoBufPayloadError>,
    fut: Option<LocalBoxFuture<'static, Result<T, ProtoBufPayloadError>>>,
}

impl<T: Message + Default> ProtoBufMessage<T> {
    /// Create `ProtoBufMessage` for request.
    pub fn new(req: &HttpRequest, payload: &mut Payload) -> Self {
        if req.content_type() != "application/protobuf" {
            return ProtoBufMessage {
                limit: 262_144,
                length: None,
                stream: None,
                fut: None,
                err: Some(ProtoBufPayloadError::ContentType),
            };
        }

        let mut len = None;
        if let Some(l) = req.headers().get(CONTENT_LENGTH) {
            if let Ok(s) = l.to_str() {
                if let Ok(l) = s.parse::<usize>() {
                    len = Some(l)
                }
            }
        }

        ProtoBufMessage {
            limit: 262_144,
            length: len,
            stream: Some(payload.take()),
            fut: None,
            err: None,
        }
    }

    /// Change max size of payload. By default max size is 256Kb
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

impl<T: Message + Default + 'static> Future for ProtoBufMessage<T> {
    type Output = Result<T, ProtoBufPayloadError>;

    fn poll(mut self: Pin<&mut Self>, task: &mut task::Context<'_>) -> Poll<Self::Output> {
        if let Some(ref mut fut) = self.fut {
            return Pin::new(fut).poll(task);
        }

        if let Some(err) = self.err.take() {
            return Poll::Ready(Err(err));
        }

        let limit = self.limit;
        if let Some(len) = self.length.take() {
            if len > limit {
                return Poll::Ready(Err(ProtoBufPayloadError::Overflow));
            }
        }

        let mut stream = self
            .stream
            .take()
            .expect("ProtoBufMessage could not be used second time");

        self.fut = Some(
            async move {
                let mut body = BytesMut::with_capacity(8192);

                while let Some(item) = stream.next().await {
                    let chunk = item?;
                    if (body.len() + chunk.len()) > limit {
                        return Err(ProtoBufPayloadError::Overflow);
                    } else {
                        body.extend_from_slice(&chunk);
                    }
                }

                Ok(<T>::decode(&mut body)?)
            }
            .boxed_local(),
        );
        self.poll(task)
    }
}

pub trait ProtoBufResponseBuilder {
    fn protobuf<T: Message>(&mut self, value: T) -> Result<HttpResponse, Error>;
}

impl ProtoBufResponseBuilder for HttpResponseBuilder {
    fn protobuf<T: Message>(&mut self, value: T) -> Result<HttpResponse, Error> {
        self.insert_header((CONTENT_TYPE, "application/protobuf"));

        let mut body = Vec::new();
        value
            .encode(&mut body)
            .map_err(ProtoBufPayloadError::Serialize)?;
        Ok(self.body(body))
    }
}

#[cfg(test)]
mod tests {
    use actix_web::http::header;
    use actix_web::test::TestRequest;

    use super::*;

    impl PartialEq for ProtoBufPayloadError {
        fn eq(&self, other: &ProtoBufPayloadError) -> bool {
            match *self {
                ProtoBufPayloadError::Overflow => {
                    matches!(*other, ProtoBufPayloadError::Overflow)
                }
                ProtoBufPayloadError::ContentType => {
                    matches!(*other, ProtoBufPayloadError::ContentType)
                }
                _ => false,
            }
        }
    }

    #[derive(Clone, PartialEq, Eq, Message)]
    pub struct MyObject {
        #[prost(int32, tag = "1")]
        pub number: i32,
        #[prost(string, tag = "2")]
        pub name: String,
    }

    #[actix_web::test]
    async fn test_protobuf() {
        let protobuf = ProtoBuf(MyObject {
            number: 9,
            name: "test".to_owned(),
        });
        let req = TestRequest::default().to_http_request();
        let resp = protobuf.respond_to(&req);
        let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
        assert_eq!(ct, "application/protobuf");
    }

    #[actix_web::test]
    async fn test_protobuf_message() {
        let (req, mut pl) = TestRequest::default().to_http_parts();
        let protobuf = ProtoBufMessage::<MyObject>::new(&req, &mut pl).await;
        assert_eq!(protobuf.err().unwrap(), ProtoBufPayloadError::ContentType);

        let (req, mut pl) = TestRequest::get()
            .insert_header((header::CONTENT_TYPE, "application/text"))
            .to_http_parts();
        let protobuf = ProtoBufMessage::<MyObject>::new(&req, &mut pl).await;
        assert_eq!(protobuf.err().unwrap(), ProtoBufPayloadError::ContentType);

        let (req, mut pl) = TestRequest::get()
            .insert_header((header::CONTENT_TYPE, "application/protobuf"))
            .insert_header((header::CONTENT_LENGTH, "10000"))
            .to_http_parts();
        let protobuf = ProtoBufMessage::<MyObject>::new(&req, &mut pl)
            .limit(100)
            .await;
        assert_eq!(protobuf.err().unwrap(), ProtoBufPayloadError::Overflow);
    }
}
