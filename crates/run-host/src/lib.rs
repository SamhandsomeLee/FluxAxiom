//! Run Host：Session、Run、Attempt 的权威存储与 Engine 调度。
//!
//! Engine 只通过 `HostPort` 提交观测。A1.1 只落三张状态表；
//! 事件、blob、租约过期扫描不在本 crate 的这一阶段。

mod error;
mod host;
mod port;
mod store;
mod time;

pub use error::HostError;
pub use host::{CreateRun, CreatedRun, RunHost};
