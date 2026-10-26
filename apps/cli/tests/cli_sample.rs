//! A4b：`flux trace --verify` 与 `flux sample export` / `feedback`。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use serde_json::{json, Value};
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
    let schema: Value = serde_json::from_str(include_str!(
        "../../../contracts/schemas/sample.schema.json"
    ))
    .unwrap();
    jsonschema::draft202012::new(&schema)
        .unwrap()
        .validate(&sample)
        .unwrap();
    assert_eq!(sample["schema_version"], "0.3-draft");
    assert_eq!(sample["rebuild"]["status"], "consistent");
    assert_eq!(sample["feedback"].as_array().unwrap().len(), 1);
    assert_eq!(sample["feedback"][0]["verdict"], "usable");
    assert!(sample.get("reasoning_content").is_none());
    let text = String::from_utf8_lossy(&export);
    assert!(!text.contains("Fake Engine A report."));
    assert_eq!(sample["calls"][0]["tools"][0]["tool_call_id"], "call_list");
    let mut again = exported(data.path(), run_id);
    let mut previous = sample;
    again.as_object_mut().unwrap().remove("exported_at");
    previous.as_object_mut().unwrap().remove("exported_at");
    assert_eq!(previous, again);
}

/// 为故障注入创建独立 Fake Run；预算参数可让 Run 停在工具轮末尾。
fn fixture(extra: &[&str]) -> (TempDir, TempDir, String) {
    let data = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let output = flux()
        .args(["run", "--workspace"])
        .arg(workspace.path())
        .args(["--prompt", "analyze", "--engine", "fake", "--data-dir"])
        .arg(data.path())
        .args(extra)
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let run_id = parse_run_id(&stdout).to_owned();
    (data, workspace, run_id)
}

/// 读取导出 JSON；测试同时经过 CLI 和 Host 的实际读取路径。
fn exported(data: &Path, run_id: &str) -> Value {
    let output = flux()
        .args(["sample", "export", run_id, "--data-dir"])
        .arg(data)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

/// 只在独立测试库中修改某条事件，模拟损坏的调用身份或旧版本事件。
fn edit_event(data: &Path, kind: &str, edit: impl FnOnce(&mut Value)) {
    let connection = rusqlite::Connection::open(data.join("host.db")).unwrap();
    let (sequence, text): (i64, String) = connection
        .query_row(
            "SELECT sequence, payload FROM events WHERE event_type=?1 ORDER BY sequence LIMIT 1",
            [kind],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut payload: Value = serde_json::from_str(&text).unwrap();
    edit(&mut payload);
    connection
        .execute(
            "UPDATE events SET payload=?1 WHERE sequence=?2",
            rusqlite::params![payload.to_string(), sequence],
        )
        .unwrap();
}

/// 从实际事件引用定位临时 blob；损坏仅限测试数据，不触碰开发数据目录。
fn model_blob(data: &Path, key: &str) -> PathBuf {
    let connection = rusqlite::Connection::open(data.join("host.db")).unwrap();
    let payload: String = connection.query_row(
        "SELECT payload FROM events WHERE event_type='model.call_completed' ORDER BY sequence LIMIT 1",
        [], |row| row.get(0)).unwrap();
    let payload: Value = serde_json::from_str(&payload).unwrap();
    let hex = payload[key]
        .as_str()
        .unwrap()
        .strip_prefix("sha256:")
        .unwrap();
    data.join("blobs")
        .join(&hex[..2])
        .join(&hex[2..4])
        .join(hex)
}

/// 尾轮错误 ID、错误工具名不能通过；旧事件没有 ID 时明确不可验证。
#[test]
fn tail_tool_identity_and_legacy_events_are_checked() {
    let (data, _workspace, run_id) = fixture(&["--max-model-calls", "1"]);
    for (key, value) in [
        ("tool_call_id", "call_search"),
        ("tool_name", "search_code"),
    ] {
        edit_event(data.path(), "tool.call_completed", |payload| {
            payload["tool_call_id"] = json!("call_list");
            payload["tool_name"] = json!("list_files");
            payload[key] = json!(value);
        });
        flux()
            .args(["trace", &run_id, "--verify", "--data-dir"])
            .arg(data.path())
            .assert()
            .failure()
            .stdout(predicate::str::contains("rule=6"));
    }
    edit_event(data.path(), "tool.call_completed", |payload| {
        payload["tool_name"] = json!("list_files");
        payload.as_object_mut().unwrap().remove("tool_call_id");
    });
    flux()
        .args(["trace", &run_id, "--data-dir"])
        .arg(data.path())
        .assert()
        .success();
    flux()
        .args(["trace", &run_id, "--verify", "--data-dir"])
        .arg(data.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "unverifiable missing=tool_call_id#1:1",
        ));
    assert_eq!(
        exported(data.path(), &run_id)["rebuild"]["status"],
        "unverifiable"
    );
}

/// 有效 JSON 标量不构成模型请求；CLI 与样本必须保留不可验证结论。
#[test]
fn scalar_request_blob_is_unverifiable() {
    let (data, _workspace, run_id) = fixture(&["--max-model-calls", "1"]);
    fs::write(model_blob(data.path(), "request_ref"), "42").unwrap();
    flux()
        .args(["trace", &run_id, "--json", "--verify", "--data-dir"])
        .arg(data.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("request_ref#1"));
    assert_eq!(
        exported(data.path(), &run_id)["rebuild"]["status"],
        "unverifiable"
    );
}

/// 坏工具条目既不能被响应解析过滤，也不能通过样本导出的校验。
#[test]
fn malformed_tool_entry_is_not_dropped() {
    let (data, _workspace, run_id) = fixture(&[]);
    let path = model_blob(data.path(), "response_ref");
    let mut response: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    response["tool_calls"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"broken"}));
    fs::write(path, response.to_string()).unwrap();
    flux()
        .args(["trace", &run_id, "--verify", "--data-dir"])
        .arg(data.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "unverifiable missing=response_ref#1",
        ));
}

/// 损坏的请求/响应默认不得回显思维链，显式 include-reasoning 仍能诊断原文。
#[test]
fn corrupt_json_is_hidden_without_reasoning_flag() {
    let (data, _workspace, run_id) = fixture(&[]);
    for (key, flag) in [
        ("request_ref", "--show-request"),
        ("response_ref", "--show-response"),
    ] {
        fs::write(
            model_blob(data.path(), key),
            r#"{"reasoning_content":"PRIVATE_SENTINEL","#,
        )
        .unwrap();
        flux()
            .args(["trace", &run_id, flag, "1", "--data-dir"])
            .arg(data.path())
            .assert()
            .success()
            .stdout(predicate::str::contains("unavailable"))
            .stdout(predicate::str::contains("PRIVATE_SENTINEL").not());
        flux()
            .args([
                "trace",
                &run_id,
                flag,
                "1",
                "--include-reasoning",
                "--data-dir",
            ])
            .arg(data.path())
            .assert()
            .success()
            .stdout(predicate::str::contains("PRIVATE_SENTINEL"));
    }
}

/// 三个文档化 verdict 都能写入并读回；旧的 needs-edit 拼法保持为别名。
#[test]
fn documented_verdicts_round_trip() {
    let (data, _workspace, run_id) = fixture(&[]);
    for verdict in ["usable", "needs_edit", "redo", "needs-edit"] {
        flux()
            .args([
                "sample",
                "feedback",
                &run_id,
                "--verdict",
                verdict,
                "--data-dir",
            ])
            .arg(data.path())
            .assert()
            .success();
    }
    let sample = exported(data.path(), &run_id);
    let verdicts: Vec<_> = sample["feedback"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["verdict"].as_str().unwrap())
        .collect();
    assert_eq!(verdicts, ["usable", "needs_edit", "redo", "needs_edit"]);
    flux()
        .args([
            "sample",
            "feedback",
            &run_id,
            "--verdict",
            "invalid",
            "--data-dir",
        ])
        .arg(data.path())
        .assert()
        .failure();
}
