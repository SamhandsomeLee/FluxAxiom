//! A5-03：CLI / Event / Sample 默认输出的隐私边界。
//!
//! **作用**：一次 Fake Run 扫描 `run` / `events` / `trace` / `sample` 默认输出与 Event payload，
//! 断言合成假密钥与隐藏文件正文不泄漏；`reasoning_content` 不进 Event 与默认 CLI。
//! **边界**：`--show-request --include-reasoning` 出思维链原文算有意披露。不拨号。

use std::fs;
use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;

/// 藏在 `.env` 里的假密钥正文；默认输出与 Event 不得出现。
const HIDDEN_ENV_SECRET: &str = "HIDDEN_ENV_SECRET=synth-a5-03-env-body";
/// 合成 API 假密钥标记。
const FAKE_API_KEY_LINE: &str = "FAKE_API_KEY=sk-test-A5-03-not-real";
/// Fake Engine 第一轮 reasoning 原文（见 `fake_engine`）。
const REASONING_SENTINEL: &str = "look around";

fn flux() -> Command {
    Command::cargo_bin("flux").unwrap()
}

fn parse_run_id(stdout: &str) -> &str {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("run_id="))
        .expect("run_id line")
}

/// 默认路径不得出现的敏感标记。
fn assert_no_default_leak(label: &str, text: &str) {
    for marker in [
        HIDDEN_ENV_SECRET,
        FAKE_API_KEY_LINE,
        "sk-test-A5-03-not-real",
    ] {
        assert!(
            !text.contains(marker),
            "{label} must not contain {marker:?}\n---\n{text}\n---"
        );
    }
}

/// Event / 默认 CLI 不得出现思维链原文。
fn assert_no_reasoning_plaintext(label: &str, text: &str) {
    assert!(
        !text.contains(REASONING_SENTINEL),
        "{label} must not contain reasoning sentinel {REASONING_SENTINEL:?}\n---\n{text}\n---"
    );
}

/// 带隐藏 `.env` 的工作区：Fake 工具若越权读到，会把正文带进 blob / 报告路径。
fn privacy_workspace() -> TempDir {
    let workspace = TempDir::new().unwrap();
    fs::write(workspace.path().join(".gitignore"), ".env\n").unwrap();
    fs::write(
        workspace.path().join(".env"),
        format!("{FAKE_API_KEY_LINE}\n{HIDDEN_ENV_SECRET}\n"),
    )
    .unwrap();
    fs::write(workspace.path().join("note.txt"), "public note\n").unwrap();
    workspace
}

/// 扫描数据目录里所有 event.payload 行。
fn event_payloads(data_dir: &Path) -> String {
    let db = data_dir.join("host.db");
    let connection = rusqlite::Connection::open(db).unwrap();
    let mut statement = connection
        .prepare("SELECT payload FROM events ORDER BY sequence")
        .unwrap();
    let texts: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    texts.join("\n")
}

/// 扫描 blob 目录文本（工具结果若泄漏隐藏文件会落在这里）。
///
/// **边界**：按 `blobs/aa/bb/<hex>` 布局递归；跳过 `tmp`。不要求 blob 无 reasoning（允许存思维链）。
fn blob_corpus(data_dir: &Path) -> String {
    let blobs = data_dir.join("blobs");
    if !blobs.is_dir() {
        return String::new();
    }
    let mut out = String::new();
    fn walk(dir: &Path, out: &mut String) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().and_then(|n| n.to_str()) == Some("tmp") {
                    continue;
                }
                walk(&path, out);
            } else if path.is_file() {
                out.push_str(&String::from_utf8_lossy(&fs::read(&path).unwrap()));
                out.push('\n');
            }
        }
    }
    walk(&blobs, &mut out);
    out
}

/// Fake Run：默认 CLI 与 Event 无假密钥 / 隐藏 `.env` / reasoning 原文。
///
/// **边界**：`artifacts --show` 与显式 `--include-reasoning` 不在本断言的「默认」集合里。
#[test]
fn fake_run_default_surfaces_hide_secrets_and_reasoning() {
    let data = TempDir::new().unwrap();
    let workspace = privacy_workspace();
    let data_dir = data.path();

    let run = flux()
        .args([
            "run",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--prompt",
            "analyze note.txt only",
            "--request-id",
            "cli_privacy_1",
            "--engine",
            "fake",
            "--data-dir",
        ])
        .arg(data_dir)
        .assert()
        .success();
    let run_stdout = String::from_utf8_lossy(&run.get_output().stdout);
    let run_stderr = String::from_utf8_lossy(&run.get_output().stderr);
    let run_id = parse_run_id(&run_stdout);

    assert_no_default_leak("run stdout", &run_stdout);
    assert_no_default_leak("run stderr", &run_stderr);
    assert_no_reasoning_plaintext("run stdout", &run_stdout);
    assert_no_reasoning_plaintext("run stderr", &run_stderr);

    let status = flux()
        .args(["status", run_id, "--data-dir"])
        .arg(data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status_text = String::from_utf8_lossy(&status);
    assert_no_default_leak("status", &status_text);
    assert_no_reasoning_plaintext("status", &status_text);

    let events = flux()
        .args(["events", run_id, "--after-sequence", "0", "--data-dir"])
        .arg(data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let events_text = String::from_utf8_lossy(&events);
    assert_no_default_leak("events", &events_text);
    assert_no_reasoning_plaintext("events", &events_text);
    assert!(events_text.contains("event seq="));

    let trace = flux()
        .args(["trace", run_id, "--data-dir"])
        .arg(data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let trace_text = String::from_utf8_lossy(&trace);
    assert_no_default_leak("trace", &trace_text);
    assert_no_reasoning_plaintext("trace", &trace_text);

    let trace_json = flux()
        .args(["trace", run_id, "--json", "--data-dir"])
        .arg(data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let trace_json_text = String::from_utf8_lossy(&trace_json);
    assert_no_default_leak("trace --json", &trace_json_text);
    assert_no_reasoning_plaintext("trace --json", &trace_json_text);

    let sample = flux()
        .args(["sample", "export", run_id, "--data-dir"])
        .arg(data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sample_text = String::from_utf8_lossy(&sample);
    assert_no_default_leak("sample export", &sample_text);
    assert_no_reasoning_plaintext("sample export", &sample_text);
    assert!(!sample_text.contains("Fake Engine A report."));

    let payloads = event_payloads(data_dir);
    assert_no_default_leak("event payloads", &payloads);
    assert_no_reasoning_plaintext("event payloads", &payloads);
    assert!(
        !payloads.contains("reasoning_content"),
        "event payload must not carry reasoning_content key"
    );

    let blobs = blob_corpus(data_dir);
    assert_no_default_leak("blobs", &blobs);
}

/// 显式 `--show-request --include-reasoning` 才允许思维链原文（有意披露）。
///
/// **边界**：同一次 Run 上，默认 `--show-request` 仍不得出现 sentinel。
#[test]
fn include_reasoning_is_intentional_disclosure() {
    let data = TempDir::new().unwrap();
    let workspace = privacy_workspace();
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
    let run_stdout = String::from_utf8_lossy(&run.get_output().stdout).into_owned();
    let run_id = parse_run_id(&run_stdout).to_owned();

    flux()
        .args(["trace", &run_id, "--show-request", "2", "--data-dir"])
        .arg(data.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("[reasoning_content:"))
        .stdout(predicate::str::contains(REASONING_SENTINEL).not())
        .stdout(predicate::str::contains(HIDDEN_ENV_SECRET).not())
        .stdout(predicate::str::contains(FAKE_API_KEY_LINE).not());

    flux()
        .args([
            "trace",
            &run_id,
            "--show-request",
            "2",
            "--include-reasoning",
            "--data-dir",
        ])
        .arg(data.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(REASONING_SENTINEL))
        .stdout(predicate::str::contains(HIDDEN_ENV_SECRET).not())
        .stdout(predicate::str::contains(FAKE_API_KEY_LINE).not());
}
