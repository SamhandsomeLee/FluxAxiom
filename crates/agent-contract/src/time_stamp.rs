use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimestampError;

impl std::fmt::Display for TimestampError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("timestamp must be UTC RFC3339")
    }
}

impl std::error::Error for TimestampError {}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Timestamp(String);

impl Timestamp {
    pub fn parse(value: impl AsRef<str>) -> Result<Self, TimestampError> {
        let value = value.as_ref();
        if is_utc_timestamp(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err(TimestampError)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Timestamp {
    type Error = TimestampError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<Timestamp> for String {
    fn from(value: Timestamp) -> Self {
        value.0
    }
}

fn is_utc_timestamp(value: &str) -> bool {
    let head = if let Some((head, fraction)) = value.split_once('.') {
        let Some(fraction) = fraction.strip_suffix('Z') else {
            return false;
        };
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        head
    } else {
        let Some(head) = value.strip_suffix('Z') else {
            return false;
        };
        head
    };
    let bytes = head.as_bytes();
    if bytes.len() != 19 {
        return false;
    }
    let digits =
        |range: std::ops::Range<usize>| bytes[range].iter().all(|byte| byte.is_ascii_digit());
    digits(0..4)
        && bytes[4] == b'-'
        && digits(5..7)
        && bytes[7] == b'-'
        && digits(8..10)
        && bytes[10] == b'T'
        && digits(11..13)
        && bytes[13] == b':'
        && digits(14..16)
        && bytes[16] == b':'
        && digits(17..19)
}
