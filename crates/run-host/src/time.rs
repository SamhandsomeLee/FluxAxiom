use fluxaxiom_contract::Timestamp;
use time::{Duration, OffsetDateTime};

/// Attempt 租约时长（秒）。创建与每次续期都写成 `now + TTL`。
pub(crate) const LEASE_TTL_SECS: i64 = 60;

/// 运行中续期间隔（秒）。A3b：20s 续一次，相对 60s TTL 留余量。
pub(crate) const LEASE_RENEW_INTERVAL_SECS: u64 = 20;

/// 当前 UTC 时刻，契约 `Timestamp` 形状。
///
/// **作用**：写入 `Run.updated_at`、event `occurred_at`、以及与租约比较。
/// **边界**：精度到秒；毫秒被截断。
pub(crate) fn now_utc() -> Timestamp {
    to_timestamp(OffsetDateTime::now_utc())
}

/// 新租约的截止时间：`now + LEASE_TTL_SECS`。
///
/// **作用**：`prepare_drive` 首次写入与 `renew_lease` 续期共用。
/// **边界**：每次调用重新取系统时钟，不是相对上一次截止时间累加。
pub(crate) fn lease_expires_at() -> Timestamp {
    to_timestamp(OffsetDateTime::now_utc() + Duration::seconds(LEASE_TTL_SECS))
}

/// 租约是否已到期（`now >= lease_expires_at`）。
///
/// **作用**：`reap_expired_leases` 过滤候选 Attempt。
/// **参数**：两边都必须是契约 UTC `Z` 时间戳。
/// **边界**：字典序与时间序一致；相等视为已到期。
pub(crate) fn lease_is_expired(lease_expires_at: &Timestamp, now: &Timestamp) -> bool {
    now.as_str() >= lease_expires_at.as_str()
}

/// 把 `OffsetDateTime` 收成契约时间戳。
///
/// **作用**：统一成 `YYYY-MM-DDTHH:MM:SSZ`，供入库与比较。
/// **边界**：要求 UTC 且以 `Z` 结尾；不用 RFC3339 的 `+00:00` 形式。格式非法会 panic（本模块只喂 UTC）。
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
