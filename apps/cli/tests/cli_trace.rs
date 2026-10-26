//! A4a：`flux trace` 边界与坏 blob。
//!
//! **作用**：补齐 §4.11 验收中 CLI 侧的不存在 Run、坏 ref、`--show-request` 遮盖。
//! **边界**：不拨号。

use std::fs;
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;

fn flux() -> Command {
    Command::cargo_bin("flux").unwrap()
}

fn parse_run_id(stdout: &str) -> &str {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("run_id="))
        .expect("run_id line")
}

/// 不存在的 Run：`trace` 非零，文案含 not found。
#[test]
fn trace_unknown_run_is_an_error() {
    let data = TempDir::new().unwrap();
    flux()
        .args([
            "trace",
            "run_00000000-0000-7000-8000-000000000000",
            "--data-dir",
        ])
        .arg(data.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

/// 删掉请求 blob 后 `trace` 仍成功；`--show-request` 显示 unavailable。
#[test]
fn missing_request_blob_is_unavailable_not_fatal() {
    let data = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let run = flux()
        .args([
            "run",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--prompt",
            "analyze",
            "--engine",
            "fake",
            "--data-dir",
        ])
        .arg(data.path())
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&run.get_output().stdout);
    let run_id = parse_run_id(&stdout);

    let json = flux()
        .args(["trace", run_id, "--json", "--data-dir"])
        .arg(data.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let request_ref = value["rounds"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|round| round["model"]["request_ref"].as_str())
        .expect("request_ref");
    let hex = request_ref.strip_prefix("sha256:").expect("sha256 ref");
    let blob_path = data
        .path()
        .join("blobs")
        .join(&hex[0..2])
        .join(&hex[2..4])
        .join(hex);
    fs::remove_file(&blob_path).unwrap();

    flux()
        .args(["trace", run_id, "--data-dir"])
        .arg(data.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("totals model_calls=3/"));

    flux()
        .args(["trace", run_id, "--show-request", "1", "--data-dir"])
        .arg(data.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("unavailable"));
}

/// `--show-request` 默认遮盖 reasoning；`--include-reasoning` 才露出脚本原文。
///
/// **边界**：第 1 轮请求还没有 assistant，故用第 2 轮（历史已含第 1 轮的 `look around`）。
#[test]
fn show_request_redacts_reasoning_unless_flagged() {
    let data = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let run = flux()
        .args([
            "run",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--prompt",
            "analyze",
            "--engine",
            "fake",
            "--data-dir",
        ])
        .arg(data.path())
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&run.get_output().stdout);
    let run_id = parse_run_id(&stdout);

    flux()
        .args(["trace", run_id, "--show-request", "2", "--data-dir"])
        .arg(data.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("[reasoning_content:"))
        .stdout(predicate::str::contains("look around").not());

    flux()
        .args([
            "trace",
            run_id,
            "--show-request",
            "2",
            "--include-reasoning",
            "--data-dir",
        ])
        .arg(data.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("look around"));
}

/// 预算耗尽的 Run：`run` 非零退出，`trace` 仍能还原已发生的轮次。
///
/// **边界**：Fake 第一轮内容为空，没有可保留的部分报告，所以这个 Run 没有 Artifact。
/// 因此 `artifacts --show` 不带 id 时成功但不输出正文，带不存在的 id 时失败。
#[test]
fn budget_exhausted_run_is_traceable_and_has_no_report() {
    let data = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let run = flux()
        .args([
            "run",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--prompt",
            "analyze",
            "--engine",
            "fake",
            "--max-model-calls",
            "2",
            "--data-dir",
        ])
        .arg(data.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("BudgetExhausted"))
        .stdout(predicate::str::contains("--- report").not())
        .stdout(predicate::str::contains("--- partial report").not());
    let stdout = String::from_utf8_lossy(&run.get_output().stdout);
    let run_id = parse_run_id(&stdout);

    flux()
        .args(["trace", run_id, "--data-dir"])
        .arg(data.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("status=Failed"))
        .stdout(predicate::str::contains("terminal_reason=BudgetExhausted"))
        .stdout(predicate::str::contains("totals model_calls=2/2"))
        .stdout(predicate::str::contains("#3 model=").not());

    flux()
        .args(["artifacts", run_id, "--show", "--data-dir"])
        .arg(data.path())
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("(no artifacts)"));

    flux()
        .args([
            "artifacts",
            run_id,
            "--show",
            "artifact_00000000-0000-7000-8000-000000000000",
            "--data-dir",
        ])
        .arg(data.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("artifact not found"));
}

/// `--show` 带不存在的 id：即使该 Run 有 Artifact 也要失败，并且不输出任何别的报告。
#[test]
fn show_with_unknown_artifact_id_fails_without_printing_others() {
    let data = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let run = flux()
        .args([
            "run",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--prompt",
            "analyze",
            "--engine",
            "fake",
            "--data-dir",
        ])
        .arg(data.path())
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&run.get_output().stdout);
    let run_id = parse_run_id(&stdout);

    flux()
        .args([
            "artifacts",
            run_id,
            "--show",
            "artifact_00000000-0000-7000-8000-000000000000",
            "--data-dir",
        ])
        .arg(data.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("Fake Engine A report.").not())
        .stderr(predicate::str::contains("artifact not found"));
}
