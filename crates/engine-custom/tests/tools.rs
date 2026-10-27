//! 三个只读工具的路径边界、分页和截断。
//!
//! **作用**：证明工具只读授权根，超页或超字节会标 `truncated` 并给出续读。
//! **边界**：不接 Engine Loop，不记账。这里的工具不碰模型输出；流式 `tool_calls` 的解析在 `tests/sse.rs`。样本是测试临时目录，不是真实仓库。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use fluxaxiom_engine_custom::{
    list_files, read_file, search_code, ToolError, WorkspaceError, WorkspaceRoot, LIST_PAGE_LIMIT,
    READ_BYTE_LIMIT, READ_LINE_LIMIT, SEARCH_MATCH_LIMIT,
};

/// 每个测试一个临时目录，避免互相看见文件。
fn fresh_dir() -> PathBuf {
    static TICK: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let tick = TICK.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("fluxaxiom-tools-{nanos}-{tick}"));
    fs::create_dir_all(&path).unwrap();
    path
}

/// 在临时目录里放几个受控文件，并返回授权根。
///
/// **边界**：`secret.txt` 被 `.gitignore` 排除。`outside.txt` 在根外。
fn fixture() -> (WorkspaceRoot, PathBuf, PathBuf) {
    let root_path = fresh_dir();
    let outside = fresh_dir();
    fs::write(outside.join("outside.txt"), "outside").unwrap();
    fs::create_dir(root_path.join("src")).unwrap();
    fs::write(root_path.join("src").join("lib.rs"), "fn host() {}\n").unwrap();
    fs::write(root_path.join(".gitignore"), "secret.txt\n").unwrap();
    fs::write(root_path.join("secret.txt"), "secret-body").unwrap();
    let root = WorkspaceRoot::open(&root_path).unwrap();
    (root, root_path, outside)
}

/// 根内文件可以读到，渲染文本的字节数等于 `byte_count`。
///
/// **边界**：`Debug` 不包含正文。
#[test]
fn read_file_returns_numbered_text() {
    let (root, _, _) = fixture();
    let output = read_file(&root, "src/lib.rs", 1).unwrap();
    let rendered = output.render();
    assert_eq!(
        rendered,
        "\
tool: read_file
path: src/lib.rs
start_line: 1

1|fn host() {}
truncated: false
"
    );
    assert_eq!(output.byte_count(), rendered.len() as u64);
    assert!(!format!("{output:?}").contains("fn host"));
}

/// 绝对路径和 `..` 不能离开授权根。
///
/// **边界**：错误文本不包含根外文件的正文。
#[test]
fn absolute_and_parent_paths_stay_inside_the_root() {
    let (root, _, outside) = fixture();
    let absolute = outside.join("outside.txt");
    let error = read_file(&root, absolute.to_str().unwrap(), 1).unwrap_err();
    assert_eq!(error, ToolError::Workspace(WorkspaceError::OutsideRoot));
    let escaped = read_file(&root, "../outside.txt", 1).unwrap_err();
    assert_eq!(escaped, ToolError::Workspace(WorkspaceError::OutsideRoot));
    let missing = read_file(&root, "../missing-dir/nope.txt", 1).unwrap_err();
    assert_eq!(missing, ToolError::Workspace(WorkspaceError::OutsideRoot));
    assert!(!escaped.to_string().contains("outside.txt"));
    assert!(!missing.to_string().contains("nope.txt"));
}

/// 指向根外的符号链接不能当根内文件读。
///
/// **边界**：默认明确记为 ignored；CI 单独执行，建不了链接时失败，不能算安全通过。
#[test]
#[ignore = "requires symlink capability; CI runs explicitly with --ignored"]
fn symlink_pointing_outside_is_rejected() {
    let (root, root_path, outside) = fixture();
    let link = root_path.join("leak.txt");
    make_file_symlink(outside.join("outside.txt").as_path(), &link);
    let error = read_file(&root, "leak.txt", 1).unwrap_err();
    assert_eq!(error, ToolError::Workspace(WorkspaceError::OutsideRoot));
}

/// `.gitignore` 排除的文件不能读、不能列出、也不能被搜到。
///
/// **边界**：错误和渲染文本都不含 `secret-body`。
#[test]
fn gitignored_files_are_not_returned() {
    let (root, _, _) = fixture();
    let error = read_file(&root, "secret.txt", 1).unwrap_err();
    assert_eq!(error, ToolError::Ignored);
    assert!(!error.to_string().contains("secret-body"));
    let listed = list_files(&root, ".", 1).unwrap().render();
    assert!(!listed.contains("secret.txt"));
    assert!(listed.contains("src"));
    let found = search_code(&root, "secret-body", 0).unwrap().render();
    assert!(found.starts_with("tool: search_code\nquery: secret-body\n"));
    assert!(!found.lines().any(|line| line.contains(":secret-body")));
}

/// 子目录里的 `.gitignore` 同样排除文件。
///
/// **边界**：规则写在 `src/.gitignore`，不是根上。
#[test]
fn nested_gitignore_hides_the_file() {
    let (root, root_path, _) = fixture();
    fs::write(root_path.join("src").join(".gitignore"), "hidden.rs\n").unwrap();
    fs::write(root_path.join("src").join("hidden.rs"), "hidden-body").unwrap();
    let error = read_file(&root, "src/hidden.rs", 1).unwrap_err();
    assert_eq!(error, ToolError::Ignored);
    let listed = list_files(&root, "src", 1).unwrap().render();
    assert!(!listed.contains("hidden.rs"));
    assert!(listed.contains("lib.rs"));
}

/// 目录超过一页时，最后一行给出下一页。
///
/// **边界**：只返回 `LIST_PAGE_LIMIT` 条。不递归进 `src`。
#[test]
fn list_files_pages_and_names_the_next_page() {
    let root_path = fresh_dir();
    for index in 0..=LIST_PAGE_LIMIT {
        fs::write(root_path.join(format!("f{index:03}.txt")), "x").unwrap();
    }
    let root = WorkspaceRoot::open(&root_path).unwrap();
    let output = list_files(&root, ".", 1).unwrap();
    assert!(output.truncated);
    assert_eq!(output.continuation, Some(2));
    let rendered = output.render();
    assert!(rendered.ends_with("truncated: true; next_page: 2\n"));
    let body_lines = rendered
        .lines()
        .filter(|line| line.starts_with('f'))
        .count();
    assert_eq!(body_lines, LIST_PAGE_LIMIT);
    let second = list_files(&root, ".", 2).unwrap();
    assert!(!second.truncated);
    assert!(second.render().contains("f"));
}

/// 正文超过字节上限时不放入半行，并给出下一行。
///
/// **边界**：返回给模型的正文不超过 `READ_BYTE_LIMIT`。
#[test]
fn read_file_stops_on_the_byte_limit() {
    let root_path = fresh_dir();
    let line = "a".repeat(1024);
    let repeats = (READ_BYTE_LIMIT / 1024) + 2;
    let mut file = String::new();
    for _ in 0..repeats {
        file.push_str(&line);
        file.push('\n');
    }
    fs::write(root_path.join("big.txt"), file).unwrap();
    let root = WorkspaceRoot::open(&root_path).unwrap();
    let output = read_file(&root, "big.txt", 1).unwrap();
    assert!(output.truncated);
    assert!(output.continuation.unwrap() > 1);
    assert!(output.render().contains("next_start_line"));
    let body = output.render();
    let kept: usize = body
        .lines()
        .filter_map(|line| line.split_once('|').map(|(_, text)| text.len()))
        .sum();
    assert!(kept <= READ_BYTE_LIMIT);
}

/// 含空字节的文件不返回正文。
///
/// **边界**：错误文本里没有那个空字节周围的内容。
#[test]
fn binary_file_is_refused() {
    let root_path = fresh_dir();
    fs::write(root_path.join("bin.dat"), b"hello\0world").unwrap();
    let root = WorkspaceRoot::open(&root_path).unwrap();
    let error = read_file(&root, "bin.dat", 1).unwrap_err();
    assert_eq!(error, ToolError::Binary);
    assert!(!error.to_string().contains("hello"));
}

/// 命中超过一页时给出下一次偏移。
///
/// **边界**：渲染里最多 `SEARCH_MATCH_LIMIT` 条命中。
#[test]
fn search_code_pages_matches() {
    let root_path = fresh_dir();
    let mut file = String::new();
    for index in 0..=SEARCH_MATCH_LIMIT {
        file.push_str(&format!("marker {index}\n"));
    }
    fs::write(root_path.join("notes.txt"), file).unwrap();
    let root = WorkspaceRoot::open(&root_path).unwrap();
    let output = search_code(&root, "marker", 0).unwrap();
    assert!(output.truncated);
    assert_eq!(output.continuation, Some(SEARCH_MATCH_LIMIT as u32));
    let hits = output
        .render()
        .lines()
        .filter(|line| line.contains(":marker"))
        .count();
    assert_eq!(hits, SEARCH_MATCH_LIMIT);
    assert!(output.render().ends_with(&format!(
        "truncated: true; next_offset: {SEARCH_MATCH_LIMIT}\n"
    )));
}

/// 隐藏文件、`.git`、`.ignore` 和被忽略目录里的文件都不能读。
///
/// **边界**：列表和搜索也看不到它们。错误文本不包含密钥或 git 配置。
#[test]
fn hidden_and_ignored_paths_cannot_be_read() {
    let (root, root_path, _) = fixture();
    fs::write(root_path.join(".env"), "API_KEY=SECRET\n").unwrap();
    fs::create_dir(root_path.join(".git")).unwrap();
    fs::write(root_path.join(".git").join("config"), "[core]\n").unwrap();
    fs::write(root_path.join(".ignore"), "via-ignore.txt\n").unwrap();
    fs::write(root_path.join("via-ignore.txt"), "ignore-body").unwrap();
    fs::write(root_path.join(".gitignore"), "secret.txt\ntarget/\n").unwrap();
    fs::create_dir(root_path.join("target")).unwrap();
    fs::write(root_path.join("target").join("x.txt"), "target-body").unwrap();
    for path in [".env", ".git/config", "via-ignore.txt", "target/x.txt"] {
        let error = read_file(&root, path, 1).unwrap_err();
        assert_eq!(error, ToolError::Ignored, "{path}");
        assert!(!error.to_string().contains("SECRET"));
        assert!(!error.to_string().contains("[core]"));
    }
    let listed = list_files(&root, ".", 1).unwrap().render();
    assert!(!listed.contains(".env"));
    assert!(!listed.contains(".git"));
    assert!(!listed.contains("via-ignore"));
    assert!(!listed.contains("target"));
    let found = search_code(&root, "SECRET", 0).unwrap().render();
    assert!(!found.lines().any(|line| line.contains(":SECRET")));
}

/// 带锚点的嵌套规则只排除该目录下一层。
///
/// **边界**：`/hidden.rs` 不排除更深层的同名文件。
#[test]
fn anchored_nested_gitignore_matches_its_own_directory() {
    let (root, root_path, _) = fixture();
    fs::write(root_path.join("src").join(".gitignore"), "/hidden.rs\n").unwrap();
    fs::write(root_path.join("src").join("hidden.rs"), "anchored-body").unwrap();
    fs::create_dir(root_path.join("src").join("nested")).unwrap();
    fs::write(
        root_path.join("src").join("nested").join("hidden.rs"),
        "kept-body\n",
    )
    .unwrap();
    assert_eq!(
        read_file(&root, "src/hidden.rs", 1).unwrap_err(),
        ToolError::Ignored
    );
    let kept = read_file(&root, "src/nested/hidden.rs", 1)
        .unwrap()
        .render();
    assert!(kept.contains("kept-body"));
    let listed = list_files(&root, "src", 1).unwrap().render();
    assert!(!listed.lines().any(|line| line == "hidden.rs"));
    assert!(listed.contains("nested/"));
}

/// 授权根上层的 `.gitignore` 不影响根内文件。
///
/// **边界**：遍历不读取父目录的忽略规则。
#[test]
fn parent_gitignore_does_not_apply() {
    let parent = fresh_dir();
    fs::write(parent.join(".gitignore"), "*.rs\n").unwrap();
    let child = parent.join("repo");
    fs::create_dir(&child).unwrap();
    fs::write(child.join("lib.rs"), "fn keep() {}\n").unwrap();
    let root = WorkspaceRoot::open(&child).unwrap();
    let rendered = read_file(&root, "lib.rs", 1).unwrap().render();
    assert!(rendered.contains("fn keep() {}"));
}

/// 根内祖先的 glob、带路径规则和忽略目录，对列表、直接读取、搜索同时生效。
///
/// **边界**：分别覆盖 .gitignore / .ignore，且带根锚点的规则不能错误屏蔽更深层同名文件。
/// 合成密钥标记不出现在任何成功结果；测试目录自动回收。
#[test]
fn ancestor_ignore_rules_apply_to_all_tools() {
    for ignore_file in [".gitignore", ".ignore"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        fs::create_dir_all(path.join("src/deep")).unwrap();
        fs::create_dir_all(path.join("src/cache")).unwrap();
        fs::write(
            path.join(ignore_file),
            "*.key\n/src/secret.txt\n/src/cache/\n",
        )
        .unwrap();
        for file in [
            "src/private.key",
            "src/secret.txt",
            "src/deep/private.key",
            "src/cache/data.txt",
        ] {
            fs::write(path.join(file), "SYNTHETIC_SECRET_MARKER\n").unwrap();
        }
        fs::write(path.join("src/deep/secret.txt"), "PUBLIC_MARKER\n").unwrap();
        let root = WorkspaceRoot::open(path).unwrap();
        for file in [
            "src/private.key",
            "src/secret.txt",
            "src/deep/private.key",
            "src/cache/data.txt",
        ] {
            assert_eq!(
                read_file(&root, file, 1).unwrap_err(),
                ToolError::Ignored,
                "{ignore_file}: {file}"
            );
        }
        assert_eq!(
            list_files(&root, "src/cache", 1).unwrap_err(),
            ToolError::Ignored
        );
        let listed = list_files(&root, "src", 1).unwrap().render();
        for hidden in ["private.key", "secret.txt", "cache/"] {
            assert!(
                !listed.lines().any(|line| line == hidden),
                "{ignore_file}: {hidden}"
            );
        }
        let deep = list_files(&root, "src/deep", 1).unwrap().render();
        assert!(!deep.contains("private.key"));
        assert!(deep.lines().any(|line| line == "secret.txt"));
        assert!(read_file(&root, "src/deep/secret.txt", 1)
            .unwrap()
            .render()
            .contains("PUBLIC_MARKER"));
        let searched = search_code(&root, "MARKER", 0).unwrap().render();
        assert!(!searched.contains("SYNTHETIC_SECRET_MARKER"));
        assert!(searched.contains("src/deep/secret.txt:1:PUBLIC_MARKER"));
    }
}

/// 子目录的否定规则可覆盖祖先 glob，三个工具应保留同一个重新放行的文件。
///
/// **边界**：覆盖 .gitignore / .ignore 的层级优先级，不改变同目录中的其它被忽略文件。
#[test]
fn nested_negation_overrides_ancestor_ignore_for_all_tools() {
    for ignore_file in [".gitignore", ".ignore"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        fs::create_dir(path.join("src")).unwrap();
        fs::write(path.join(ignore_file), "*.key\n").unwrap();
        fs::write(path.join("src").join(ignore_file), "!public.key\n").unwrap();
        fs::write(path.join("src/public.key"), "PUBLIC_MARKER\n").unwrap();
        fs::write(path.join("src/private.key"), "SYNTHETIC_SECRET_MARKER\n").unwrap();
        let root = WorkspaceRoot::open(path).unwrap();
        assert!(read_file(&root, "src/public.key", 1)
            .unwrap()
            .render()
            .contains("PUBLIC_MARKER"));
        assert_eq!(
            read_file(&root, "src/private.key", 1).unwrap_err(),
            ToolError::Ignored
        );
        let listed = list_files(&root, "src", 1).unwrap().render();
        assert!(listed.lines().any(|line| line == "public.key"));
        assert!(!listed.contains("private.key"));
        let searched = search_code(&root, "MARKER", 0).unwrap().render();
        assert!(searched.contains("src/public.key:1:PUBLIC_MARKER"));
        assert!(!searched.contains("SYNTHETIC_SECRET_MARKER"));
    }
}

/// 列表和搜索的小结果用完整文本断言。
///
/// **边界**：目录带 `/`。被忽略和隐藏的名字不出现。
#[test]
fn list_and_search_render_the_full_text() {
    let (root, _, _) = fixture();
    assert_eq!(
        list_files(&root, ".", 1).unwrap().render(),
        "\
tool: list_files
path: .
page: 1

src/
truncated: false
"
    );
    assert_eq!(
        search_code(&root, "host", 0).unwrap().render(),
        "\
tool: search_code
query: host
offset: 0

src/lib.rs:1:fn host() {}
truncated: false
"
    );
}

/// 超长首行截断后，续读从下一行开始。
///
/// **边界**：不能把 `next_start_line` 停在同一行。第二次读取能看到后面的短行。
#[test]
fn overlong_first_line_advances_to_the_next_line() {
    let root_path = fresh_dir();
    let mut file = vec![b'a'; READ_BYTE_LIMIT + 64];
    file.extend(b"\nsecond\n");
    fs::write(root_path.join("wide.txt"), file).unwrap();
    let root = WorkspaceRoot::open(&root_path).unwrap();
    let output = read_file(&root, "wide.txt", 1).unwrap();
    assert_eq!(output.continuation, Some(2));
    assert!(output.render().contains('…'));
    assert!(!output.render().contains("second"));
    let next = read_file(&root, "wide.txt", 2).unwrap();
    assert!(next.render().contains("2|second"));
    assert!(!next.truncated);
}

/// 渲染文本里带行号的正文行。
///
/// **参数**：`rendered` 是 `ToolOutput::render` 的结果。
/// **边界**：只认 `数字|` 开头的行，头部和最后一行不算。
fn numbered_lines(rendered: &str) -> Vec<&str> {
    rendered
        .lines()
        .filter(|line| {
            line.split_once('|')
                .is_some_and(|(number, _)| number.parse::<u32>().is_ok())
        })
        .collect()
}

/// 被截断的长行前面已有内容时，不能把正文推过字节上限。
///
/// **边界**：30 行各 1023 字节后面跟一行 40 KiB。第一次只返回那 30 行，`next_start_line` 指向长行；
/// 第二次长行作为首行被截断；第三次读到后面的短行。纳入的文件字节始终不超过 `READ_BYTE_LIMIT`。
#[test]
fn clipped_line_after_content_waits_for_the_next_call() {
    let root_path = fresh_dir();
    let mut file = String::new();
    for _ in 0..30 {
        file.push_str(&"a".repeat(1023));
        file.push('\n');
    }
    file.push_str(&"b".repeat(40 * 1024));
    file.push_str("\nend\n");
    fs::write(root_path.join("mixed.txt"), file).unwrap();
    let root = WorkspaceRoot::open(&root_path).unwrap();

    let first = read_file(&root, "mixed.txt", 1).unwrap();
    let rendered = first.render();
    let lines = numbered_lines(&rendered);
    assert_eq!(lines.len(), 30);
    let kept: usize = lines
        .iter()
        .filter_map(|line| line.split_once('|').map(|(_, text)| text.len()))
        .sum();
    assert!(kept <= READ_BYTE_LIMIT);
    assert!(!rendered.contains('b'));
    assert_eq!(first.continuation, Some(31));

    let second = read_file(&root, "mixed.txt", 31).unwrap();
    let rendered = second.render();
    let lines = numbered_lines(&rendered);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("31|b") && lines[0].ends_with('…'));
    assert_eq!(second.continuation, Some(32));

    let third = read_file(&root, "mixed.txt", 32).unwrap();
    assert!(third.render().contains("32|end"));
    assert!(!third.truncated);
}

/// 行数已满时，被截断的长行同样留给下一次调用。
///
/// **边界**：`READ_LINE_LIMIT` 行短行后面跟一行 40 KiB，第一次恰好返回 `READ_LINE_LIMIT` 行。
#[test]
fn clipped_line_does_not_exceed_the_line_limit() {
    let root_path = fresh_dir();
    let mut file = String::new();
    for _ in 0..READ_LINE_LIMIT {
        file.push_str("x\n");
    }
    file.push_str(&"b".repeat(40 * 1024));
    file.push('\n');
    fs::write(root_path.join("lines.txt"), file).unwrap();
    let root = WorkspaceRoot::open(&root_path).unwrap();
    let first = read_file(&root, "lines.txt", 1).unwrap();
    let rendered = first.render();
    assert_eq!(numbered_lines(&rendered).len(), READ_LINE_LIMIT);
    assert!(!rendered.contains('b'));
    assert_eq!(first.continuation, Some(READ_LINE_LIMIT as u32 + 1));
}

/// 起始行大于 1 时，续读行号继续向前。
///
/// **边界**：第二页从给出的行开始，而不是从第 1 行重读。
#[test]
fn start_line_past_one_continues_forward() {
    let root_path = fresh_dir();
    let line = "b".repeat(1024);
    let mut file = String::new();
    for _ in 0..40 {
        file.push_str(&line);
        file.push('\n');
    }
    fs::write(root_path.join("paged.txt"), file).unwrap();
    let root = WorkspaceRoot::open(&root_path).unwrap();
    let first = read_file(&root, "paged.txt", 1).unwrap();
    let next_line = first.continuation.expect("page continues");
    assert!(next_line > 1);
    let second = read_file(&root, "paged.txt", next_line).unwrap();
    assert!(second.render().contains(&format!("{next_line}|")));
    assert!(!second.render().contains("1|"));
}

/// 起始行超过文件末尾时不截断。
///
/// **边界**：正文为空，最后一行是 `truncated: false`。
#[test]
fn start_line_past_eof_is_empty() {
    let (root, _, _) = fixture();
    assert_eq!(
        read_file(&root, "src/lib.rs", 9).unwrap().render(),
        "\
tool: read_file
path: src/lib.rs
start_line: 9

truncated: false
"
    );
}

/// CRLF 行尾不进入交给模型的正文。
///
/// **边界**：两行都保留，行尾只有渲染自己的换行。
#[test]
fn crlf_lines_drop_the_carriage_return() {
    let root_path = fresh_dir();
    fs::write(root_path.join("crlf.txt"), "one\r\ntwo\r\n").unwrap();
    let root = WorkspaceRoot::open(&root_path).unwrap();
    assert_eq!(
        read_file(&root, "crlf.txt", 1).unwrap().render(),
        "\
tool: read_file
path: crlf.txt
start_line: 1

1|one
2|two
truncated: false
"
    );
}

/// 窗口之后的空字节仍让整次读取失败。
///
/// **边界**：不返回前面的文本行。
#[test]
fn nul_after_the_first_line_rejects_the_file() {
    let root_path = fresh_dir();
    let mut bytes = b"hello\n".to_vec();
    bytes.extend(std::iter::repeat_n(b'a', 16));
    bytes.push(0);
    fs::write(root_path.join("tail.bin"), bytes).unwrap();
    let root = WorkspaceRoot::open(&root_path).unwrap();
    let error = read_file(&root, "tail.bin", 1).unwrap_err();
    assert_eq!(error, ToolError::Binary);
    assert!(!error.to_string().contains("hello"));
}

/// 指向根外目录的符号链接不能当根内目录用。
///
/// **边界**：默认明确记为 ignored；显式执行时，缺少目录链接能力会失败并标注未验证。
#[test]
#[ignore = "requires symlink capability; CI runs explicitly with --ignored"]
fn directory_symlink_pointing_outside_is_rejected() {
    let (root, root_path, outside) = fixture();
    fs::write(outside.join("inside.txt"), "linked-body\n").unwrap();
    let link = root_path.join("linked");
    make_dir_symlink(&outside, &link);
    let error = read_file(&root, "linked/inside.txt", 1).unwrap_err();
    assert_eq!(error, ToolError::Workspace(WorkspaceError::OutsideRoot));
    let listed = list_files(&root, "linked", 1).unwrap_err();
    assert_eq!(listed, ToolError::Workspace(WorkspaceError::OutsideRoot));
}

/// 非法工具参数在读盘前失败（A5-01）。
///
/// **作用**：补齐 `page` / `start_line` / `query` 的受控拒绝，错误文本不含路径与正文。
/// **边界**：`page=0`、`start_line=0`、空查询与含换行的查询均为 `InvalidRequest`。
#[test]
fn invalid_tool_parameters_are_rejected_before_io() {
    let (root, root_path, _) = fixture();
    let before = file_digest(&root_path.join("src").join("lib.rs"));
    assert_eq!(
        list_files(&root, "src", 0).unwrap_err(),
        ToolError::InvalidRequest
    );
    assert_eq!(
        read_file(&root, "src/lib.rs", 0).unwrap_err(),
        ToolError::InvalidRequest
    );
    assert_eq!(
        search_code(&root, "", 0).unwrap_err(),
        ToolError::InvalidRequest
    );
    assert_eq!(
        search_code(&root, "host\n", 0).unwrap_err(),
        ToolError::InvalidRequest
    );
    assert_eq!(before, file_digest(&root_path.join("src").join("lib.rs")));
    assert!(!ToolError::InvalidRequest.to_string().contains("lib.rs"));
}

/// resolve 成功后把同名路径换成根外 symlink，再读必须受控失败（A5-01 / ADR-024）。
///
/// **作用**：静态、可重复地模拟「校验后路径身份变成根外链接」；不用并发线程，不加 resolve/open 测试缝。
/// **边界**：默认明确记为 ignored；显式执行时建不了链接会失败（未验证）。根外目标字节前后不变；错误与任何 Ok 正文都不得含 `OUTSIDE_SECRET`。
/// 不证明单次 `read_file` 调用内部的并发 TOCTOU 已关闭。
#[test]
#[ignore = "requires symlink capability; CI runs explicitly with --ignored"]
fn bait_path_swapped_for_outside_symlink_is_rejected() {
    let (root, root_path, outside) = fixture();
    let bait = root_path.join("bait.txt");
    let outside_target = outside.join("outside-secret.txt");
    fs::write(&bait, "INSIDE\n").unwrap();
    fs::write(&outside_target, "OUTSIDE_SECRET\n").unwrap();
    let outside_before = file_digest(&outside_target);

    assert!(root.resolve("bait.txt").is_ok());

    fs::remove_file(&bait).unwrap();
    make_file_symlink(&outside_target, &bait);

    let error = read_file(&root, "bait.txt", 1).unwrap_err();
    assert_eq!(error, ToolError::Workspace(WorkspaceError::OutsideRoot));
    assert!(!error.to_string().contains("OUTSIDE_SECRET"));
    assert!(!format!("{error:?}").contains("OUTSIDE_SECRET"));
    assert_eq!(outside_before, file_digest(&outside_target));
    assert_eq!(
        fs::read_to_string(&outside_target).unwrap(),
        "OUTSIDE_SECRET\n"
    );
}

/// 工作区关键文件在越界尝试前后字节不变（A5-01）。
///
/// **边界**：绝对路径与 `../` 失败后，根内 `src/lib.rs` 与根外 `outside.txt` 摘要不变。
#[test]
fn outside_escape_attempts_do_not_mutate_workspace_bytes() {
    let (root, root_path, outside) = fixture();
    let inside = root_path.join("src").join("lib.rs");
    let outside_file = outside.join("outside.txt");
    let inside_before = file_digest(&inside);
    let outside_before = file_digest(&outside_file);
    let _ = read_file(&root, "../outside.txt", 1);
    let _ = read_file(&root, outside_file.to_str().unwrap(), 1);
    assert_eq!(inside_before, file_digest(&inside));
    assert_eq!(outside_before, file_digest(&outside_file));
}

/// 文件内容的稳定摘要，供测试前后比对。
///
/// **边界**：用字节长度与简单滚动和，避免为测试引入哈希依赖。碰撞在本夹具下可接受。
fn file_digest(path: &Path) -> (u64, u64) {
    let bytes = fs::read(path).unwrap();
    let mut rolling = 0_u64;
    for (index, byte) in bytes.iter().enumerate() {
        rolling = rolling
            .wrapping_mul(131)
            .wrapping_add(*byte as u64)
            .wrapping_add(index as u64);
    }
    (bytes.len() as u64, rolling)
}

#[cfg(windows)]
/// 以 `\` 开头、没有盘符的路径在读盘前拒绝。
///
/// **边界**：`is_absolute` 对这种路径为假，仍然必须是 `OutsideRoot`。
#[test]
fn leading_backslash_without_a_drive_is_outside() {
    let (root, _, _) = fixture();
    let error = read_file(&root, r"\Windows\win.ini", 1).unwrap_err();
    assert_eq!(error, ToolError::Workspace(WorkspaceError::OutsideRoot));
}

/// 在当前系统上建立文件符号链接。
///
/// **参数**：`target` 是根外文件。`link` 是根内的新路径。
/// **边界**：建链失败时使显式能力验收失败，不能把未验证记录成通过；保留 OS 错误原因。
fn make_file_symlink(target: &Path, link: &Path) {
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_file(target, link);
    #[cfg(not(windows))]
    let result = std::os::unix::fs::symlink(target, link);
    result.expect("UNVERIFIED: file symlink capability is required for this security test");
}

/// 在当前系统上建立目录符号链接。
///
/// **参数**：`target` 是根外目录。`link` 是根内的新路径。
/// **边界**：建链失败时使显式能力验收失败，不能把未验证记录成通过；保留 OS 错误原因。
fn make_dir_symlink(target: &Path, link: &Path) {
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_dir(target, link);
    #[cfg(not(windows))]
    let result = std::os::unix::fs::symlink(target, link);
    result.expect("UNVERIFIED: directory symlink capability is required for this security test");
}
