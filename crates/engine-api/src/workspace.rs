use std::path::{Component, Path, PathBuf};

/// 一次工具调用被允许进入的工作区根。
///
/// **作用**：Host 在 `prepare_drive` 时打开目录，经 `EngineRunRequest` 交给 Engine。工具只接受这个类型。
/// **边界**：只能由 `open` 从调用方已经选定的目录构造。构造时做规范化。P0a 的 `workspace_ref` 存的就是这个目录路径，不另建标识登记表。
#[derive(Clone, PartialEq, Eq)]
pub struct WorkspaceRoot {
    root: PathBuf,
}

impl std::fmt::Debug for WorkspaceRoot {
    /// **作用**：日志里能看出授权根在哪。
    /// **参数**：`formatter` 接收调试文本。
    /// **边界**：只写规范化后的根路径。不写文件正文。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceRoot")
            .field("root", &self.root)
            .finish()
    }
}

impl WorkspaceRoot {
    /// 把调用方给出的目录收成授权根。
    ///
    /// **作用**：后续 `resolve` 都以这个规范化路径为界。
    /// **参数**：`path` 必须是已存在的目录。相对路径按进程当前目录解析，这是调用方的路径，不是模型的路径。
    /// **边界**：不存在、不是目录、或无法规范化时返回 `WorkspaceError::Unavailable`。不跟随指向别处后再把那个别处当成根。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let canonical = canonical_path(path.as_ref()).map_err(|_| WorkspaceError::Unavailable)?;
        if !canonical.is_dir() {
            return Err(WorkspaceError::Unavailable);
        }
        Ok(Self { root: canonical })
    }

    /// 把模型给出的相对路径收成根内的绝对路径。
    ///
    /// **作用**：`list_files`、`read_file`、`search_code` 在读盘前都走这里。
    /// **参数**：`requested` 是相对路径。空字符串表示根自己。`.` 同样表示根。
    /// **边界**：含空字节或换行是 `InvalidPath`，不访问磁盘。`..`、根目录、盘符、以及以 `\` 或 `/` 开头的路径在访问磁盘前就是 `OutsideRoot`。规范化后仍不在根内（含指向根外的符号链接）也是 `OutsideRoot`。目标不存在才是 `NotFound`。
    pub fn resolve(&self, requested: &str) -> Result<PathBuf, WorkspaceError> {
        if requested.is_empty() || requested == "." {
            return Ok(self.root.clone());
        }
        reject_before_io(requested)?;
        let joined = self.root.join(requested);
        let canonical = canonical_path(&joined).map_err(|_| WorkspaceError::NotFound)?;
        if !canonical.starts_with(&self.root) {
            return Err(WorkspaceError::OutsideRoot);
        }
        Ok(canonical)
    }

    /// 规范化后的授权根。
    ///
    /// **作用**：工具用它做 `.gitignore` 和相对路径计算。
    /// **边界**：返回的是 `open` 时的目录，不是模型后来传入的路径。
    pub fn as_path(&self) -> &Path {
        &self.root
    }

    /// 把根内绝对路径写成使用 `/` 的相对路径。
    ///
    /// **作用**：渲染和事件里的路径不随操作系统分隔符变化。
    /// **参数**：`path` 必须已经过 `resolve`。
    /// **边界**：不在根内时返回 `OutsideRoot`，不返回一个看起来合法的相对路径。
    pub fn relative(&self, path: &Path) -> Result<String, WorkspaceError> {
        let suffix = path
            .strip_prefix(&self.root)
            .map_err(|_| WorkspaceError::OutsideRoot)?;
        if suffix.as_os_str().is_empty() {
            return Ok(String::from("."));
        }
        let mut parts = Vec::new();
        for component in suffix.components() {
            match component {
                Component::Normal(part) => {
                    let Some(text) = part.to_str() else {
                        return Err(WorkspaceError::InvalidPath);
                    };
                    parts.push(text);
                }
                _ => return Err(WorkspaceError::OutsideRoot),
            }
        }
        Ok(parts.join("/"))
    }
}

/// 打开或解析授权根时的失败。
///
/// **作用**：把「根不可用」「路径不在根内」「目标不存在」分开。
/// **边界**：不携带文件正文。`Display` 也不写绝对路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceError {
    /// 调用方给的根不存在或不是目录。
    Unavailable,
    /// 模型给的路径在读盘之前就不是根内相对路径，或规范化后落到根外。
    OutsideRoot,
    /// 路径含空字节、换行，或某个分量不是 UTF-8。
    InvalidPath,
    /// 相对路径在根内没有对应文件。
    NotFound,
}

impl std::fmt::Display for WorkspaceError {
    /// **作用**：给调用方一句类别说明。
    /// **参数**：`formatter` 接收显示文本。
    /// **边界**：不写路径，也不写文件正文。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "workspace root is unavailable",
            Self::OutsideRoot => "path is outside the workspace root",
            Self::InvalidPath => "path is not a usable relative path",
            Self::NotFound => "path was not found in the workspace",
        })
    }
}

impl std::error::Error for WorkspaceError {}

/// 在访问磁盘之前拒绝不是根内相对路径的输入。
///
/// **参数**：`requested` 是模型给出的路径文本。
/// **边界**：空字节和换行是 `InvalidPath`。`..`、根目录、盘符、绝对路径，以及以 `\` 或 `/` 开头的路径是 `OutsideRoot`。不调用 `canonicalize`，因此根外文件是否存在不会改变错误类别。
fn reject_before_io(requested: &str) -> Result<(), WorkspaceError> {
    if requested.contains('\0') || requested.contains('\n') || requested.contains('\r') {
        return Err(WorkspaceError::InvalidPath);
    }
    let path = Path::new(requested);
    if path.is_absolute() {
        return Err(WorkspaceError::OutsideRoot);
    }
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                if part.to_str().is_none() {
                    return Err(WorkspaceError::InvalidPath);
                }
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(WorkspaceError::OutsideRoot);
            }
        }
    }
    Ok(())
}

/// 规范化路径，保留 `canonicalize` 给出的前缀。
///
/// **参数**：`path` 必须已经存在。
/// **边界**：不去掉 `\\?\`。超过传统 260 字符的路径依赖这个前缀才能继续打开。根和目标都走这里，`starts_with` 才对得齐。
fn canonical_path(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

#[cfg(test)]
mod tests {
    use super::{WorkspaceError, WorkspaceRoot};
    use std::fs;

    /// 相对路径在根内时 `resolve` 成功；`..` 与绝对路径在读盘前拒绝。
    ///
    /// **作用**：A5-01 在 `engine-api` 层锁住授权根边界，不依赖工具包装。
    #[test]
    fn resolve_rejects_parent_and_absolute_before_io() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("ok.txt"), "in").unwrap();
        let root = WorkspaceRoot::open(directory.path()).unwrap();
        assert!(root.resolve("ok.txt").is_ok());
        assert_eq!(root.resolve("../ok.txt"), Err(WorkspaceError::OutsideRoot));
        let absolute = directory.path().join("ok.txt");
        assert_eq!(
            root.resolve(absolute.to_str().unwrap()),
            Err(WorkspaceError::OutsideRoot)
        );
    }

    /// 空字节与换行是 `InvalidPath`，错误文本不含路径。
    #[test]
    fn resolve_rejects_control_bytes_as_invalid_path() {
        let directory = tempfile::tempdir().unwrap();
        let root = WorkspaceRoot::open(directory.path()).unwrap();
        assert_eq!(root.resolve("a\0b"), Err(WorkspaceError::InvalidPath));
        assert_eq!(root.resolve("a\nb"), Err(WorkspaceError::InvalidPath));
        assert!(!WorkspaceError::InvalidPath
            .to_string()
            .contains(directory.path().to_str().unwrap_or("")));
    }
}
