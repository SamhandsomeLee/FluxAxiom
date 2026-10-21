use fluxaxiom_contract::Timestamp;
use time::{Duration, OffsetDateTime};

/// A1.1 创建 Attempt 时写入的租约时长。不续期；过期扫描留给 A3。
pub(crate) const LEASE_TTL_SECS: i64 = 60;

pub(crate) fn now_utc() -> Timestamp {
    to_timestamp(OffsetDateTime::now_utc())
}

pub(crate) fn lease_expires_at() -> Timestamp {
    to_timestamp(OffsetDateTime::now_utc() + Duration::seconds(LEASE_TTL_SECS))
}

/// 契约时间戳要求 UTC 且以 `Z` 结尾。不用 RFC3339 的 `+00:00` 形式。
fn to_timestamp(datetime: OffsetDateTime) -> Timestamp {
    let raw = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        datetime.year(),
        u8::from(datetime.month()),
        datetime.day(),
        datetime.hour(),
        datetime.minute(),
        datetime.second()
    );
    Timestamp::parse(raw).expect("utc formatting matches the contract timestamp")
}
