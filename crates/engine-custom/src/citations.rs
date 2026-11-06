//! 从报告正文抽出相对路径，并对照授权根里工具能看见的文件。
//!
//! **作用**：EA-04 的路径证据。调用方把报告正文和 [`WorkspaceRoot`] 交给 [`path_citations`]。
//! **边界**：不读模型，不打开 Host，不判断事实是否答对。`trace --verify` 的 `consistent` 不在这里计算。

use std::path::Path;

use fluxaxiom_engine_api::WorkspaceRoot;

use crate::tools::walk_from;

/// 报告里的一条路径引用。
///
/// **作用**：[`path_citations`] 的一项。测试和调用方用 `found` 区分真实文件和编造路径。
/// **边界**：不含报告正文。`path` 不带行号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathCitation {
    /// 规范化后的相对路径，使用 `/`。带目录时含 `/`；只有文件名时没有 `/`。不为空。
    pub path: String,
    /// 只读工具的遍历能否看见这个文件。`false` 表示没有这个可见文件，不是“内容错误”。
    pub found: bool,
}

/// 抽出报告里的路径，并标出哪些是可见文件。
///
/// **作用**：评测一次报告有没有引用工作区里不存在的文件。
/// **参数**：`root` 是这次任务的授权根，只用来列可见文件。`report` 是报告正文，空串没有引用。
/// **边界**：同一路径只保留第一次出现。反引号里的文件名（如 `` `Cargo.toml` ``、`` `inventory.rs:26` ``）算引用；
/// 正文里带 `/` 的相对路径也算。`item.quantity`、`fs::write` 和 URL 不算。隐藏文件、被忽略的文件和 `.git` 下的文件算没找到。
/// 不跟随符号链接。不把目录当成命中。
#[must_use]
pub fn path_citations(root: &WorkspaceRoot, report: &str) -> Vec<PathCitation> {
    let files = visible_files(root);
    cited_paths(report)
        .into_iter()
        .map(|path| {
            let found = files.iter().any(|file| file_matches(file, &path));
            PathCitation { path, found }
        })
        .collect()
}

/// 返回报告里引用了、但可见文件中没有的路径。
///
/// **作用**：调用方用空结果表示路径证据通过。
/// **参数**：`root` 与 `report` 同 [`path_citations`]。
/// **边界**：顺序与 [`path_citations`] 中未命中的项一致。不返回命中的路径。
#[must_use]
pub fn missing_path_citations(root: &WorkspaceRoot, report: &str) -> Vec<String> {
    path_citations(root, report)
        .into_iter()
        .filter(|citation| !citation.found)
        .map(|citation| citation.path)
        .collect()
}

/// 带目录的路径必须整段相同；只有文件名时，任一可见文件的最后一段相同即可。
///
/// **参数**：`visible` 是遍历得到的相对路径，使用 `/`。`cited` 是规范化后的引用。
/// **边界**：不把目录前缀的部分重合当成命中。
fn file_matches(visible: &str, cited: &str) -> bool {
    if cited.contains('/') {
        visible == cited
    } else {
        visible.rsplit('/').next() == Some(cited)
    }
}

/// 按报告出现顺序收集路径，去掉重复。
///
/// **参数**：`report` 是完整正文。空串得到空列表。
/// **边界**：不访问磁盘。代码围栏的 ``` 不当成引用。
fn cited_paths(report: &str) -> Vec<String> {
    let chars: Vec<char> = report.chars().collect();
    let mut paths = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '`' {
            if chars.get(index + 1) == Some(&'`') && chars.get(index + 2) == Some(&'`') {
                index += 3;
                continue;
            }
            index += 1;
            let start = index;
            while index < chars.len() && chars[index] != '`' {
                index += 1;
            }
            let span: String = chars[start..index].iter().collect();
            if index < chars.len() {
                index += 1;
            }
            if let Some(path) = normalize_span(&span) {
                push_unique(&mut paths, path);
            }
            continue;
        }
        if is_path_char(chars[index]) {
            let start = index;
            while index < chars.len() && (is_path_char(chars[index]) || chars[index] == ':') {
                index += 1;
            }
            let token: String = chars[start..index].iter().collect();
            if token.contains('/') || token.contains('\\') {
                if let Some(path) = normalize_slashed(&token) {
                    push_unique(&mut paths, path);
                }
            }
            continue;
        }
        index += 1;
    }
    paths
}

/// 反引号里的一段：带斜杠的相对路径，或单个带扩展名的文件名。
///
/// **参数**：`span` 是一对反引号中间的原文，可带 `:行号`。
/// **边界**：含空白、`::`、括号或 URL 的片段返回 `None`。
fn normalize_span(span: &str) -> Option<String> {
    if span.contains('/') || span.contains('\\') {
        normalize_slashed(span)
    } else {
        normalize_filename(span)
    }
}

/// 正文里的相对路径。必须含 `/` 或 `\`。
///
/// **参数**：`token` 是连续的路径字符，可在末尾带 `:行号` 或 `:行号:列号`。
/// **边界**：绝对路径、`..` 和没有扩展名的段落返回 `None`。
fn normalize_slashed(token: &str) -> Option<String> {
    let path = clean_path(token)?;
    if path.contains('/') {
        Some(path)
    } else {
        None
    }
}

/// 反引号里的单个文件名。
///
/// **参数**：`token` 是反引号原文，可带 `:行号`。
/// **边界**：含斜杠时返回 `None`，交给 [`normalize_slashed`]。
fn normalize_filename(token: &str) -> Option<String> {
    let path = clean_path(token)?;
    if path.contains('/') {
        None
    } else {
        Some(path)
    }
}

/// 去掉行号和多余标点，并检查每一段都是相对路径分量。
///
/// **参数**：`raw` 是候选原文。
/// **边界**：留下的 `:`、`..`、空段或没有扩展名的普通名字都返回 `None`。点文件名可以留下。结果使用 `/`。
fn clean_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains("://") || trimmed.contains([' ', '\n', '\t']) {
        return None;
    }
    let without_location = strip_location(trimmed);
    let without_dot = strip_sentence_dot(without_location);
    if without_dot.contains(':') {
        return None;
    }
    let path = without_dot.replace('\\', "/");
    let path = path.strip_prefix("./").unwrap_or(&path);
    if path.starts_with('/') || path.is_empty() {
        return None;
    }
    let mut last = "";
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        if !part
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        {
            return None;
        }
        last = part;
    }
    if !valid_filename(last) {
        return None;
    }
    Some(path.to_owned())
}

/// 最后一段必须是带扩展名的文件名，或以 `.` 开头的点文件名。
///
/// **参数**：`last` 是路径的最后一段，不含 `/`。
/// **边界**：空段、单独的 `.` 和没有扩展名的普通名字返回 `false`。
fn valid_filename(last: &str) -> bool {
    if let Some(rest) = last.strip_prefix('.') {
        return !rest.is_empty()
            && rest
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'));
    }
    let Some((name, extension)) = last.rsplit_once('.') else {
        return false;
    };
    !name.is_empty() && !extension.is_empty()
}

/// 去掉末尾的 `:数字`，最多两段，覆盖行号和列号。
///
/// **参数**：`token` 还没替换斜杠。
/// **边界**：冒号后面不是纯数字时保留原文。
fn strip_location(token: &str) -> &str {
    let mut text = token;
    for _ in 0..2 {
        let Some((head, tail)) = text.rsplit_once(':') else {
            break;
        };
        if tail.is_empty() || !tail.bytes().all(|byte| byte.is_ascii_digit()) {
            break;
        }
        text = head;
    }
    text
}

/// 去掉句子结尾多出来的一个英文句点。
///
/// **参数**：`token` 可能是 `src/main.rs.`。
/// **边界**：扩展名本身的点留下。只有一个点且在末尾时不删，后面的扩展名检查会拒绝它。
fn strip_sentence_dot(token: &str) -> &str {
    if token.ends_with('.') && token.matches('.').count() >= 2 {
        &token[..token.len() - 1]
    } else {
        token
    }
}

/// 路径字符：字母、数字、`_`、`-`、`.`、`/` 和 `\`。
///
/// **参数**：`ch` 是正文里的一个字符。
/// **边界**：冒号不算，行号由调用方另行吃进 token。
fn is_path_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | '\\')
}

/// 按首次出现保留。
///
/// **参数**：`paths` 是已经收下的路径。`path` 是新候选。
/// **边界**：相同字符串不追加，不改变已有顺序。
fn push_unique(paths: &mut Vec<String>, path: String) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

/// 授权根里只读工具能看见的文件，相对路径使用 `/`。
///
/// **参数**：`root` 已经打开。
/// **边界**：跳过遍历错误、目录，以及路径分量是 `.git` 的条目。不读文件正文。
fn visible_files(root: &WorkspaceRoot) -> Vec<String> {
    let mut files = Vec::new();
    for entry in walk_from(root.as_path()).build() {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if entry.file_name() == ".git" || has_git_component(entry.path()) {
            continue;
        }
        let Ok(relative) = root.relative(entry.path()) else {
            continue;
        };
        if relative.split('/').any(|part| part == ".git") {
            continue;
        }
        files.push(relative);
    }
    files
}

/// 路径分量里是否有 `.git`。
///
/// **参数**：`path` 是遍历得到的绝对或相对路径。
/// **边界**：只看分量名，不访问磁盘。
fn has_git_component(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == ".git")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// 建一个临时授权根。
    ///
    /// **参数**：`files` 是要写入的相对路径和正文。路径使用 `/`。
    /// **边界**：目录由调用方在测试结束时保留到 `TempDir` 丢弃。空列表得到空根。
    fn workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, WorkspaceRoot) {
        let directory = tempfile::tempdir().unwrap();
        for (path, body) in files {
            let full = directory.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(full, body).unwrap();
        }
        let root = WorkspaceRoot::open(directory.path()).unwrap();
        (directory, root)
    }

    /// 反引号文件名和带斜杠的路径会被抽出，方法和字段不会。
    ///
    /// **边界**：URL、`::` 和 `item.quantity` 不进入结果。行号被去掉。
    #[test]
    fn citations_keep_paths_and_skip_code_words() {
        let (_directory, root) = workspace(&[("src/main.rs", "fn main() {}\n")]);
        let report = "\
见 `Cargo.toml` 和 src/main.rs。
`inventory.rs:26` 的 item.quantity 不是文件。
`fs::write` 与 https://example.com/a.rs 也不是。
";
        let cited: Vec<String> = path_citations(&root, report)
            .into_iter()
            .map(|citation| citation.path)
            .collect();
        assert_eq!(cited, vec!["Cargo.toml", "src/main.rs", "inventory.rs"]);
    }

    /// 工作区里有的文件命中，没有的文件进入缺失列表。
    ///
    /// **边界**：同名文件在任一可见目录里都算文件名命中。不读真实模型。
    #[test]
    fn missing_citations_are_paths_the_tools_cannot_see() {
        let (_directory, root) = workspace(&[
            ("src/main.rs", "fn main() {}\n"),
            ("src/inventory.rs", "pub fn remove() {}\n"),
            ("Cargo.toml", "[package]\nname = \"mini\"\n"),
            (".gitignore", "secret.rs\n"),
            ("secret.rs", "hidden\n"),
        ]);
        let report = "\
读了 `src/main.rs`、`Cargo.toml` 和 `inventory.rs:42`。
还引用了 `src/missing.rs`、`nope.rs`、`secret.rs` 与 `src/.hidden.rs`。
";
        let citations = path_citations(&root, report);
        assert!(citations
            .iter()
            .any(|item| item.path == "src/main.rs" && item.found));
        assert!(citations
            .iter()
            .any(|item| item.path == "Cargo.toml" && item.found));
        assert!(citations
            .iter()
            .any(|item| item.path == "inventory.rs" && item.found));
        assert_eq!(
            missing_path_citations(&root, report),
            vec![
                "src/missing.rs".to_owned(),
                "nope.rs".to_owned(),
                "secret.rs".to_owned(),
                "src/.hidden.rs".to_owned(),
            ]
        );
    }
}
