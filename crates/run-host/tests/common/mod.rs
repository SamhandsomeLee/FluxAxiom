//! 集成测试共用的创建参数。
//!
//! 引擎身份必须和 `drive.rs` 里的 Fake Engine 一致：Attempt 在创建时就写入
//! `engine_id` / `engine_version`，drive 时不再改。

use fluxaxiom_contract::Budget;
use fluxaxiom_engine_api::EngineDescriptor;
use fluxaxiom_run_host::CreateRun;

/// 测试用 Engine 身份。不是正式 crate，也不会进生产组装。
pub const FAKE_ENGINE_ID: &str = "fake";
pub const FAKE_ENGINE_VERSION: &str = "0.1.0";

/// 一份合法的创建请求。调用方只改自己要测的字段（`request_id`、`session_id`）。
pub fn create_run(request_id: impl Into<String>) -> CreateRun {
    CreateRun {
        request_id: request_id.into(),
        session_id: None,
        input_ref: "analyze runtime".to_owned(),
        workspace_ref: "workspace:test".to_owned(),
        budget: Budget {
            max_model_calls: 8,
            max_tool_calls: 32,
        },
        engine: EngineDescriptor {
            engine_id: FAKE_ENGINE_ID.to_owned(),
            version: FAKE_ENGINE_VERSION.to_owned(),
        },
    }
}
