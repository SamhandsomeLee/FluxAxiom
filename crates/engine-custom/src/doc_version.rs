//! 模型别名 → 文档版本表（A4b / §4.12.2）。
//!
//! **作用**：样本导出写入 `model.doc_version` / `doc_checked_at`；运行时不猜「最新」。
//! **边界**：表随 ADR-020 手工更新；别名不在表里时两端都写 `unknown`。

/// 文档记载的模型版本与核对日期。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocVersion {
    /// 文档列出的版本字符串，例如 `DeepSeek-V4.1-Flash-1017`。
    pub version: &'static str,
    /// 核对该条目时的日期（`YYYY-MM-DD`）。
    pub checked_at: &'static str,
}

/// 按请求别名查文档版本。
///
/// **作用**：样本 `model` 段填 `doc_version` / `doc_checked_at`。
/// **参数**：`alias` 是请求里的 `model` 字段（如 `deepseek-flash`）。
/// **边界**：未知别名返回 `None`；调用方写成 `"unknown"`。
#[must_use]
pub fn lookup(alias: &str) -> Option<DocVersion> {
    match alias {
        "deepseek-flash" => Some(DocVersion {
            version: "DeepSeek-V4.1-Flash-1017",
            checked_at: "2026-10-21",
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_alias_matches_adr020_initial_row() {
        let row = lookup("deepseek-flash").expect("listed");
        assert_eq!(row.version, "DeepSeek-V4.1-Flash-1017");
        assert_eq!(row.checked_at, "2026-10-21");
    }

    #[test]
    fn unknown_alias_is_none() {
        assert!(lookup("deepseek-reasoner").is_none());
    }
}
