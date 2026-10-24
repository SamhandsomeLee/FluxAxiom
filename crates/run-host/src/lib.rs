//! Run Host：Session、Run、Attempt、durable event 的权威存储与 Engine 调度。
//!
//! **作用**：状态变更和对应 event 在同一个 SQLite 事务里提交。原文放在数据目录的 blob 里。
//! **边界**：Engine 只通过 `HostPort` 提交观测、blob 和预算预留。
//! `open` 时扫描租约已过期的 drive；`request_cancel` 供 CLI 接线 Ctrl-C（A3b / ADR-021）。
//! `reserve` 只在单次 `drive_run` 的内存里计数，已用量不入库；同 Run 再 drive 本阶段不允许。

mod blob;
mod error;
mod events;
mod host;
mod port;
mod store;
mod time;

pub use error::HostError;
pub use host::{CreateRun, CreatedRun, RunHost};
