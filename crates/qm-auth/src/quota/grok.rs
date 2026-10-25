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

/// The data frame the live meter answered with in September 2026 (usage numbers only): 25% of
/// the weekly credits used, Grok Build at the same 25%, the weekly period, and an empty prepaid
/// balance, which is never read.
#[cfg(test)]
pub(super) const LIVE_MESSAGE: [u8; 86] = [
    10, 84, 13, 0, 0, 200, 65, 18, 0, 26, 0, 34, 12, 8, 161, 186, 215, 212, 6, 16, 232, 175, 182,
    166, 3, 42, 12, 8, 161, 175, 252, 212, 6, 16, 232, 175, 182, 166, 3, 58, 7, 8, 2, 21, 0, 0,
    200, 65, 66, 30, 8, 2, 18, 12, 8, 161, 186, 215, 212, 6, 16, 232, 175, 182, 166, 3, 26, 12, 8,
    161, 175, 252, 212, 6, 16, 232, 175, 182, 166, 3, 88, 1, 98, 0, 104, 1,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn framed(message: &[u8], status: Option<&str>) -> Vec<u8> {
        let mut body = vec![0];
        body.extend_from_slice(&u32::try_from(message.len()).unwrap().to_be_bytes());
        body.extend_from_slice(message);
        if let Some(status) = status {
            let trailer = format!("grpc-status:{status}\r\n");
            body.push(TRAILER_FLAG);
            body.extend_from_slice(&u32::try_from(trailer.len()).unwrap().to_be_bytes());
            body.extend_from_slice(trailer.as_bytes());
        }
        body
    }

    fn summary(read: Read) -> QuotaSummary {
        match read {
            Read::Summary(summary) => summary,
            other => panic!("expected a reading, got {other:?}"),
        }
    }

    #[test]
    fn reads_the_live_capture_as_weekly_credits_and_grok_build() {
        for body in [
            framed(&LIVE_MESSAGE, Some("0")),
            framed(&LIVE_MESSAGE, None),
            LIVE_MESSAGE.to_vec(),
        ] {
            let summary = summary(read(None, &body));
            let start: Timestamp = "2026-08-31T19:59:29Z".parse().unwrap();
            let end: Timestamp = "2026-09-07T19:59:29Z".parse().unwrap();
            assert_eq!(summary.buckets.len(), 2);
            for (bucket, kind) in summary.buckets.iter().zip([
                QuotaKind::Credits,
                QuotaKind::Product(GrokProduct::GrokBuild),
            ]) {
                assert_eq!(bucket.kind, kind);
                assert_eq!(bucket.remaining_percent, Some(75.0));
                assert_eq!(bucket.resets_at, Some(end));
                assert_eq!(
                    bucket.window_seconds,
                    Some(u64::try_from((end - start).get_seconds()).unwrap())
                );
            }
            assert_eq!(summary.buckets[0].window_seconds, Some(7 * 86_400));
        }
    }

    /// A config message: an optional shared percent, products, and an optional period.
    fn config(shared: Option<f32>, products: &[(u8, Option<f32>)], period: bool) -> Vec<u8> {
        fn field(number: u8, body: &[u8]) -> Vec<u8> {
            let mut out = vec![number << 3 | 2, u8::try_from(body.len()).unwrap()];
            out.extend_from_slice(body);
            out
        }
        fn float(number: u8, value: f32) -> Vec<u8> {
            let mut out = vec![number << 3 | 5];
            out.extend_from_slice(&value.to_le_bytes());
            out
        }
        let mut config = Vec::new();
        if let Some(shared) = shared {
            config.extend(float(1, shared));
        }
        for (product, used) in products {
            let mut body = vec![8, *product];
            if let Some(used) = used {
                body.extend(float(2, *used));
            }
            config.extend(field(7, &body));
        }
        if period {
            // Weekly: a Timestamp's seconds are field 1.
            let seconds = |mut n: u64| {
                let mut out = vec![8];
                while n >= 0x80 {
                    out.push(u8::try_from(n & 0x7f).unwrap() | 0x80);
                    n >>= 7;
                }
                out.push(u8::try_from(n).unwrap());
                out
            };
            let (start, end) = (seconds(1_000_000_000), seconds(1_000_604_800));
            let mut body = vec![8, 2];
            body.extend(field(2, &start));
            body.extend(field(3, &end));
            config.extend(field(8, &body));
        }
        field(1, &config)
    }

    #[test]
    fn a_missing_percent_is_zero_used_only_beside_a_valid_period() {
        let with_period = summary(read(
            None,
            &config(None, &[(4, None), (9, Some(5.0))], true),
        ));
        assert_eq!(
            with_period
                .buckets
                .iter()
                .map(|b| (b.kind, b.remaining_percent))
                .collect::<Vec<_>>(),
            [
                (QuotaKind::Credits, Some(100.0)),
                (QuotaKind::Product(GrokProduct::Chat), Some(100.0)),
            ]
        );
        assert_eq!(with_period.buckets[0].window_seconds, Some(604_800));
        assert_eq!(read(None, &config(None, &[(2, None)], false)), Read::Empty);
        let shared_only = summary(read(None, &config(Some(140.0), &[], false)));
        assert_eq!(shared_only.buckets[0].remaining_percent, Some(0.0));
        assert_eq!(shared_only.buckets[0].resets_at, None);
    }

    #[test]
    fn grpc_verdicts_map_to_fixed_outcomes() {
        assert_eq!(read(Some("16"), b""), Read::Refused);
        assert_eq!(read(Some("7"), b""), Read::Refused);
        assert_eq!(
            read(Some("8"), b""),
            Read::Failed(QuotaError::RateLimited { retry_after: None })
        );
        assert_eq!(read(Some("14"), b""), Read::Failed(QuotaError::Unavailable));
        assert_eq!(
            read(Some("x"), b""),
            Read::Failed(QuotaError::InvalidResponse)
        );
        assert_eq!(
            read(None, &framed(&LIVE_MESSAGE, Some("16"))),
            Read::Refused
        );
        assert!(matches!(read(Some("0"), &LIVE_MESSAGE), Read::Summary(_)));
    }

    #[test]
    fn malformed_answers_are_refused_not_guessed() {
        let mut compressed = framed(&LIVE_MESSAGE, None);
        compressed[0] = 1;
        let mut truncated = framed(&LIVE_MESSAGE, None);
        truncated.truncate(40);
        let mut two = framed(&LIVE_MESSAGE, None);
        two.extend(framed(&LIVE_MESSAGE, None));
        let nan = config(Some(f32::NAN), &[], true);
        for body in [
            Vec::new(),
            compressed,
            truncated,
            two,
            nan,
            vec![0x80, 0, 0, 0, 0],
            b"{\"not\":\"protobuf\"}".to_vec(),
            vec![10, 200],
            // A deprecated group (wire type 3).
            vec![11],
        ] {
            assert_eq!(
                read(None, &body),
                Read::Failed(QuotaError::InvalidResponse),
                "{body:?}"
            );
        }
    }
}
