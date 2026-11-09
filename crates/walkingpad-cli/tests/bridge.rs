use futures::{SinkExt, StreamExt};
use serde_json::Value;
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error, Message, client::IntoClientRequest},
};
use uuid::Uuid;
use walkingpad::telemetry::{Quality, TelemetrySession, TelemetryUpdate};

struct Server {
    child: Child,
    path: PathBuf,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn start() -> (Server, Value) {
    start_with_control(false).await
}

async fn start_with_control(control: bool) -> (Server, Value) {
    let path = std::env::temp_dir().join(format!("walkingpad-test-{}.json", Uuid::new_v4()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_walkingpad"));
    command.args([
        "bridge",
        "--simulate",
        "--seconds",
        if control { "25" } else { "3" },
        "--connection-file",
        path.to_str().unwrap(),
    ]);
    if control {
        command.arg("--allow-speed-control");
    }
    let child = command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let server = Server { child, path };
    let details = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(bytes) = std::fs::read(&server.path)
                && let Ok(details) = serde_json::from_slice::<Value>(&bytes)
            {
                return details;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("bridge did not publish its connection file");
    (server, details)
}

async fn control_request(details: &Value, request: &Value) -> Value {
    let endpoint = details["url"]
        .as_str()
        .unwrap()
        .replace("/telemetry", "/control");
    let mut upgrade = endpoint.into_client_request().unwrap();
    upgrade.headers_mut().insert(
        "authorization",
        format!("Bearer {}", details["token"].as_str().unwrap())
            .parse()
            .unwrap(),
    );
    let (mut socket, _) = connect_async(upgrade).await.unwrap();
    socket
        .send(Message::Text(request.to_string().into()))
        .await
        .unwrap();
    let message = tokio::time::timeout(Duration::from_secs(12), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

#[tokio::test]
async fn speed_control_is_opt_in_and_authenticated() {
    let (_server, details) = start().await;
    assert!(details["speed_control"].is_null());
    let mut request = details["url"]
        .as_str()
        .unwrap()
        .replace("/telemetry", "/control")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", details["token"].as_str().unwrap())
            .parse()
            .unwrap(),
    );
    assert!(
        matches!(connect_async(request).await.unwrap_err(), Error::Http(r) if r.status().as_u16()==401)
    );
    let (_server, details) = start_with_control(true).await;
    assert!(
        connect_async(
            details["url"]
                .as_str()
                .unwrap()
                .replace("/telemetry", "/control")
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn speed_commands_change_telemetry_once_and_confirm_both_directions() {
    let (_server, details) = start_with_control(true).await;
    assert_eq!(details["speed_control"]["min_tenths"], 10);
    assert_eq!(details["speed_control"]["max_tenths"], 50);
    let mut upgrade = details["url"]
        .as_str()
        .unwrap()
        .into_client_request()
        .unwrap();
    upgrade.headers_mut().insert(
        "authorization",
        format!("Bearer {}", details["token"].as_str().unwrap())
            .parse()
            .unwrap(),
    );
    let (mut telemetry, _) = connect_async(upgrade).await.unwrap();
    let input: Value = loop {
        let msg = telemetry.next().await.unwrap().unwrap();
        let input: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        if input["usable_speed_mps"].as_f64().is_some() {
            break input;
        }
    };
    let up = serde_json::json!({"request_id":Uuid::new_v4(),"connection_id":input["connection_id"],
        "sequence":input["sequence"],"expected_speed_tenths":30,"delta_tenths":1});
    let reply = control_request(&details, &up).await;
    assert_eq!(reply["status"], "confirmed", "{reply}");
    assert_eq!(reply["target_kmh"], 3.1);
    let duplicate = control_request(&details, &up).await;
    assert_eq!(duplicate["status"], "rejected");
    tokio::time::sleep(Duration::from_secs(10)).await;
    // Drain buffered telemetry to the current report, not the original 3.0 snapshot.
    let current: Value = loop {
        let msg = telemetry.next().await.unwrap().unwrap();
        let value: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        if value["elapsed_s"].as_u64().unwrap_or(0) >= 11 {
            break value;
        }
    };
    assert!((current["usable_speed_mps"].as_f64().unwrap() * 3.6 - 3.1).abs() < 1e-8);
    let down = serde_json::json!({"request_id":Uuid::new_v4(),"connection_id":current["connection_id"],
        "sequence":current["sequence"],"expected_speed_tenths":31,"delta_tenths":-1});
    let reply = control_request(&details, &down).await;
    assert_eq!(reply["status"], "confirmed", "{reply}");
    assert_eq!(reply["target_kmh"], 3.0);
}

#[tokio::test]
async fn bridge_authenticates_streams_and_invalidates_on_shutdown() {
    let (mut server, details) = start().await;
    let url = details["url"].as_str().unwrap();
    assert!(url.starts_with("ws://127.0.0.1:"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&server.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let denied = connect_async(url).await.unwrap_err();
    assert!(matches!(denied, Error::Http(response) if response.status().as_u16() == 401));
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", details["token"].as_str().unwrap())
            .parse()
            .unwrap(),
    );
    let (mut socket, _) = connect_async(request).await.unwrap();
    let packets = tokio::time::timeout(Duration::from_secs(5), async {
        let mut packets = Vec::new();
        while let Some(message) = socket.next().await {
            if let Message::Text(text) = message.unwrap() {
                let value: Value = serde_json::from_str(&text).unwrap();
                let ended = value["connected"] == false;
                packets.push(value);
                if ended {
                    break;
                }
            }
        }
        packets
    })
    .await
    .unwrap();
    assert!(packets.len() >= 2);
    assert!(packets.iter().all(|p| p["input_source"] == "simulation"));
    assert!(packets.iter().any(|p| {
        p["usable_speed_mps"]
            .as_f64()
            .is_some_and(|speed| speed > 0.0)
    }));
    assert_eq!(packets.last().unwrap()["connected"], false);
    assert!(packets.last().unwrap()["usable_speed_mps"].is_null());
    assert!(server.child.wait().unwrap().success());
    assert!(!server.path.exists());
}

#[tokio::test]
async fn subscriber_consumes_the_same_contract_as_a_game() {
    let (mut server, _) = start().await;
    let path = server.path.clone();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_walkingpad"))
            .args(["subscribe", "--connection-file", path.to_str().unwrap()])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let packets: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(packets.len() > 1);
    assert_eq!(packets.last().unwrap()["connected"], false);
    assert!(server.child.wait().unwrap().success());
}

#[tokio::test]
async fn subscriber_invalidates_last_speed_when_a_server_stalls() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let path = std::env::temp_dir().join(format!("walkingpad-stall-test-{}.json", Uuid::new_v4()));
    std::fs::write(&path, serde_json::to_vec(&serde_json::json!({
        "schema_version": 1, "url": format!("ws://{}/telemetry", listener.local_addr().unwrap()),
        "token": "synthetic-test-token", "input_source": "simulation"
    })).unwrap()).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut session = TelemetrySession::new(Quality::Simulated);
        session
            .apply(
                TelemetryUpdate {
                    speed_mps: Some(1.0),
                    ..Default::default()
                },
                Duration::ZERO,
            )
            .unwrap();
        let snapshot = session.snapshot(Duration::ZERO, Duration::from_secs(2));
        socket
            .send(Message::Text(
                serde_json::to_string(&snapshot).unwrap().into(),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(4)).await;
    });
    let client_path = path.clone();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_walkingpad"))
            .args([
                "subscribe",
                "--connection-file",
                client_path.to_str().unwrap(),
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    server.abort();
    std::fs::remove_file(path).unwrap();
    assert!(!output.status.success());
    let packets: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0]["usable_speed_mps"], 1.0);
    assert!(packets[1]["usable_speed_mps"].is_null());
    assert_eq!(packets[1]["connected"], false);
    assert!(packets[1]["field_age_ms"]["speed_mps"].as_u64().unwrap() >= 3000);
}
