//! `flux ctx replay` 不写 Run，Fake 下重复同一次工具调用。

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

fn event_count(data: &TempDir, run_id: &str) -> usize {
    let output = flux()
        .args(["events", run_id, "--data-dir"])
        .arg(data.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|line| line.starts_with("event "))
        .count()
}

#[test]
fn replay_fake_agrees_and_does_not_write_events() {
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
    let stdout = String::from_utf8(run.get_output().stdout.clone()).unwrap();
    let run_id = parse_run_id(&stdout);
    let before = event_count(&data, run_id);

    flux()
        .args([
            "ctx",
            "replay",
            run_id,
            "--call",
            "1",
            "--samples",
            "3",
            "--engine",
            "fake",
            "--data-dir",
        ])
        .arg(data.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("agree_rate=1"))
        .stdout(predicate::str::contains("list_files"));

    assert_eq!(event_count(&data, run_id), before);
}

#[test]
fn replay_rejects_other_strategies_and_a_missing_call() {
    flux()
        .args([
            "ctx",
            "replay",
            "run_01a175b8-f488-77ed-b6ea-153e72ff7616",
            "--call",
            "1",
            "--strategy",
            "mask@1",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("only identity@1 is available"));

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
    let stdout = String::from_utf8(run.get_output().stdout.clone()).unwrap();
    let run_id = parse_run_id(&stdout);
    flux()
        .args([
            "ctx",
            "replay",
            run_id,
            "--call",
            "9",
            "--engine",
            "fake",
            "--data-dir",
        ])
        .arg(data.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("model call 9 not found"));
}
