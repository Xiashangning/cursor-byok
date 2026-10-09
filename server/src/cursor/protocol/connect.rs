//! Encodes and decodes Connect protocol frames.
use axum::{
    body::Body,
    http::{header, HeaderValue, Response},
};
use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;
use serde::Serialize;

use crate::{Error, Result};

pub const END_STREAM_FLAG: u8 = 0x02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectCode {
    Canceled,
    InvalidArgument,
    NotFound,
    Unavailable,
    Internal,
}

impl ConnectCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Canceled => "canceled",
            Self::InvalidArgument => "invalid_argument",
            Self::NotFound => "not_found",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConnectErrorDetail {
    #[serde(rename = "type")]
    pub type_name: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectStreamError {
    pub code: ConnectCode,
    pub message: String,
    pub details: Vec<ConnectErrorDetail>,
}

#[derive(Serialize)]
struct EndStreamResponse<'a> {
    error: WireError<'a>,
}

#[derive(Serialize)]
struct WireError<'a> {
    code: &'static str,
    #[serde(skip_serializing_if = "str::is_empty")]
    message: &'a str,
    #[serde(skip_serializing_if = "details_are_empty")]
    details: &'a [ConnectErrorDetail],
}

fn details_are_empty(details: &&[ConnectErrorDetail]) -> bool {
    details.is_empty()
}

pub fn encode_message<M: Message>(message: &M) -> Result<Bytes> {
    let len = message.encoded_len();
    let mut output = BytesMut::with_capacity(5 + len);
    output.put_u8(0);
    output.put_u32(len as u32);
    message.encode(&mut output)?;
    Ok(output.freeze())
}

pub fn encode_end_stream() -> Bytes {
    encode_end_stream_payload(b"{}")
}

pub fn encode_error_end_stream(error: &ConnectStreamError) -> Result<Bytes> {
    let payload = serde_json::to_vec(&EndStreamResponse {
        error: WireError {
            code: error.code.as_str(),
            message: &error.message,
            details: &error.details,
        },
    })?;
    Ok(encode_end_stream_payload(&payload))
}

fn encode_end_stream_payload(payload: &[u8]) -> Bytes {
    let mut output = BytesMut::with_capacity(5 + payload.len());
    output.put_u8(END_STREAM_FLAG);
    output.put_u32(payload.len() as u32);
    output.extend_from_slice(payload);
    output.freeze()
}

/// Unary Connect response: a raw protobuf body with an application/proto content type.
pub fn proto_response<M: Message>(message: &M) -> Response<Body> {
    proto_bytes(message.encode_to_vec())
}

pub fn proto_bytes(body: Vec<u8>) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    response
}

/// 单帧 unary 体的判定,decode_unary 与转发方共用同一口径:
/// 首字节非 END_STREAM 标志且声明长度恰好等于剩余字节数。
pub fn is_framed_unary(body: &[u8]) -> bool {
    body.len() >= 5
        && body[0] & END_STREAM_FLAG == 0
        && u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize == body.len() - 5
}

pub fn decode_unary<M: Message + Default>(body: &[u8]) -> Result<M> {
    if is_framed_unary(body) {
        return Ok(M::decode(&body[5..])?);
    }
    Ok(M::decode(body)?)
}

/// Incremental Connect envelopes: HTTP chunks may split or combine frames.
#[derive(Default)]
pub struct FrameDecoder {
    pending: BytesMut,
}

impl FrameDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<(u8, Bytes)>> {
        self.pending.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while self.pending.len() >= 5 {
            let length = u32::from_be_bytes(self.pending[1..5].try_into().unwrap()) as usize;
            if length > 64 * 1024 * 1024 {
                self.pending.clear();
                return Err(Error::Protocol("Connect frame exceeds 64 MiB".into()));
            }
            if self.pending.len() < length + 5 {
                break;
            }
            let frame = self.pending.split_to(length + 5).freeze();
            frames.push((frame[0], frame.slice(5..)));
        }
        Ok(frames)
    }

    pub fn finish(&self) -> Result<()> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(Error::Protocol("truncated Connect envelope".into()))
        }
    }
}

pub fn decode_frames(body: &[u8]) -> Result<Vec<(u8, Bytes)>> {
    let mut decoder = FrameDecoder::default();
    let frames = decoder.push(body)?;
    decoder.finish()?;
    Ok(frames)
}
