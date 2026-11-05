//! 上下文策略。准备时调用 L0，恒等策略仍用于旧记录。
//!
//! **作用**：以后的遮蔽、摘要和折叠各自实现这个 trait，加进链里。
//! **边界**：恒等策略不改视图，也不往记录里追加编辑。

use crate::record::{ContextRecord, ContextSpec, ReplayError};
use crate::transcript::View;

/// 写入记录的恒等策略标识。
pub const IDENTITY_STRATEGY: &str = "identity@1";

/// 一次可重放的上下文编辑。
///
/// **作用**：`apply` 改视图并记下编辑；`replay` 按记录重做同一件事。
/// **边界**：`ForwardDedup` 与 `SupersedeRead` 已经实现它。`prepare` 每次调用 L0，只在超过软触发后才试 L1。L2 留给以后。
pub trait ContextStrategy: Send + Sync {
    /// 返回这条策略的稳定标识。
    ///
    /// **作用**：写入 `context_strategy`，重放时用来选出实现。
    /// **边界**：只含标识本身，不含本次估算或档案 hash。
    fn id(&self) -> &str;

    /// 在视图上执行编辑，并把编辑追加到记录。
    ///
    /// **作用**：策略链上的一步。`prepare` 调用 L0，并在超过软触发后调用 L1。
    /// **参数**：`view` 是这次准备好的视图，策略可以改其中的工具正文；空视图允许。`spec` 是这次调用的派生规格。`record` 已有估算和硬上限，策略只追加自己的 `edits`。
    /// **边界**：不得删除事实里的 blob。恒等实现三个参数都不改。
    fn apply(&self, view: &mut View, spec: &ContextSpec, record: &mut ContextRecord);

    /// 按记录重放这一条策略。
    ///
    /// **作用**：校验器重建视图时，由管理器按策略名调用。
    /// **参数**：`view` 从事实抄出，策略只应用自己的编辑；空视图允许。`record` 是当时写下的 context 记录，策略名对不上时拒绝。
    /// **边界**：记录对不上时返回 [`ReplayError`]，不猜。
    fn replay(&self, view: &mut View, record: &ContextRecord) -> Result<(), ReplayError>;
}

/// 空链使用的恒等策略。
///
/// **作用**：证明 trait 有一个不改请求的实现。管理器在链为空时直接使用同一标识。
/// **边界**：不读 `spec`，不写 `edits`。
#[derive(Debug, Clone, Copy)]
pub struct Identity;

impl ContextStrategy for Identity {
    /// 返回 `identity@1`。
    ///
    /// **作用**：记录和重放用它认出恒等策略。
    /// **边界**：不带剖面或运行参数。
    fn id(&self) -> &str {
        IDENTITY_STRATEGY
    }

    /// 保持视图和记录不变。
    ///
    /// **作用**：链上如果显式放入恒等策略，请求文字仍与事实相同。
    /// **参数**：`view` 被忽略，空视图也允许。`spec` 被忽略，硬上限为 0 也允许。`record` 被忽略，已有的 `edits` 不会在这里被清空或追加。
    /// **边界**：不追加 `edits`。
    fn apply(&self, _view: &mut View, _spec: &ContextSpec, _record: &mut ContextRecord) {}

    /// 确认记录就是恒等策略且没有编辑。
    ///
    /// **作用**：重放时拒绝把遮蔽记录当成恒等记录。
    /// **参数**：`view` 已从事实抄出，这里不改它。`record` 提供策略名和 `edits`。
    /// **边界**：策略名不符或 `edits` 非空时返回错误。
    fn replay(&self, _view: &mut View, record: &ContextRecord) -> Result<(), ReplayError> {
        if record.context_strategy != self.id() {
            return Err(ReplayError::UnknownStrategy {
                id: record.context_strategy.clone(),
            });
        }
        if !record.edits.is_empty() {
            return Err(ReplayError::IdentityHasEdits);
        }
        Ok(())
    }
}
