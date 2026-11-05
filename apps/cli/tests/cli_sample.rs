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

/// 比较 trace 校验和样本导出的完整三态结果，同时核对 trace 的退出码。
///
/// **作用**：context 篡改回归经过 CLI、Host 与校验器，防止只修一条读取路径。
/// **参数**：`data` 是独立测试数据目录；`run_id` 是已终态的 Run；`expected` 是期望的完整 rebuild JSON。
/// **边界**：只有 consistent 的 trace 应零退出；sample export 对三态都正常导出。
fn assert_rebuild_result(data: &Path, run_id: &str, expected: Value) {
    let output = flux()
        .args(["trace", run_id, "--json", "--verify", "--data-dir"])
        .arg(data)
        .output()
        .unwrap();
    assert_eq!(output.status.success(), expected["status"] == "consistent");
    let trace: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(trace["rebuild"], expected);
    assert_eq!(exported(data, run_id)["rebuild"], expected);
}

/// 在独立测试库中保存修改后的 context 记录，并把事件引用指向新的内容哈希。
///
/// **作用**：构造可读取但语义矛盾的记录，排除原 blob 内容与哈希不符的干扰。
/// **参数**：`data` 是夹具目录；`edit` 只修改首轮记录的 JSON，事件上的策略名保持原值。
/// **边界**：不改原 blob，不接触开发数据；坏记录仅供本测试的校验路径读取。
fn replace_context_record(data: &Path, edit: impl FnOnce(&mut Value)) {
    let mut record: Value =
        serde_json::from_slice(&fs::read(model_blob(data, "context_ref")).unwrap()).unwrap();
    edit(&mut record);
    let host = fluxaxiom_run_host::RunHost::open(data).unwrap();
    let reference = host
        .put_blob(&serde_json::to_vec(&record).unwrap(), "application/json")
        .unwrap();
    edit_event(data, "model.call_completed", |payload| {
        payload["context_ref"] = json!(reference.as_str());
    });
}

/// 事件宣称未知策略而记录仍是 identity 时，trace 与 sample 都必须报记录矛盾。
#[test]
fn context_strategy_mismatch_is_inconsistent() {
    let (data, _workspace, run_id) = fixture(&["--max-model-calls", "1"]);
    assert_rebuild_result(data.path(), &run_id, json!({"status": "consistent"}));
    edit_event(data.path(), "model.call_completed", |payload| {
        payload["context_strategy"] = json!("mask@999");
    });
    assert_rebuild_result(
        data.path(),
        &run_id,
        json!({
            "status": "inconsistent", "call": 1, "rule": 1,
            "detail": "context strategy differs between event and record"
        }),
    );
}

/// 可解析的已知策略非法编辑和越界用量轮次是明确矛盾，不是缺少校验能力。
#[test]
fn invalid_known_context_records_are_inconsistent() {
    for case in ["l0_bad_edit", "identity_has_edits", "bad_estimate"] {
        let (data, _workspace, run_id) = fixture(&["--max-model-calls", "1"]);
        replace_context_record(data.path(), |record| match case {
            "l0_bad_edit" => record["edits"] = json!([{"op": "unexpected-edit"}]),
            "identity_has_edits" => {
                record["context_strategy"] = json!("identity@1");
                record["edits"] = json!([{"op": "unexpected-edit"}]);
            }
            "bad_estimate" => {
                record["estimate"]["source"] = json!("usage");
                record["estimate"]["anchor_round"] = json!(1);
            }
            _ => unreachable!(),
        });
        if case == "identity_has_edits" {
            edit_event(data.path(), "model.call_completed", |payload| {
                payload["context_strategy"] = json!("identity@1");
            });
        }
        let detail = match case {
            "l0_bad_edit" => "context edit cannot be replayed",
            "identity_has_edits" => "identity context record contains edits",
            "bad_estimate" => "invalid context estimate anchor",
            _ => unreachable!(),
        };
        assert_rebuild_result(
            data.path(),
            &run_id,
            json!({
                "status": "inconsistent", "call": 1, "rule": 1,
                "detail": detail
            }),
        );
    }
}

/// 未知策略、未知 schema 与明确不可重放仍为 unverifiable；旧事件仍按恒等策略校验。
#[test]
fn unsupported_and_legacy_context_records_keep_their_classification() {
    for kind in [
        "unknown_strategy",
        "unsupported_schema",
        "not_replayable",
        "legacy",
    ] {
        let (data, _workspace, run_id) = fixture(&["--max-model-calls", "1"]);
        if kind == "legacy" {
            edit_event(data.path(), "model.call_completed", |payload| {
                let payload = payload.as_object_mut().unwrap();
                payload.remove("context_ref");
                payload.remove("context_strategy");
            });
            assert_rebuild_result(data.path(), &run_id, json!({"status": "consistent"}));
        } else {
            replace_context_record(data.path(), |record| match kind {
                "unknown_strategy" => record["context_strategy"] = json!("mask@999"),
                "unsupported_schema" => record["schema_version"] = json!(4),
                "not_replayable" => record["replayable"] = json!(false),
                _ => unreachable!(),
            });
            if kind == "unknown_strategy" {
                edit_event(data.path(), "model.call_completed", |payload| {
                    payload["context_strategy"] = json!("mask@999");
                });
            }
            assert_rebuild_result(
                data.path(),
                &run_id,
                json!({
                    "status": "unverifiable", "missing": "context_replay#1"
                }),
            );
        }
    }
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
