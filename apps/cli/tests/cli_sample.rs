//! A4b：`flux trace --verify` 与 `flux sample export` / `feedback`。

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

/// Fake 多轮工具 Run：`--verify` 为 consistent；样本导出含 rebuild 与反馈。
#[test]
fn verify_and_sample_export_for_fake_run() {
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
        .args(["trace", run_id, "--verify", "--data-dir"])
        .arg(data.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("rebuild=consistent"));

    let json = flux()
        .args(["trace", run_id, "--json", "--verify", "--data-dir"])
        .arg(data.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(value["rebuild"]["status"], "consistent");

    flux()
        .args([
            "sample",
            "feedback",
            run_id,
            "--verdict",
            "usable",
            "--note",
            "ok",
            "--data-dir",
        ])
        .arg(data.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("feedback_id=fbk_"));

    let export = flux()
        .args(["sample", "export", run_id, "--data-dir"])
        .arg(data.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sample: serde_json::Value = serde_json::from_slice(&export).unwrap();
    assert_eq!(sample["schema_version"], "0.3-draft");
    assert_eq!(sample["rebuild"]["status"], "consistent");
    assert_eq!(sample["feedback"].as_array().unwrap().len(), 1);
    assert_eq!(sample["feedback"][0]["verdict"], "usable");
    assert!(sample.get("reasoning_content").is_none());
    let text = String::from_utf8_lossy(&export);
    assert!(!text.contains("Fake Engine A report."));
}
