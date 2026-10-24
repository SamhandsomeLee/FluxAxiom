//! A3a：CLI Fake 路径贯通。
//!
//! **作用**：`flux run --engine fake` 创建并驱动 Run；同一数据目录上的另一次调用可读 `status` / `events` / `artifacts`。
//! **边界**：不拨号。不测 Ctrl-C / 孤儿扫描。

use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;

fn flux() -> Command {
    Command::cargo_bin("flux").unwrap()
}

/// 从 `flux run` 的 stdout 取出 `run_id=` 行。
fn parse_run_id(stdout: &str) -> &str {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("run_id="))
        .expect("run_id line")
}

/// Fake run 完成后，跨调用可读终态、事件与 Artifact。
#[test]
fn fake_run_is_readable_from_a_second_invocation() {
    let data = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let data_dir = data.path();

    let run = flux()
        .args([
            "run",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--prompt",
            "analyze",
            "--request-id",
            "cli_fake_1",
            "--engine",
            "fake",
            "--data-dir",
        ])
        .arg(data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("run_id=run_"))
        .stdout(predicate::str::contains("status=Completed"))
        .stdout(predicate::str::contains("event seq="));

    let stdout = String::from_utf8_lossy(&run.get_output().stdout);
    let run_id = parse_run_id(&stdout);

    flux()
        .args(["status", run_id, "--data-dir"])
        .arg(data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("status=Completed"));

    flux()
        .args(["events", run_id, "--after-sequence", "0", "--data-dir"])
        .arg(data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("run.completed"));

    flux()
        .args(["events", run_id, "--follow", "--data-dir"])
        .arg(data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("run.completed"));

    flux()
        .args(["artifacts", run_id, "--data-dir"])
        .arg(data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("type=report"));
}

/// 缺 `DEEPSEEK_API_KEY` 时在落库之前失败：数据目录不被创建，也就没有孤儿 Run。
#[test]
fn deepseek_without_key_fails_before_creating_a_run() {
    let root = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let data_dir = root.path().join("never-created");

    flux()
        .env_remove("DEEPSEEK_API_KEY")
        .args([
            "run",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--prompt",
            "analyze",
            "--engine",
            "deepseek",
            "--data-dir",
        ])
        .arg(&data_dir)
        .assert()
        .failure()
        .stderr(predicate::str::contains("DEEPSEEK_API_KEY"))
        .stdout(predicate::str::contains("run_id=").not());
    assert!(!data_dir.exists());
}

/// 不带 `--follow` 的 `events` 在 Run 不存在时报错，不静默输出空内容。
#[test]
fn events_for_an_unknown_run_is_an_error() {
    let first = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();

    let run = flux()
        .args([
            "run",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--prompt",
            "analyze",
            "--request-id",
            "cli_unknown_1",
            "--data-dir",
        ])
        .arg(first.path())
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&run.get_output().stdout);
    let run_id = parse_run_id(&stdout);

    flux()
        .args(["events", run_id, "--data-dir"])
        .arg(other.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}
