//! Host 数据目录解析。
//!
//! **作用**：把 CLI 的 `--data-dir` / 环境变量 / 平台默认收成路径，供 `RunHost::open`。
//! **边界**：不创建目录；`RunHost::open` 负责建库与 `blobs/`。与 `--workspace` 无关。
//! 环境变量只在这里读一次（clap 不再带 `env =`），空串按未设置处理。

use std::env;
use std::path::PathBuf;

/// 环境变量名。设置后覆盖平台默认，仍可被 `--data-dir` 覆盖。
pub const DATA_DIR_ENV: &str = "FLUXAXIOM_DATA_DIR";

/// 解析本次命令使用的数据目录。
///
/// **参数**：`override_dir` 来自 `--data-dir`；`None` 时读环境变量，再退回平台默认。
/// **边界**：返回的路径可能尚不存在。相对路径按进程当前目录理解。
pub fn resolve(override_dir: Option<PathBuf>) -> PathBuf {
    resolve_with(override_dir, |name| env::var(name).ok())
}

/// 同 [`resolve`]，环境变量查找由调用方给，测试不必改进程环境。
///
/// **作用**：把「显式覆盖 → 环境变量 → 平台默认」收成一条路径。
/// **参数**：`lookup` 模拟 `std::env::var`；返回 `None` 表示未设置。
/// **边界**：空串经 [`non_empty`] 视为未设置。
fn resolve_with(override_dir: Option<PathBuf>, lookup: impl Fn(&str) -> Option<String>) -> PathBuf {
    if let Some(path) = override_dir {
        return path;
    }
    if let Some(path) = non_empty(&lookup, DATA_DIR_ENV) {
        return PathBuf::from(path);
    }
    default_data_dir(&lookup)
}

/// 取非空的环境变量值。
///
/// **作用**：统一「未设置」与「设成空串」两种缺省。
/// **边界**：只过滤空串，不 trim 空白。
fn non_empty(lookup: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    lookup(name).filter(|value| !value.is_empty())
}

/// 平台默认数据目录（见 ADR-021）。
///
/// **边界**：Windows → `%LOCALAPPDATA%\FluxAxiom`；其它 → `$XDG_DATA_HOME/fluxaxiom` 或 `~/.local/share/fluxaxiom`。
/// 都拿不到时退回当前目录下的 `.fluxaxiom`，保证 CLI 仍能启动。
fn default_data_dir(lookup: &impl Fn(&str) -> Option<String>) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(base) = non_empty(lookup, "LOCALAPPDATA") {
            return PathBuf::from(base).join("FluxAxiom");
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(base) = non_empty(lookup, "XDG_DATA_HOME") {
            return PathBuf::from(base).join("fluxaxiom");
        }
        if let Some(home) = non_empty(lookup, "HOME") {
            return PathBuf::from(home).join(".local/share/fluxaxiom");
        }
    }
    PathBuf::from(".fluxaxiom")
}

#[cfg(test)]
mod tests {
    use super::{resolve_with, DATA_DIR_ENV};
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// 用固定表代替进程环境。
    ///
    /// **作用**：构造传给 [`super::resolve_with`] 的 `lookup`。
    /// **参数**：`pairs` 是 `(变量名, 值)`；同名后者覆盖前者。
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let table: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |name| table.get(name).cloned()
    }

    /// `--data-dir` 优先于 `FLUXAXIOM_DATA_DIR`。
    #[test]
    fn override_beats_environment() {
        let lookup = env_of(&[(DATA_DIR_ENV, "from-env")]);
        let path = resolve_with(Some(PathBuf::from("explicit-data")), lookup);
        assert_eq!(path, PathBuf::from("explicit-data"));
    }

    /// 有环境变量时不走平台默认。
    #[test]
    fn environment_beats_platform_default() {
        let lookup = env_of(&[(DATA_DIR_ENV, "from-env")]);
        assert_eq!(resolve_with(None, lookup), PathBuf::from("from-env"));
    }

    /// 空串与未设置等价。
    #[test]
    fn empty_environment_value_counts_as_unset() {
        let with_empty = resolve_with(None, env_of(&[(DATA_DIR_ENV, "")]));
        let unset = resolve_with(None, env_of(&[]));
        assert_eq!(with_empty, unset);
    }

    /// Windows 默认落在 `%LOCALAPPDATA%\FluxAxiom`；拿不到则 `.fluxaxiom`。
    #[cfg(windows)]
    #[test]
    fn windows_default_uses_local_app_data() {
        let base = r"C:\Users\x\AppData\Local";
        let path = resolve_with(None, env_of(&[("LOCALAPPDATA", base)]));
        assert_eq!(path, PathBuf::from(base).join("FluxAxiom"));
        assert_eq!(resolve_with(None, env_of(&[])), PathBuf::from(".fluxaxiom"));
    }

    /// Unix：`XDG_DATA_HOME` 优先于 `HOME`，都没有则 `.fluxaxiom`。
    #[cfg(not(windows))]
    #[test]
    fn unix_default_prefers_xdg_then_home() {
        let xdg = resolve_with(
            None,
            env_of(&[("XDG_DATA_HOME", "/xdg"), ("HOME", "/home/x")]),
        );
        assert_eq!(xdg, PathBuf::from("/xdg/fluxaxiom"));
        let home = resolve_with(None, env_of(&[("HOME", "/home/x")]));
        assert_eq!(home, PathBuf::from("/home/x/.local/share/fluxaxiom"));
        assert_eq!(resolve_with(None, env_of(&[])), PathBuf::from(".fluxaxiom"));
    }
}
