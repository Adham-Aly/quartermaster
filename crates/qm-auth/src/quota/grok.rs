//! Grok's subscription credits meter: the private gRPC-web call behind grok.com's usage page,
//! `POST grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig` with an empty request message.
//!
//! There is no published schema, so the answer is read with a small bounded protobuf scanner and
//! only these field numbers (as other tools read them; they may change without notice):
//!
//! ```text
//! 1  config
//!    1  credit usage percent      float, 0-100, shared by every product
//!    7  products (repeated)       { 1 product enum, 2 usage percent float }
//!    8  current period            { 1 type (1 monthly, 2 weekly), 2 start, 3 end (Timestamp) }
//! ```
//!
//! Money fields are never read. Proto3 leaves out a zero float, so a missing percent beside a
//! valid period is a real 0% used. Anything malformed is refused, never guessed at.

use super::{QuotaBucket, QuotaError, QuotaKind, QuotaSummary};
use jiff::Timestamp;

/// The empty request message, framed: no compression, length 0.
pub(super) const EMPTY_REQUEST: [u8; 5] = [0, 0, 0, 0, 0];

/// gRPC's canonical status codes that the meter may answer with.
const UNAUTHENTICATED: u32 = 16;
const PERMISSION_DENIED: u32 = 7;
const RESOURCE_EXHAUSTED: u32 = 8;

const FRAME_HEADER: usize = 5;
const TRAILER_FLAG: u8 = 0x80;

/// A Grok product the meter reports separately. Unknown product numbers are left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GrokProduct {
    Api,
    GrokBuild,
    Chat,
}

impl GrokProduct {
    fn from_number(number: u64) -> Option<Self> {
        match number {
            1 => Some(Self::Api),
            2 => Some(Self::GrokBuild),
            4 => Some(Self::Chat),
            _ => None,
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Api => "API",
            Self::GrokBuild => "Grok Build",
            Self::Chat => "Grok chat",
        }
    }
}

/// What a successful HTTP answer from the meter said.
#[derive(Debug, PartialEq)]
pub(super) enum Read {
    /// The meter's reading.
    Summary(QuotaSummary),
    /// A well-formed answer with nothing to read as usage.
    Empty,
    /// The meter turned down this login (gRPC unauthenticated or permission denied).
    Refused,
    /// The meter's own gRPC verdict, mapped to a fixed error.
    Failed(QuotaError),
}

/// Reads a 2xx answer: the gRPC status from the headers (a trailers-only answer) or the trailer
/// frame, then the one data frame.
pub(super) fn read(header_status: Option<&str>, body: &[u8]) -> Read {
    if let Some(status) = header_status {
        match grpc_status(status) {
            Ok(0) => {}
            Ok(code) => return verdict(code),
            Err(e) => return Read::Failed(e),
        }
    }
    match frames(body) {
        Ok(Frames::Status(code)) => verdict(code),
        Ok(Frames::Message(message)) => match parse(message) {
            Ok(Some(summary)) => Read::Summary(summary),
            Ok(None) => Read::Empty,
            Err(e) => Read::Failed(e),
        },
        Err(e) => Read::Failed(e),
    }
}

fn verdict(code: u32) -> Read {
    match code {
        UNAUTHENTICATED | PERMISSION_DENIED => Read::Refused,
        RESOURCE_EXHAUSTED => Read::Failed(QuotaError::RateLimited { retry_after: None }),
        _ => Read::Failed(QuotaError::Unavailable),
    }
}

fn grpc_status(text: &str) -> Result<u32, QuotaError> {
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|code| *code <= 16)
        .ok_or(QuotaError::InvalidResponse)
}

enum Frames<'a> {
    Message(&'a [u8]),
    Status(u32),
}

/// The data frame of a unary gRPC-web answer, or the trailer's non-zero status. A body that
/// isn't framed is taken as the bare message, as some servers send it.
fn frames(body: &[u8]) -> Result<Frames<'_>, QuotaError> {
    let Some(&first) = body.first() else {
        return Err(QuotaError::InvalidResponse);
    };
    // A framed body starts with a flags byte; a protobuf tag is never 0 or 1.
    if !(first == 0 || first == 1 || first & TRAILER_FLAG != 0) {
        return Ok(Frames::Message(body));
    }
    let (mut message, mut status, mut at) = (None, None, 0);
    while at < body.len() {
        let header = body
            .get(at..at + FRAME_HEADER)
            .ok_or(QuotaError::InvalidResponse)?;
        let flags = header[0];
        let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        let length = usize::try_from(length).map_err(|_| QuotaError::InvalidResponse)?;
        let frame = body
            .get(at + FRAME_HEADER..at + FRAME_HEADER + length)
            .ok_or(QuotaError::InvalidResponse)?;
        match flags {
            0 if message.is_none() && status.is_none() => message = Some(frame),
            TRAILER_FLAG if status.is_none() => status = Some(trailer_status(frame)?),
            // Compressed, a second message or trailer, or unknown flags.
            _ => return Err(QuotaError::InvalidResponse),
        }
        at += FRAME_HEADER + length;
    }
    match (status, message) {
        (Some(code), _) if code != 0 => Ok(Frames::Status(code)),
        (_, Some(message)) => Ok(Frames::Message(message)),
        _ => Err(QuotaError::InvalidResponse),
    }
}

/// The trailer frame's `grpc-status`; a trailer without one is a zero status.
fn trailer_status(frame: &[u8]) -> Result<u32, QuotaError> {
    let text = std::str::from_utf8(frame).map_err(|_| QuotaError::InvalidResponse)?;
    for line in text.lines() {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("grpc-status")
        {
            return grpc_status(value);
        }
    }
    Ok(0)
}

/// The meter's message as buckets: the shared credits first, then each known product. `None`
/// when it holds nothing to read as usage.
fn parse(message: &[u8]) -> Result<Option<QuotaSummary>, QuotaError> {
    let root = scan(message)?;
    let config = scan(
        bytes_at(&root, 1)
            .next()
            .ok_or(QuotaError::InvalidResponse)?,
    )?;
    let period = bytes_at(&config, 8).next().map(scan).transpose()?;
    let (start, end) = period
        .as_deref()
        .map_or((None, None), |p| (timestamp_at(p, 2), timestamp_at(p, 3)));
    let kind = period.as_deref().and_then(|p| varint_at(p, 1));
    let valid_period =
        matches!(kind, Some(1 | 2)) && matches!((start, end), (Some(s), Some(e)) if e > s);
    let window_seconds = match (start, end) {
        (Some(start), Some(end)) if valid_period => {
            u64::try_from(end.as_second() - start.as_second()).ok()
        }
        _ => None,
    };
    let bucket = |kind: QuotaKind, used: f32| {
        let mut b = QuotaBucket::new(kind);
        b.remaining_percent = Some(100.0 - f64::from(used).clamp(0.0, 100.0));
        b.resets_at = end;
        b.window_seconds = window_seconds;
        b
    };
    let mut buckets = Vec::new();
    let shared = float_at(&config, 1)?;
    if shared.is_some() || valid_period {
        buckets.push(bucket(QuotaKind::Credits, shared.unwrap_or(0.0)));
    }
    for raw in bytes_at(&config, 7) {
        let product = scan(raw)?;
        let used = float_at(&product, 2)?;
        if used.is_none() && !valid_period {
            continue;
        }
        if let Some(named) = varint_at(&product, 1).and_then(GrokProduct::from_number) {
            buckets.push(bucket(QuotaKind::Product(named), used.unwrap_or(0.0)));
        }
    }
    Ok((!buckets.is_empty()).then(|| QuotaSummary::new(buckets)))
}

/// One protobuf field: its number and its value as the wire says.
#[derive(Debug)]
enum Value<'a> {
    Varint(u64),
    Fixed32([u8; 4]),
    Fixed64,
    Bytes(&'a [u8]),
}

/// Every field of one message, in order, with no schema. Groups and truncation refuse it.
fn scan(mut bytes: &[u8]) -> Result<Vec<(u64, Value<'_>)>, QuotaError> {
    const BAD: QuotaError = QuotaError::InvalidResponse;
    let mut fields = Vec::new();
    while !bytes.is_empty() {
        let key = varint(&mut bytes)?;
        let number = key >> 3;
        if number == 0 || number > 0x1fff_ffff {
            return Err(BAD);
        }
        let value = match key & 7 {
            0 => Value::Varint(varint(&mut bytes)?),
            1 => {
                bytes = bytes.get(8..).ok_or(BAD)?;
                Value::Fixed64
            }
            2 => {
                let length = usize::try_from(varint(&mut bytes)?).map_err(|_| BAD)?;
                let (value, rest) = bytes.split_at_checked(length).ok_or(BAD)?;
                bytes = rest;
                Value::Bytes(value)
            }
            5 => {
                let (value, rest) = bytes.split_first_chunk::<4>().ok_or(BAD)?;
                bytes = rest;
                Value::Fixed32(*value)
            }
            _ => return Err(BAD),
        };
        fields.push((number, value));
    }
    Ok(fields)
}

fn varint(bytes: &mut &[u8]) -> Result<u64, QuotaError> {
    let mut value = 0u64;
    for shift in 0..10 {
        let (&byte, rest) = bytes.split_first().ok_or(QuotaError::InvalidResponse)?;
        *bytes = rest;
        if shift == 9 && byte > 1 {
            return Err(QuotaError::InvalidResponse);
        }
        value |= u64::from(byte & 0x7f) << (shift * 7);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(QuotaError::InvalidResponse)
}

fn bytes_at<'a>(fields: &'a [(u64, Value<'a>)], number: u64) -> impl Iterator<Item = &'a [u8]> {
    fields.iter().filter_map(move |(n, v)| match v {
        Value::Bytes(b) if *n == number => Some(*b),
        _ => None,
    })
}

fn varint_at(fields: &[(u64, Value<'_>)], number: u64) -> Option<u64> {
    fields.iter().find_map(|(n, v)| match v {
        Value::Varint(x) if *n == number => Some(*x),
        _ => None,
    })
}

/// A float field; a non-finite value refuses the message.
fn float_at(fields: &[(u64, Value<'_>)], number: u64) -> Result<Option<f32>, QuotaError> {
    let found = fields.iter().find_map(|(n, v)| match v {
        Value::Fixed32(raw) if *n == number => Some(f32::from_le_bytes(*raw)),
        _ => None,
    });
    match found {
        Some(value) if !value.is_finite() => Err(QuotaError::InvalidResponse),
        found => Ok(found),
    }
}

/// A `google.protobuf.Timestamp` submessage's seconds.
fn timestamp_at(fields: &[(u64, Value<'_>)], number: u64) -> Option<Timestamp> {
    let message = scan(bytes_at(fields, number).next()?).ok()?;
    Timestamp::from_second(i64::try_from(varint_at(&message, 1)?).ok()?).ok()
}
