use std::fs;
use std::path::PathBuf;

use fluxaxiom_contract::{
    Artifact, Attempt, Error, Event, Run, RunSnapshot, Sample, Session, SCHEMA_VERSION,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_json(path: &str) -> Value {
    let text = fs::read_to_string(repo_root().join(path)).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn assert_valid(schema_path: &str, instance: &Value) {
    let schema = read_json(schema_path);
    let validator = jsonschema::draft202012::new(&schema).expect(schema_path);
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|error| error.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "{schema_path} rejected {instance}: {errors:?}"
    );
}

fn assert_invalid(schema_path: &str, instance: &Value) {
    let schema = read_json(schema_path);
    let validator = jsonschema::draft202012::new(&schema).expect(schema_path);
    assert!(
        !validator.is_valid(instance),
        "{schema_path} accepted {instance}"
    );
}

fn round_trip<T>(schema_path: &str, example_path: &str)
where
    T: DeserializeOwned + Serialize,
{
    let original = read_json(example_path);
    assert_valid(schema_path, &original);
    let typed: T = serde_json::from_value(original.clone()).unwrap();
    let serialized = serde_json::to_value(&typed).unwrap();
    assert_valid(schema_path, &serialized);
    let again: T = serde_json::from_value(serialized).unwrap();
    let once_more = serde_json::to_value(&again).unwrap();
    assert_eq!(serde_json::to_value(&typed).unwrap(), once_more);
}

#[test]
fn examples_match_schemas_and_rust_types() {
    round_trip::<Session>(
        "contracts/schemas/session.schema.json",
        "contracts/examples/session.json",
    );
    round_trip::<Run>(
        "contracts/schemas/run.schema.json",
        "contracts/examples/run.json",
    );
    round_trip::<Attempt>(
        "contracts/schemas/attempt.schema.json",
        "contracts/examples/attempt.json",
    );
    round_trip::<Event>(
        "contracts/schemas/event.schema.json",
        "contracts/examples/event-run-accepted.json",
    );
    round_trip::<Event>(
        "contracts/schemas/event.schema.json",
        "contracts/examples/event-model-call.json",
    );
    round_trip::<Event>(
        "contracts/schemas/event.schema.json",
        "contracts/examples/event-context-exhausted.json",
    );
    round_trip::<Event>(
        "contracts/schemas/event.schema.json",
        "contracts/examples/event-tool-call.json",
    );
    round_trip::<Event>(
        "contracts/schemas/event.schema.json",
        "contracts/examples/event-artifact-created.json",
    );
    round_trip::<Artifact>(
        "contracts/schemas/artifact.schema.json",
        "contracts/examples/artifact.json",
    );
    round_trip::<RunSnapshot>(
        "contracts/schemas/run-snapshot.schema.json",
        "contracts/examples/run-snapshot.json",
    );
    round_trip::<Error>(
        "contracts/schemas/error.schema.json",
        "contracts/examples/error.json",
    );
    round_trip::<Sample>(
        "contracts/schemas/sample.schema.json",
        "contracts/examples/sample.json",
    );
    round_trip::<Event>(
        "contracts/schemas/event.schema.json",
        "contracts/examples/event-attempt-started.json",
    );
}

#[test]
fn schema_version_is_draft() {
    assert_eq!(SCHEMA_VERSION, "0.3-draft");
    let event = read_json("contracts/examples/event-run-accepted.json");
    assert_eq!(event["schema_version"], "0.3-draft");
}

#[test]
fn reasoning_content_is_rejected_in_event_payload() {
    let mut event = read_json("contracts/examples/event-model-call.json");
    event["payload"]["reasoning_content"] = Value::String("hidden".to_owned());
    assert_invalid("contracts/schemas/event.schema.json", &event);
}

/// 新工具事件携带关联 ID，旧事件省略时仍能完整往返且不补造字段。
#[test]
fn tool_call_id_is_optional_but_preserved_in_full_event_json() {
    let legacy = read_json("contracts/examples/event-tool-call.json");
    let mut current = legacy.clone();
    current["payload"]["tool_call_id"] = Value::String("call_read_1".into());
    for value in [legacy, current] {
        assert_valid("contracts/schemas/event.schema.json", &value);
        let event: Event = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(event).unwrap(), value);
    }
    let mut invalid = read_json("contracts/examples/event-tool-call.json");
    invalid["payload"]["tool_call_id"] = Value::String(String::new());
    assert_invalid("contracts/schemas/event.schema.json", &invalid);
}

/// 旧模型事件省略 context 字段仍合法，读回后写出显式 null。半对和空串拒绝。
#[test]
fn model_call_context_fields_are_a_pair() {
    let current = read_json("contracts/examples/event-model-call.json");
    assert_valid("contracts/schemas/event.schema.json", &current);
    let event: Event = serde_json::from_value(current.clone()).unwrap();
    assert_eq!(serde_json::to_value(&event).unwrap(), current);

    let mut legacy = current.clone();
    let payload = legacy["payload"].as_object_mut().unwrap();
    payload.remove("context_ref");
    payload.remove("context_strategy");
    assert_valid("contracts/schemas/event.schema.json", &legacy);
    let parsed: Event = serde_json::from_value(legacy).unwrap();
    let rewritten = serde_json::to_value(&parsed).unwrap();
    assert_eq!(rewritten["payload"]["context_ref"], Value::Null);
    assert_eq!(rewritten["payload"]["context_strategy"], Value::Null);
    assert_valid("contracts/schemas/event.schema.json", &rewritten);

    let mut one_sided = current.clone();
    one_sided["payload"]["context_strategy"] = Value::Null;
    assert_invalid("contracts/schemas/event.schema.json", &one_sided);
    assert!(serde_json::from_value::<Event>(one_sided).is_err());

    let mut empty = current;
    empty["payload"]["context_ref"] = Value::String(String::new());
    assert_invalid("contracts/schemas/event.schema.json", &empty);
    assert!(serde_json::from_value::<Event>(empty).is_err());
}

/// 耗尽诊断只有引用和策略名，完整往返；缺键、空串、null 与额外正文均拒绝。
#[test]
fn context_exhausted_diagnostic_matches_schema_and_rust() {
    let value = read_json("contracts/examples/event-context-exhausted.json");
    assert_valid("contracts/schemas/event.schema.json", &value);
    let event: Event = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(event).unwrap(), value);
    for field in ["context_ref", "context_strategy"] {
        for replacement in [None, Some(Value::Null), Some(Value::String(String::new()))] {
            let mut broken = value.clone();
            let payload = broken["payload"].as_object_mut().unwrap();
            match replacement {
                Some(replacement) => {
                    payload.insert(field.to_owned(), replacement);
                }
                None => {
                    payload.remove(field);
                }
            }
            assert_invalid("contracts/schemas/event.schema.json", &broken);
            assert!(serde_json::from_value::<Event>(broken).is_err());
        }
    }
    let mut with_body = value;
    with_body["payload"]["reasoning_content"] = Value::String("PRIVATE_BODY".to_owned());
    assert_invalid("contracts/schemas/event.schema.json", &with_body);
    assert!(serde_json::from_value::<Event>(with_body).is_err());
}

/// `context_exhausted` 能通过 Run、快照、样本和失败事件的 schema。
#[test]
fn context_exhausted_is_a_terminal_reason() {
    let mut run = read_json("contracts/examples/run.json");
    run["status"] = Value::String("Failed".to_owned());
    run["terminal_reason"] = Value::String("context_exhausted".to_owned());
    assert_valid("contracts/schemas/run.schema.json", &run);

    let mut snapshot = read_json("contracts/examples/run-snapshot.json");
    snapshot["status"] = Value::String("Failed".to_owned());
    snapshot["terminal_reason"] = Value::String("context_exhausted".to_owned());
    assert_valid("contracts/schemas/run-snapshot.schema.json", &snapshot);

    let mut sample = read_json("contracts/examples/sample.json");
    sample["run"]["status"] = Value::String("Failed".to_owned());
    sample["run"]["terminal_reason"] = Value::String("context_exhausted".to_owned());
    assert_valid("contracts/schemas/sample.schema.json", &sample);

    let mut event = read_json("contracts/examples/event-run-accepted.json");
    event["event_type"] = Value::String("run.failed".to_owned());
    event["payload"] = serde_json::json!({ "terminal_reason": "context_exhausted" });
    assert_valid("contracts/schemas/event.schema.json", &event);
    let parsed: Event = serde_json::from_value(event.clone()).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap(), event);
}

/// 新样本可以带 context 引用；空串仍然非法。旧样本省略这两个键。
#[test]
fn sample_call_keeps_optional_context_fields() {
    let mut sample = read_json("contracts/examples/sample.json");
    assert_valid("contracts/schemas/sample.schema.json", &sample);
    sample["calls"][0]["context_ref"] = Value::String("sha256:abc".to_owned());
    sample["calls"][0]["context_strategy"] = Value::String("identity@1".to_owned());
    assert_valid("contracts/schemas/sample.schema.json", &sample);
    let parsed: Sample = serde_json::from_value(sample.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&parsed).unwrap()["calls"][0]["context_strategy"],
        "identity@1"
    );
    sample["calls"][0]["context_ref"] = Value::String(String::new());
    assert_invalid("contracts/schemas/sample.schema.json", &sample);
}

#[test]
fn running_run_cannot_carry_a_terminal_reason() {
    let mut run = read_json("contracts/examples/run.json");
    run["terminal_reason"] = Value::String("completed".to_owned());
    assert_invalid("contracts/schemas/run.schema.json", &run);
}
