//! UTC wire timestamps with stable historical serialization.
use jiff::{fmt::temporal::DateTimePrinter, SignedDuration, Timestamp};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::ops::{Add, AddAssign, Sub, SubAssign};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TimestampParseError(String);
impl From<&str> for TimestampParseError {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}
impl From<String> for TimestampParseError {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(transparent))]
pub struct WireTimestamp(Timestamp);

// Earliest UTC instant that prints with a four-digit year. Jiff supplies
// the upper bound, chosen to support all of its legal UTC offsets.
const MIN_WIRE_SECOND: i64 = -62_167_219_200;

impl WireTimestamp {
    fn in_wire_range(ts: Timestamp) -> bool {
        ts >= Timestamp::from_second(MIN_WIRE_SECOND).expect("wire minimum is valid")
    }
    pub fn checked_add(self, duration: SignedDuration) -> Option<Self> {
        let ts = self.0.checked_add(duration).ok()?;
        Self::in_wire_range(ts).then_some(Self(ts))
    }
    pub fn now() -> Self {
        Self(Timestamp::now())
    }
    pub fn from_second(seconds: i64) -> Result<Self, String> {
        let ts = Timestamp::from_second(seconds).map_err(|error| error.to_string())?;
        Self::in_wire_range(ts)
            .then_some(Self(ts))
            .ok_or_else(|| "timestamp outside supported UTC range".into())
    }
    pub fn from_millisecond(value: i64) -> Result<Self, String> {
        let ts = Timestamp::from_millisecond(value).map_err(|error| error.to_string())?;
        Self::in_wire_range(ts)
            .then_some(Self(ts))
            .ok_or_else(|| "timestamp outside supported UTC range".into())
    }
    pub fn from_parts(seconds: i64, nanos: u32) -> Option<Self> {
        Timestamp::new(seconds, i32::try_from(nanos).ok()?)
            .ok()
            .filter(|&ts| Self::in_wire_range(ts))
            .map(Self)
    }
    pub fn parse_rfc3339(s: &str) -> Result<Self, TimestampParseError> {
        s.parse()
    }
    pub fn rfc3339_millis(self) -> String {
        DateTimePrinter::new()
            .precision(Some(3))
            .timestamp_to_string(&self.0)
    }
    pub fn rfc3339_seconds(self) -> String {
        DateTimePrinter::new()
            .precision(Some(0))
            .timestamp_to_string(&self.0)
    }
    pub fn as_second(self) -> i64 {
        self.0.as_second()
    }
    pub fn duration_since(self, other: Self) -> SignedDuration {
        self.0.duration_since(other.0)
    }
    fn serialized(self) -> String {
        let nanos = self.0.subsec_nanosecond();
        let precision = if nanos == 0 {
            0
        } else if nanos % 1_000_000 == 0 {
            3
        } else if nanos % 1_000 == 0 {
            6
        } else {
            9
        };
        DateTimePrinter::new()
            .precision(Some(precision))
            .timestamp_to_string(&self.0)
    }
    pub fn rfc3339(self) -> String {
        self.serialized().replace('Z', "+00:00")
    }
}
impl std::str::FromStr for WireTimestamp {
    type Err = TimestampParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let b = s.as_bytes();
        let digits =
            |r: std::ops::Range<usize>| b.get(r).is_some_and(|v| v.iter().all(u8::is_ascii_digit));
        if b.len() < 20
            || !digits(0..4)
            || b[4] != b'-'
            || !digits(5..7)
            || b[7] != b'-'
            || !digits(8..10)
            || !matches!(b[10], b'T' | b't' | b' ')
            || !digits(11..13)
            || b[13] != b':'
            || !digits(14..16)
            || b[16] != b':'
            || !digits(17..19)
        {
            return Err("invalid RFC3339 timestamp".into());
        }
        if &b[17..19] == b"60" {
            return Err("leap-second timestamps are unsupported".into());
        }
        let mut end = 19;
        if b.get(end) == Some(&b'.') {
            end += 1;
            let start = end;
            while b.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            if end == start {
                return Err("invalid RFC3339 fraction".into());
            }
        }
        let offset = b.get(end..).unwrap_or_default();
        let valid_offset = matches!(offset, [b'Z' | b'z'])
            || (offset.len() == 6
                && matches!(offset[0], b'+' | b'-')
                && offset[1..3].iter().all(u8::is_ascii_digit)
                && offset[3] == b':'
                && offset[4..6].iter().all(u8::is_ascii_digit));
        if !valid_offset
            || (offset.len() == 6
                && (&offset[1..3] > b"23".as_slice() || &offset[4..6] > b"59".as_slice()))
        {
            return Err("invalid RFC3339 offset".into());
        }
        let normalized = if end > 29 && b[19] == b'.' {
            format!("{}{}", &s[..29], &s[end..])
        } else {
            s.to_owned()
        };
        let timestamp = normalized
            .parse::<Timestamp>()
            .map_err(|e| TimestampParseError(e.to_string()))?;
        if !Self::in_wire_range(timestamp) {
            return Err("timestamp outside supported UTC range".into());
        }
        Ok(Self(timestamp))
    }
}
impl Serialize for WireTimestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.serialized())
    }
}
impl<'de> Deserialize<'de> for WireTimestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}
impl Add<SignedDuration> for WireTimestamp {
    type Output = Self;
    fn add(self, duration: SignedDuration) -> Self {
        self.checked_add(duration)
            .expect("timestamp outside supported UTC range")
    }
}
impl Sub<SignedDuration> for WireTimestamp {
    type Output = Self;
    fn sub(self, duration: SignedDuration) -> Self {
        self.checked_add(duration.checked_neg().expect("duration out of range"))
            .expect("timestamp outside supported UTC range")
    }
}
impl AddAssign<SignedDuration> for WireTimestamp {
    fn add_assign(&mut self, duration: SignedDuration) {
        *self = *self + duration;
    }
}
impl SubAssign<SignedDuration> for WireTimestamp {
    fn sub_assign(&mut self, duration: SignedDuration) {
        *self = *self - duration;
    }
}
impl Sub for WireTimestamp {
    type Output = SignedDuration;
    fn sub(self, other: Self) -> SignedDuration {
        self.duration_since(other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checked_arithmetic_preserves_wire_year_range() {
        let earliest = WireTimestamp::from_second(MIN_WIRE_SECOND).unwrap();
        let latest = WireTimestamp(Timestamp::MAX);
        assert!(WireTimestamp::from_second(MIN_WIRE_SECOND - 1).is_err());
        assert!(earliest
            .checked_add(SignedDuration::from_nanos(-1))
            .is_none());
        assert!("0000-01-01T00:59:59.999999999+01:00"
            .parse::<WireTimestamp>()
            .is_err());
        assert!(earliest
            .checked_add(SignedDuration::from_secs(-1))
            .is_none());
        assert!(latest.checked_add(SignedDuration::from_secs(1)).is_none());
    }
    #[test]
    fn rejects_non_rfc3339_temporal_forms() {
        for input in [
            "2025-01-02T03:04Z",
            "2025-01-02T03:04:05+01",
            "2025-01-02T03:04:05+0100",
            "2025-01-02T03:04:05Z[UTC]",
            "2025-01-02T03:04:60Z",
            "2025-01-02T03:04:05+24:00",
            "0000-01-01T00:00:00+01:00",
        ] {
            assert!(input.parse::<WireTimestamp>().is_err(), "{input}");
        }
    }
    #[test]
    fn timestamp_json_preserves_canonical_fraction_width() {
        for (input, expected) in [
            ("2025-01-02T03:04:05Z", "2025-01-02T03:04:05Z"),
            ("2025-01-02T03:04:05.1Z", "2025-01-02T03:04:05.100Z"),
            ("2025-01-02T03:04:05.1234Z", "2025-01-02T03:04:05.123400Z"),
            (
                "2025-01-02T03:04:05.1234567Z",
                "2025-01-02T03:04:05.123456700Z",
            ),
            ("2025-01-02T04:04:05+01:00", "2025-01-02T03:04:05Z"),
            ("2025-01-02t01:04:05-02:00", "2025-01-02T03:04:05Z"),
            ("2025-01-02 03:04:05z", "2025-01-02T03:04:05Z"),
            (
                "2025-01-02T03:04:05.12345678999Z",
                "2025-01-02T03:04:05.123456789Z",
            ),
            ("1969-12-31T23:59:59.1Z", "1969-12-31T23:59:59.100Z"),
        ] {
            let timestamp: WireTimestamp = input.parse().unwrap();
            assert_eq!(
                serde_json::to_string(&timestamp).unwrap(),
                format!("\"{expected}\"")
            );
        }
    }
}
