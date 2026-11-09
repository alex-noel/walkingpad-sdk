use serde_json::Value;
use std::{
    path::PathBuf,
    process::{Command, Output},
};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_walkingpad"))
        .args(args)
        .output()
        .unwrap()
}

fn rows(output: &Output) -> Vec<Value> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn simulated_speed_expires_and_disconnects() {
    let data = rows(&run(&["simulate", "--speed-kmh", "3"]));
    assert_eq!(data.len(), 6);
    assert!((data[0]["usable_speed_mps"].as_f64().unwrap() - 3.0 / 3.6).abs() < 1e-8);
    assert_eq!(data[0]["input_source"], "simulation");
    assert_eq!(data[0]["quality"], "simulated");
    assert!(data[3]["usable_speed_mps"].is_null());
    assert_eq!(data[3]["stale"], true);
    assert_eq!(data[4]["usable_speed_mps"], 0.0);
    assert_eq!(data[5]["connected"], false);
    assert!(data[5]["usable_speed_mps"].is_null());
}

#[test]
fn replay_preserves_partial_packet_age_and_labels_recorded_input() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ftms-synthetic.jsonl");
    let data = rows(&run(&["replay", fixture.to_str().unwrap()]));
    assert_eq!(data.len(), 4);
    assert!(data.iter().all(|row| row["input_source"] == "replay"));
    assert_eq!(data[1]["distance_m"], 1205);
    assert_eq!(data[1]["field_age_ms"]["speed_mps"], 3500);
    assert!(data[1]["usable_speed_mps"].is_null());
    assert_eq!(data[2]["usable_speed_mps"], 0.0);
    assert_eq!(data[3]["connected"], false);
}

#[test]
fn captured_x21_acknowledgements_are_never_invented_zero_speed() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/x21-handshake-only.jsonl");
    let output = run(&["replay", fixture.to_str().unwrap()]);
    let data = rows(&output);
    assert!(!data.is_empty());
    assert!(data.iter().all(|row| row["input_source"] == "replay"
        && row["quality"] == "device_observation"
        && row["stale"] == true
        && row["speed_mps"].is_null()
        && row["usable_speed_mps"].is_null()));
    assert_eq!(data.last().unwrap()["connected"], false);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Undecoded"));
}

#[test]
fn captured_x21_stopped_metrics_decode_and_preserve_unknown_distance() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/x21-stopped-telemetry.jsonl");
    let data = rows(&run(&["replay", fixture.to_str().unwrap()]));
    let fresh: Vec<_> = data.iter().filter(|r| r["stale"] == false).collect();
    assert_eq!(fresh.len(), 2);
    for row in fresh {
        assert_eq!(row["input_source"], "replay");
        assert_eq!(row["speed_mps"], 0.0);
        assert_eq!(row["usable_speed_mps"], 0.0);
        assert_eq!(row["belt_state"], "stopped");
        assert_eq!(row["steps"], 0);
        assert_eq!(row["elapsed_s"], 0);
        assert!(row["distance_m"].is_null());
    }
    assert_eq!(data.last().unwrap()["connected"], false);
    assert!(data.last().unwrap()["usable_speed_mps"].is_null());
}

#[test]
fn malformed_packet_and_invalid_simulation_return_failure() {
    assert!(!run(&["decode", "0404"]).status.success());
    assert!(!run(&["simulate", "--speed-kmh", "NaN"]).status.success());
    assert!(!run(&["simulate", "--speed-kmh", "999"]).status.success());
}

#[test]
fn capture_requires_service_and_characteristic_together() {
    let output = run(&[
        "capture",
        "--device",
        "test",
        "--output",
        "unused.jsonl",
        "--service",
        "00001826-0000-1000-8000-00805f9b34fb",
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--characteristic"));
}

#[test]
fn live_input_is_explicit_and_rejects_simulation_options_before_bluetooth() {
    for args in [
        vec!["bridge", "--connection-file", "unused.json"],
        vec![
            "bridge",
            "--simulate",
            "--record",
            "unused.jsonl",
            "--connection-file",
            "unused.json",
        ],
        vec![
            "bridge",
            "--device",
            "test",
            "--simulate",
            "--connection-file",
            "unused.json",
        ],
        vec![
            "bridge",
            "--device",
            "test",
            "--speed-kmh",
            "3",
            "--connection-file",
            "unused.json",
        ],
        vec![
            "bridge",
            "--device",
            "test",
            "--demo-faults",
            "--connection-file",
            "unused.json",
        ],
        vec![
            "capture",
            "--device",
            "test",
            "--telemetry",
            "--service",
            "00011234-0000-1000-8000-00805f9b34fb",
            "--characteristic",
            "0001fed8-0000-1000-8000-00805f9b34fb",
            "--output",
            "unused.jsonl",
        ],
    ] {
        let output = run(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "arguments: {args:?}; {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
