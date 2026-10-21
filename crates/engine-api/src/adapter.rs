use std::sync::Arc;

use async_trait::async_trait;

use crate::host::HostPort;
use crate::types::{EngineDescriptor, EngineOutcome, EngineRunRequest};

/// Engine 侧入口。CLI 选择具体实现并注入 Host；`run-host` 不依赖某个 Engine crate。
///
/// Engine 只通过 `HostPort` 提交观测、blob 和预算，不写数据库，也不发 `run.*` / `attempt.*`。
/// 终态由 Host 根据 `run` 返回的 `EngineOutcome` 决定。
#[async_trait]
pub trait EngineAdapter: Send + Sync {
    /// Host 用它写入 Attempt 的 `engine_id` / `engine_version`。这是实现身份，不是权限。
    fn descriptor(&self) -> EngineDescriptor;

    /// 执行一次已由 Host 创建的 Run。`host` 是本次 Run 唯一的写回通道。
    async fn run(&self, request: EngineRunRequest, host: Arc<dyn HostPort>) -> EngineOutcome;
}
