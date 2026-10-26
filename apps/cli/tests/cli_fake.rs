//! A3a / A4a：CLI Fake 路径贯通与 trace / 报告读取。
//!
//! **作用**：`flux run --engine fake` 创建并驱动 Run；同一数据目录上可读 `status` / `events` / `artifacts` / `trace`。
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

/// 取出以 `header_prefix` 开头的那一行之后的全部输出。
///
/// **边界**：只取第一处。找不到该行时测试失败。
fn body_after_header<'a>(output: &'a str, header_prefix: &str) -> &'a str {
    let start = output
        .find(header_prefix)
        .unwrap_or_else(|| panic!("no `{header_prefix}` header in output"));
    let after_header = output[start..].find('\n').expect("header line end") + start + 1;
    &output[after_header..]
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
        .stdout(predicate::str::contains("event seq="))
        .stdout(predicate::str::contains("--- report "))
        .stdout(predicate::str::contains("Fake Engine A report."))
        .stdout(predicate::str::contains("outcome=").not());

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

    let shown = flux()
        .args(["artifacts", run_id, "--show", "--data-dir"])
        .arg(data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("Fake Engine A report."));
    // 两条路径各自带分隔头；头之后到输出结束的字节必须完全相同，并且以换行结尾。
    let shown_text = String::from_utf8_lossy(&shown.get_output().stdout);
    let run_body = body_after_header(&stdout, "--- report ");
    let shown_body = body_after_header(&shown_text, "--- artifact ");
    assert_eq!(
        run_body, shown_body,
        "run report and artifacts --show differ"
    );
    assert_eq!(run_body, "Fake Engine A report.\n");

    flux()
        .args(["trace", run_id, "--data-dir"])
        .arg(data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("#1 model="))
        .stdout(predicate::str::contains("#2 model="))
        .stdout(predicate::str::contains("#3 model="))
        .stdout(predicate::str::contains("tool list_files"))
        .stdout(predicate::str::contains("tool search_code"))
        .stdout(predicate::str::contains("totals model_calls=3/"))
        .stdout(predicate::str::contains("look around").not());

    flux()
        .args(["trace", run_id, "--json", "--data-dir"])
        .arg(data_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("\"rounds\""))
        .stdout(predicate::str::contains("\"totals\""));
}

/// 缺 `DEEPSEEK_API_KEY` 时在落库之前失败：数据目录不被创建，也就没有孤儿 Run。
///
/// **边界**：设空串而不是 `env_remove`：CLI 启动会 `dotenvy` 加载仓库 `.env`，
/// 若变量已不存在会被文件写回；空串则不被覆盖，`from_env` 仍失败。
#[test]
fn deepseek_without_key_fails_before_creating_a_run() {
    let root = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let data_dir = root.path().join("never-created");

    flux()
        .env("DEEPSEEK_API_KEY", "")
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
