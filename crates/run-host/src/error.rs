/// Host 存储与调度失败。不把 SQLite 忙等待包装成业务终态；忙超时仍是存储错误。
#[derive(Debug)]
pub enum HostError {
    Sqlite(rusqlite::Error),
    /// blob 文件读写失败。路径留在这个错误里，不放进 `HostPortError`。
    Io(std::io::Error),
    NotFound,
    Invalid(&'static str),
    Corrupt(&'static str),
    Poisoned,
    /// 测试在 COMMIT 前中止事务。生产构建不会返回它。
    #[cfg(test)]
    Aborted,
}

impl std::fmt::Display for HostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "{error}"),
            Self::Io(error) => write!(formatter, "{error}"),
            Self::NotFound => formatter.write_str("record not found"),
            Self::Invalid(message) => formatter.write_str(message),
            Self::Corrupt(message) => {
                write!(formatter, "stored record is corrupt: {message}")
            }
            Self::Poisoned => formatter.write_str("host database mutex was poisoned"),
            #[cfg(test)]
            Self::Aborted => formatter.write_str("commit aborted"),
        }
    }
}

impl std::error::Error for HostError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::NotFound | Self::Invalid(_) | Self::Corrupt(_) | Self::Poisoned => None,
            #[cfg(test)]
            Self::Aborted => None,
        }
    }
}

impl From<rusqlite::Error> for HostError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<std::io::Error> for HostError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) fn is_constraint_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(info, _)
            if info.code == rusqlite::ErrorCode::ConstraintViolation
    )
}
