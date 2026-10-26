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

#[test]
fn running_run_cannot_carry_a_terminal_reason() {
    let mut run = read_json("contracts/examples/run.json");
    run["terminal_reason"] = Value::String("completed".to_owned());
    assert_invalid("contracts/schemas/run.schema.json", &run);
}
