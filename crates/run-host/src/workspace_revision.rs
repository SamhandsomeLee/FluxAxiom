//! 工作区 git 版本采集（A4b / §4.12.3）。
//!
//! **作用**：在 `prepare_drive` 进入 Running 之前记录「当时分析的是哪一版代码」。
//! **边界**：由 Host 直接执行 `git`，不经 shell；失败记 `Unknown`，不使 Run 失败。

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use fluxaxiom_contract::WorkspaceRevision;

/// 每条 git 命令的超时（秒）。
const GIT_TIMEOUT_SECS: u64 = 5;

/// 采集工作区版本。
///
/// **作用**：返回写入 `attempt.started` / 样本导出的 `WorkspaceRevision`。
/// **参数**：`root` 是已打开的授权工作区绝对路径。
/// **边界**：非 git、找不到 `git`、超时、无 HEAD、或任一命令失败 → `Unknown`。
/// 设置 `GIT_OPTIONAL_LOCKS=0`，避免 `status` 刷新 index 改写工作区。
pub(crate) fn collect(root: &Path) -> WorkspaceRevision {
    let commit = match git_stdout(root, &["rev-parse", "HEAD"]) {
        Some(text) => {
            let trimmed = text.trim();
            if trimmed.len() == 40 && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
                trimmed.to_owned()
            } else {
                return WorkspaceRevision::Unknown;
            }
        }
        None => return WorkspaceRevision::Unknown,
    };
    let prefix = match git_stdout(root, &["rev-parse", "--show-prefix"]) {
        Some(text) => text.trim_end_matches(['\r', '\n', '/']).to_owned(),
        None => return WorkspaceRevision::Unknown,
    };
    let dirty = match git_stdout(root, &["status", "--porcelain", "--", "."]) {
        Some(text) => !text.trim().is_empty(),
        None => return WorkspaceRevision::Unknown,
    };
    WorkspaceRevision::Git {
        commit,
        dirty,
        prefix,
    }
}

/// 跑一条 git 子命令，成功时返回 stdout；失败 / 超时 / 找不到可执行文件返回 `None`。
fn git_stdout(root: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(GIT_TIMEOUT_SECS);
    // 一边等待进程一边排空 stdout，避免 status 输出写满管道后相互等待。
    let mut stdout = child.stdout.take()?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut text = String::new();
        let result = stdout.read_to_string(&mut text).map(|_| text);
        let _ = sender.send(result);
    });
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return receiver
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    .ok()?
                    .ok();
            }
            Ok(Some(_)) => return None,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()
            .expect("git available for A4b tests");
        assert!(status.success(), "git {args:?} failed");
    }

    /// 干净仓库：`dirty=false`，`prefix` 为空。
    #[test]
    fn clean_repo_is_not_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init"]);
        git(root, &["checkout", "-b", "main"]);
        fs::write(root.join("a.txt"), "a\n").unwrap();
        git(root, &["add", "a.txt"]);
        git(root, &["commit", "-m", "init"]);
        let before = fs::read(root.join(".git").join("index")).unwrap();
        match collect(root) {
            WorkspaceRevision::Git {
                commit,
                dirty,
                prefix,
            } => {
                assert_eq!(commit.len(), 40);
                assert!(!dirty);
                assert!(prefix.is_empty());
            }
            WorkspaceRevision::Unknown => panic!("expected git"),
        }
        let after = fs::read(root.join(".git").join("index")).unwrap();
        assert_eq!(before, after, "index must not change");
    }

    /// 已跟踪文件改动或未跟踪文件都算脏。
    #[test]
    fn dirty_when_tracked_or_untracked_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init"]);
        git(root, &["checkout", "-b", "main"]);
        fs::write(root.join("a.txt"), "a\n").unwrap();
        git(root, &["add", "a.txt"]);
        git(root, &["commit", "-m", "init"]);
        fs::write(root.join("a.txt"), "b\n").unwrap();
        assert!(matches!(
            collect(root),
            WorkspaceRevision::Git { dirty: true, .. }
        ));
        git(root, &["checkout", "--", "a.txt"]);
        fs::write(root.join("new.txt"), "n\n").unwrap();
        assert!(matches!(
            collect(root),
            WorkspaceRevision::Git { dirty: true, .. }
        ));
    }

    /// 工作区是子目录时 `prefix` 正确，仓库别处改动不算脏。
    #[test]
    fn subdirectory_prefix_ignores_sibling_dirt() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init"]);
        git(root, &["checkout", "-b", "main"]);
        fs::create_dir(root.join("ws")).unwrap();
        fs::write(root.join("ws").join("a.txt"), "a\n").unwrap();
        fs::write(root.join("outside.txt"), "o\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "init"]);
        fs::write(root.join("outside.txt"), "changed\n").unwrap();
        match collect(&root.join("ws")) {
            WorkspaceRevision::Git { dirty, prefix, .. } => {
                assert!(!dirty);
                assert_eq!(prefix, "ws");
            }
            WorkspaceRevision::Unknown => panic!("expected git"),
        }
    }

    /// 非 git 目录为 `unknown`。
    #[test]
    fn non_git_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(collect(dir.path()), WorkspaceRevision::Unknown);
    }

    /// 大量脏文件的 status 输出超过管道容量时仍能采集，不会误触发五秒超时。
    #[test]
    fn large_status_output_does_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init"]);
        fs::write(root.join("seed"), "seed").unwrap();
        git(root, &["add", "seed"]);
        git(root, &["commit", "-m", "init"]);
        for index in 0..1500 {
            fs::write(
                root.join(format!("file-{index:04}-{}", "x".repeat(80))),
                "x",
            )
            .unwrap();
        }
        let output = git_stdout(root, &["status", "--porcelain", "--", "."])
            .expect("stdout must be drained while git runs");
        assert!(output.len() > 128 * 1024);
        assert!(matches!(
            collect(root),
            WorkspaceRevision::Git { dirty: true, .. }
        ));
    }
}
