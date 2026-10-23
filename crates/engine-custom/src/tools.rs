use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder, Sink, SinkMatch};
use ignore::WalkBuilder;

use fluxaxiom_engine_api::{WorkspaceError, WorkspaceRoot};

/// `list_files` 单页条目数。暂定值，执行方案只要求有上限。
pub const LIST_PAGE_LIMIT: usize = 100;

/// `read_file` 单次最多行数。暂定值，执行方案只要求有上限。
pub const READ_LINE_LIMIT: usize = 2000;

/// `read_file` 单次纳入正文的最大字节数。暂定值。这一刀没有 token 计数。
pub const READ_BYTE_LIMIT: usize = 32 * 1024;

/// `search_code` 单次最多匹配条数。暂定值，与目录页相同。
pub const SEARCH_MATCH_LIMIT: usize = 100;

/// 单条搜索命中在文本里最多保留的字符数。超出的部分用 `…` 省略，不因此单独把整页标成截断。
const SEARCH_LINE_CHARS: usize = 200;

/// 一次只读工具的结果。
///
/// **作用**：Host 以后写 `tool.call_completed` 时直接取路径、截断标记和渲染后的字节数。交给模型的是 `render` 的文本。
/// **边界**：`body` 不进 `Debug`。`byte_count` 是渲染文本的字节数，不是截断前的文件大小。
#[derive(Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// 工具名。空字符串不会出现，三个工具各写自己的名字。
    pub tool_name: &'static str,
    /// 规范化相对路径，或搜索查询。使用 `/`。
    pub normalized_path_or_query: String,
    /// 为真时，还有没交给模型的条目、行或匹配。
    pub truncated: bool,
    /// 当前页、起始行或偏移。从 1 或 0 开始，见各工具。
    pub cursor: u32,
    /// 续读参数。截断时才有。`list_files` 是下一页，`read_file` 是下一行，`search_code` 是下一偏移。
    pub continuation: Option<u32>,
    body: String,
}

impl std::fmt::Debug for ToolOutput {
    /// **作用**：日志里能看出哪个工具、是否截断、正文多长。
    /// **参数**：`formatter` 接收调试文本。
    /// **边界**：不写正文，也不写查询里可能带出的源码。查询长度代替原文。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolOutput")
            .field("tool_name", &self.tool_name)
            .field("query_or_path_len", &self.normalized_path_or_query.len())
            .field("truncated", &self.truncated)
            .field("cursor", &self.cursor)
            .field("body_len", &self.body.len())
            .finish()
    }
}

impl ToolOutput {
    /// 渲染成交给模型的纯文本。
    ///
    /// **作用**：`Message::Tool.content` 使用这个字符串。字段顺序固定，便于快照测试。
    /// **边界**：头部是工具名、路径或查询、当前游标，然后空行，然后正文，最后一行是截断和续读。不输出 JSON。
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = String::new();
        text.push_str("tool: ");
        text.push_str(self.tool_name);
        text.push('\n');
        if self.tool_name == "search_code" {
            text.push_str("query: ");
        } else {
            text.push_str("path: ");
        }
        text.push_str(&self.normalized_path_or_query);
        text.push('\n');
        let cursor_label = match self.tool_name {
            "list_files" => "page",
            "read_file" => "start_line",
            _ => "offset",
        };
        text.push_str(cursor_label);
        text.push_str(": ");
        text.push_str(&self.cursor.to_string());
        text.push_str("\n\n");
        text.push_str(&self.body);
        if !self.body.is_empty() && !self.body.ends_with('\n') {
            text.push('\n');
        }
        match self.continuation {
            Some(next) => {
                let next_label = match self.tool_name {
                    "list_files" => "next_page",
                    "read_file" => "next_start_line",
                    _ => "next_offset",
                };
                text.push_str("truncated: true; ");
                text.push_str(next_label);
                text.push_str(": ");
                text.push_str(&next.to_string());
                text.push('\n');
            }
            None => text.push_str("truncated: false\n"),
        }
        text
    }

    /// 返回给模型的字节数。
    ///
    /// **作用**：以后写入 `tool.call_completed` 的 `byte_count`。
    /// **边界**：等于 `render` 的字节长度。截断前的文件大小不在这里。
    #[must_use]
    pub fn byte_count(&self) -> u64 {
        self.render().len() as u64
    }
}

/// 只读工具失败。
///
/// **作用**：把越界、忽略、二进制和查询错误分开。
/// **边界**：不携带文件正文。路径细节不进 `Display`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    /// 授权根打不开。
    Workspace(WorkspaceError),
    /// 路径不存在。
    NotFound,
    /// 调用要求目录，目标不是目录。
    NotADirectory,
    /// 调用要求文件，目标不是文件。
    NotAFile,
    /// 遍历规则不会交出这个路径。包括隐藏文件、`.gitignore`、`.ignore`、被忽略的父目录，以及 `.git`。
    Ignored,
    /// 文件含空字节，不交给模型。
    Binary,
    /// 文件不是 UTF-8。
    NotUtf8,
    /// 页码或起始行不是正数，或查询为空、含换行、不是合法正则。
    InvalidRequest,
    /// 读目录或文件时操作系统返回错误。
    Io,
}

impl std::fmt::Display for ToolError {
    /// **作用**：给调用方一句类别说明。
    /// **参数**：`formatter` 接收显示文本。
    /// **边界**：不写路径和文件正文。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Workspace(error) => return error.fmt(formatter),
            Self::NotFound => "path was not found",
            Self::NotADirectory => "path is not a directory",
            Self::NotAFile => "path is not a file",
            Self::Ignored => "path is ignored",
            Self::Binary => "file is binary",
            Self::NotUtf8 => "file is not utf-8",
            Self::InvalidRequest => "tool request is invalid",
            Self::Io => "tool io failed",
        })
    }
}

impl std::error::Error for ToolError {}

impl From<WorkspaceError> for ToolError {
    /// **作用**：解析授权路径失败时直接变成工具错误。
    /// **参数**：`error` 是 `WorkspaceRoot` 的失败。
    /// **边界**：不附加路径。
    fn from(error: WorkspaceError) -> Self {
        Self::Workspace(error)
    }
}

/// 列出授权根内一个目录的一页条目。
///
/// **作用**：模型用来翻目录。条目按名称排序。目录名以 `/` 结尾。
/// **参数**：`relative_dir` 是相对授权根的目录，空字符串表示根。`page` 从 1 开始。
/// **边界**：单页最多 `LIST_PAGE_LIMIT` 条。超出时 `truncated` 为真，`continuation` 是下一页。不递归，不调用 shell。隐藏文件、忽略规则和 `.git` 与 `read_file` 使用同一套遍历配置。
pub fn list_files(
    root: &WorkspaceRoot,
    relative_dir: &str,
    page: u32,
) -> Result<ToolOutput, ToolError> {
    if page == 0 {
        return Err(ToolError::InvalidRequest);
    }
    let directory = root.resolve(relative_dir)?;
    if !directory.is_dir() {
        return Err(ToolError::NotADirectory);
    }
    ensure_visible(root, &directory)?;
    let mut names = Vec::new();
    let mut walker = walk_from(&directory);
    walker.max_depth(Some(1));
    for entry in walker.build() {
        let entry = entry.map_err(|_| ToolError::Io)?;
        if entry.depth() == 0 {
            continue;
        }
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        if name == ".git" {
            continue;
        }
        let mut shown = name.to_owned();
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            shown.push('/');
        }
        names.push(shown);
    }
    let start = (page as usize - 1).saturating_mul(LIST_PAGE_LIMIT);
    let page_names: Vec<_> = names
        .iter()
        .skip(start)
        .take(LIST_PAGE_LIMIT)
        .cloned()
        .collect();
    let truncated = names.len() > start + page_names.len();
    let relative = root.relative(&directory)?;
    Ok(ToolOutput {
        tool_name: "list_files",
        normalized_path_or_query: relative,
        truncated,
        cursor: page,
        continuation: truncated.then_some(page + 1),
        body: page_names.join("\n"),
    })
}

/// 按行读取授权根内的一个文本文件。
///
/// **作用**：模型用来看文件正文。行号从 `start_line` 起，写在每行前面。
/// **参数**：`relative_file` 是相对授权根的文件。`start_line` 从 1 开始。
/// **边界**：最多 `READ_LINE_LIMIT` 行，且纳入的文件字节不超过 `READ_BYTE_LIMIT`。单行超过上限时截断该行并标 `…`，续读从下一行开始，不会停在同一行。
/// 被截断的行和普通行一样占额度：前面已经有内容、剩余额度放不下它时，这一行整个留给下一次调用，作为那次的首行再被截断。被截断行被丢掉的尾部不会再出现。遍历看不到的路径、以及 `.git` 下的路径，返回 `Ignored`。文件任意位置有空字节则整次拒绝。
///
/// **大文件的成本（已知限制，P0a 接受，见执行方案 §4.7.4）**：
/// - 每次调用都从文件开头顺序读。`start_line` 之前的行要读过去才能数行号；窗口结束后还要把剩余部分读完，只为确认没有空字节。所以一次调用的 I/O 与**整个文件大小**成正比，不是与返回的 32 KiB 成正比。
/// - 模型翻页读一个 N 字节的文件，要读 `N × 页数` 字节。没有缓存，没有按字节偏移的游标，也没有文件大小上限，几百 MB 的日志文件会让每一页都很慢。
/// - 内存是有界的：单行最多留 `READ_BYTE_LIMIT` 字节，超出的部分边读边丢，不会因为没有换行的大文件而撑爆内存。有界的只有内存，时间不是。
/// - 之所以仍读完剩余部分：这是为了对二进制文件整次拒绝，使同一个文件在任意 `start_line` 下结论一致。如果以后改成只检查窗口，翻到后面才发现二进制就会出现「前几页能读、后几页被拒」。
/// - 以后要优化时的方向：先在入口按 `metadata().len()` 设上限并返回明确错误；或让续读游标带字节偏移而不是行号，下一页直接 `seek`。两者都会改变 `next_start_line` 的含义，需要同步改渲染和方案。
pub fn read_file(
    root: &WorkspaceRoot,
    relative_file: &str,
    start_line: u32,
) -> Result<ToolOutput, ToolError> {
    if start_line == 0 {
        return Err(ToolError::InvalidRequest);
    }
    let path = root.resolve(relative_file)?;
    if !path.is_file() {
        return Err(ToolError::NotAFile);
    }
    ensure_visible(root, &path)?;
    let file = File::open(&path).map_err(|_| ToolError::Io)?;
    let mut reader = BufReader::new(file);
    let mut line_number = 0_u32;
    let mut body = String::new();
    let mut kept_bytes = 0_usize;
    let mut kept_lines = 0_usize;
    let mut truncated = false;
    let mut next_line = None;
    while let Some(line) = read_capped_line(&mut reader, READ_BYTE_LIMIT)? {
        line_number += 1;
        if line_number < start_line {
            continue;
        }
        let line_len = line.bytes.len();
        let fits = kept_lines < READ_LINE_LIMIT && kept_bytes + line_len <= READ_BYTE_LIMIT;
        // 先判额度，再看是否被截断：被截断的行同样要占额度。放不下就整行留给下一次调用，让它作为首行再被截断，
        // 否则前面已有内容时，这一行会把正文推过 `READ_BYTE_LIMIT` 或 `READ_LINE_LIMIT`。
        // 首行总是放得下（被截断的行最长就是 `READ_BYTE_LIMIT`），所以续读一定前进。
        if !fits {
            truncated = true;
            next_line = Some(line_number);
            break;
        }
        if line.clipped {
            let mut bytes = line.bytes;
            trim_utf8_boundary(&mut bytes);
            let text = std::str::from_utf8(&bytes).map_err(|_| ToolError::NotUtf8)?;
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&line_number.to_string());
            body.push('|');
            body.push_str(text);
            body.push('…');
            truncated = true;
            next_line = Some(line_number + 1);
            break;
        }
        let text = std::str::from_utf8(&line.bytes).map_err(|_| ToolError::NotUtf8)?;
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&line_number.to_string());
        body.push('|');
        body.push_str(text);
        kept_bytes += line_len;
        kept_lines += 1;
    }
    if rest_has_nul(&mut reader)? {
        return Err(ToolError::Binary);
    }
    let relative = root.relative(&path)?;
    Ok(ToolOutput {
        tool_name: "read_file",
        normalized_path_or_query: relative,
        truncated,
        cursor: start_line,
        continuation: next_line,
        body,
    })
}

/// 在授权根内搜索正则。
///
/// **作用**：模型用来定位文本。命中格式是 `相对路径:行号:行文本`。
/// **参数**：`query` 是正则，不能为空或含换行。`offset` 是要跳过的命中条数，从 0 开始。
/// **边界**：最多返回 `SEARCH_MATCH_LIMIT` 条。与 `read_file` 使用同一套遍历配置，因此隐藏文件、忽略规则和 `.git` 都不会被搜到。不调用 shell。单行超过 `SEARCH_LINE_CHARS` 个字符时截显示，不因此把整页标成截断。
pub fn search_code(
    root: &WorkspaceRoot,
    query: &str,
    offset: u32,
) -> Result<ToolOutput, ToolError> {
    if query.is_empty() || query.contains('\n') || query.contains('\r') || query.contains('\0') {
        return Err(ToolError::InvalidRequest);
    }
    let matcher = RegexMatcherBuilder::new()
        .build(query)
        .map_err(|_| ToolError::InvalidRequest)?;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\0'))
        .line_number(true)
        .build();
    let mut collector = MatchCollector {
        root,
        current: PathBuf::new(),
        skip: offset as usize,
        matches: Vec::new(),
        truncated: false,
    };
    let walker = walk_from(root.as_path());
    for entry in walker.build() {
        let entry = entry.map_err(|_| ToolError::Io)?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if entry.file_name() == ".git" || path_has_git_component(entry.path()) {
            continue;
        }
        collector.current = entry.path().to_path_buf();
        searcher
            .search_path(&matcher, entry.path(), &mut collector)
            .map_err(|_| ToolError::Io)?;
        if collector.truncated {
            break;
        }
    }
    let next_offset = collector
        .truncated
        .then_some(offset + SEARCH_MATCH_LIMIT as u32);
    Ok(ToolOutput {
        tool_name: "search_code",
        normalized_path_or_query: query.to_owned(),
        truncated: collector.truncated,
        cursor: offset,
        continuation: next_offset,
        body: collector.matches.join("\n"),
    })
}

/// 三个工具共用的目录遍历。
///
/// **作用**：`list_files`、`search_code` 和可见性判断使用同一组过滤。
/// **参数**：`path` 是本次遍历的起点，必须已经落在授权根内。
/// **边界**：不读授权根上层的忽略文件，也不读用户的全局 gitignore。没有 `.git` 目录时仍然读取 `.gitignore`。不跟随符号链接。
fn walk_from(path: &Path) -> WalkBuilder {
    let mut walker = WalkBuilder::new(path);
    walker
        .standard_filters(true)
        .require_git(false)
        .parents(false)
        .git_global(false)
        .follow_links(false)
        .sort_by_file_name(|left, right| left.cmp(right));
    walker
}

/// 路径的每一层是否都会被同一套遍历交出来。
///
/// **作用**：`read_file` 用它拒绝列表和搜索看不到的文件。
/// **参数**：`path` 必须已经由 `resolve` 收进授权根。
/// **边界**：任一组分是 `.git`，或父目录的遍历列不出这一层，就返回 `Ignored`。不另写一套 gitignore 匹配。
fn ensure_visible(root: &WorkspaceRoot, path: &Path) -> Result<(), ToolError> {
    let relative = root.relative(path)?;
    if relative == "." {
        return Ok(());
    }
    let mut directory = root.as_path().to_path_buf();
    for part in relative.split('/') {
        if part == ".git" {
            return Err(ToolError::Ignored);
        }
        if !parent_lists_child(&directory, OsStr::new(part))? {
            return Err(ToolError::Ignored);
        }
        directory.push(part);
    }
    Ok(())
}

/// 父目录的一页遍历里是否出现这个名字。
///
/// **参数**：`directory` 是已经可见的目录。`child` 是下一层的文件名。
/// **边界**：只看深度 1。遍历规则与 `list_files` 相同，因此隐藏文件和被忽略目录都不会出现。
fn parent_lists_child(directory: &Path, child: &OsStr) -> Result<bool, ToolError> {
    let mut walker = walk_from(directory);
    walker.max_depth(Some(1));
    for entry in walker.build() {
        let entry = entry.map_err(|_| ToolError::Io)?;
        if entry.depth() == 1 && entry.file_name() == child {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 路径里是否有名为 `.git` 的一层。
///
/// **参数**：`path` 是遍历交出的路径。
/// **边界**：只看文件名，不访问磁盘。
fn path_has_git_component(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == ".git")
}

struct CappedLine {
    bytes: Vec<u8>,
    clipped: bool,
}

/// 读一行，最多保留 `cap` 个字节。
///
/// **作用**：避免没有换行的大文件把整行读进内存。
/// **参数**：`cap` 是这一行可以留下的最大字节数。
/// **边界**：遇到空字节返回 `Binary`，不返回这一行。超过 `cap` 时丢掉该行剩余部分并标 `clipped`。`\r` 不进入结果。文件结束且没有字节时返回 `None`。
fn read_capped_line(
    reader: &mut impl BufRead,
    cap: usize,
) -> Result<Option<CappedLine>, ToolError> {
    let mut bytes = Vec::new();
    let mut clipped = false;
    loop {
        let available = reader.fill_buf().map_err(|_| ToolError::Io)?;
        if available.is_empty() {
            if bytes.is_empty() && !clipped {
                return Ok(None);
            }
            return Ok(Some(CappedLine { bytes, clipped }));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let nul = available.iter().position(|byte| *byte == 0);
        if nul.is_some_and(|index| newline.is_none_or(|end| index < end)) {
            return Err(ToolError::Binary);
        }
        if clipped {
            if let Some(index) = newline {
                reader.consume(index + 1);
                return Ok(Some(CappedLine {
                    bytes,
                    clipped: true,
                }));
            }
            let length = available.len();
            reader.consume(length);
            continue;
        }
        let take_until = newline.unwrap_or(available.len());
        let piece = &available[..take_until];
        let room = cap.saturating_sub(bytes.len());
        if piece.len() > room {
            bytes.extend(
                piece
                    .iter()
                    .take(room)
                    .copied()
                    .filter(|byte| *byte != b'\r'),
            );
            reader.consume(take_until);
            clipped = true;
            if newline.is_some() {
                reader.consume(1);
                return Ok(Some(CappedLine {
                    bytes,
                    clipped: true,
                }));
            }
            continue;
        }
        bytes.extend(piece.iter().copied().filter(|byte| *byte != b'\r'));
        reader.consume(take_until);
        if newline.is_some() {
            reader.consume(1);
            return Ok(Some(CappedLine {
                bytes,
                clipped: false,
            }));
        }
    }
}

/// 丢掉已经结束的行之后，文件剩余部分是否含空字节。
///
/// **参数**：`reader` 停在还没交给模型的字节上。
/// **边界**：发现空字节就返回真。不把这些字节留在内存里。
/// **成本**：会把文件剩余部分整个读一遍，时间与剩余字节数成正比。这是 `read_file` 大文件变慢的主要来源之一，原因和替代方案写在 `read_file` 的注释里。不要在没有看那段说明的情况下把它改成只扫窗口。
fn rest_has_nul(reader: &mut impl BufRead) -> Result<bool, ToolError> {
    loop {
        let available = reader.fill_buf().map_err(|_| ToolError::Io)?;
        if available.is_empty() {
            return Ok(false);
        }
        if available.contains(&0) {
            return Ok(true);
        }
        let length = available.len();
        reader.consume(length);
    }
}

/// 去掉末尾不完整的 UTF-8 字节。
///
/// **参数**：`bytes` 可能在字符中间被截断。
/// **边界**：至少留一个字节时才继续删。全删光也合法，调用方会得到空文本。
fn trim_utf8_boundary(bytes: &mut Vec<u8>) {
    while !bytes.is_empty() && std::str::from_utf8(bytes).is_err() {
        bytes.pop();
    }
}

struct MatchCollector<'a> {
    root: &'a WorkspaceRoot,
    current: PathBuf,
    skip: usize,
    matches: Vec<String>,
    truncated: bool,
}

impl Sink for MatchCollector<'_> {
    /// **作用**：搜索器把读文件失败也收成同一种错误。
    /// **边界**：路径出了授权根或行不是 UTF-8 时停止，调用方记成 `Io`。不把该行正文放进错误。
    type Error = std::io::Error;

    /// **作用**：收下一条命中，直到凑满一页再多看一条以判断是否截断。
    /// **参数**：`matched` 是当前文件里的一行命中。路径来自搜索开始前记下的 `current`。
    /// **边界**：跳过 `skip` 条。返回 `false` 时停止当前文件。单行只保留前 `SEARCH_LINE_CHARS` 个字符。
    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self.skip > 0 {
            self.skip -= 1;
            return Ok(true);
        }
        if self.matches.len() == SEARCH_MATCH_LIMIT {
            self.truncated = true;
            return Ok(false);
        }
        let Some(line_number) = matched.line_number() else {
            return Ok(true);
        };
        let relative = self
            .root
            .relative(&self.current)
            .map_err(|_| std::io::Error::other("path left the workspace"))?;
        let text = std::str::from_utf8(matched.bytes())
            .map_err(|_| std::io::Error::other("match is not utf-8"))?;
        let text = text.trim_end_matches(['\n', '\r']);
        let shown: String = text.chars().take(SEARCH_LINE_CHARS).collect();
        let shown = if shown.chars().count() < text.chars().count() {
            format!("{shown}…")
        } else {
            shown
        };
        self.matches
            .push(format!("{relative}:{line_number}:{shown}"));
        Ok(true)
    }
}
