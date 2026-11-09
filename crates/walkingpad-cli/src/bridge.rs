use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, watch},
    task::JoinSet,
};
use tokio_tungstenite::{
    accept_hdr_async_with_config, connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        handshake::server::{Request, Response},
    },
};
use uuid::Uuid;
use walkingpad::telemetry::{
    BeltState, InputSource, Quality, TelemetrySession, TelemetrySnapshot, TelemetryUpdate,
};

use crate::STALE_AFTER;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use walkingpad::ble::{BleClient, CaptureEvent};
use walkingpad::control::{
    SpeedLimits, SpeedReply, SpeedRequest,
    transport::{SpeedControlHost, SpeedWork},
};
use walkingpad::recording::{CaptureRecord, write_record};

#[derive(Serialize, Deserialize)]
pub struct ConnectionDetails {
    pub schema_version: u8,
    pub url: String,
    pub token: String,
    pub input_source: InputSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_control: Option<SpeedLimits>,
}

/// Remove only the exact credentials file this process created.
struct ConnectionFile {
    path: PathBuf,
    contents: Vec<u8>,
}
impl ConnectionFile {
    fn create(path: PathBuf, details: &ConnectionDetails) -> Result<Self> {
        let contents = serde_json::to_vec_pretty(details)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).with_context(|| {
            format!(
                "cannot create {}; create its parent directory and choose a new filename",
                path.display()
            )
        })?;
        if let Err(error) = file.write_all(&contents).and_then(|_| file.flush()) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(error.into());
        }
        Ok(Self { path, contents })
    }
}
impl Drop for ConnectionFile {
    fn drop(&mut self) {
        if fs::read(&self.path).is_ok_and(|bytes| bytes == self.contents) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub async fn serve_live(
    path: PathBuf,
    device: String,
    scan_seconds: u64,
    seconds: u64,
    limits: Option<SpeedLimits>,
    recording_path: Option<PathBuf>,
) -> Result<()> {
    let mut recording = recording_path.as_ref().map(crate::new_file).transpose()?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let details = ConnectionDetails {
        schema_version: 1,
        url: format!("ws://{}/telemetry", listener.local_addr()?),
        token: Uuid::new_v4().simple().to_string(),
        input_source: InputSource::LiveDevice,
        speed_control: limits,
    };
    let _file = ConnectionFile::create(path, &details)?;
    let connection = BleClient::new()
        .await?
        .connect(&device, Duration::from_secs(scan_seconds))
        .await?;
    let started = Instant::now();
    if let Some(file) = recording.as_mut() {
        write_record(
            file,
            &CaptureRecord::Profile {
                format_version: 1,
                profile: connection.profile.clone(),
                quality: Quality::DeviceObservation,
            },
        )?;
    }
    let mut session = TelemetrySession::new(Quality::DeviceObservation);
    let (sender, receiver) = watch::channel(session.snapshot(Duration::ZERO, STALE_AFTER));
    let (command_tx, command_rx) = mpsc::channel(8);
    let controls = limits.map(|limits| SpeedControlHost::new(limits, command_rx, receiver.clone()));
    let command_tx = limits.map(|_| command_tx);
    let mut decoder = crate::PacketDecoder::default();
    let mut clients = JoinSet::new();
    eprintln!(
        "Live telemetry bridge connected to {}. Ctrl-C disconnects.",
        connection
            .profile
            .device
            .name
            .as_deref()
            .unwrap_or("selected device")
    );
    let result = {
        let monitor = connection.monitor_with_speed_control(
            Duration::from_secs(seconds),
            async {
                let _ = tokio::signal::ctrl_c().await;
            },
            |event| {
                let elapsed_ms = match event {
                    CaptureEvent::Notification(packet) => {
                        if let Some(file) = recording.as_mut() {
                            write_record(
                                file,
                                &CaptureRecord::Notification {
                                    packet: packet.clone(),
                                },
                            )
                            .map_err(|error| error.to_string())?;
                        }
                        if let Err(error) = decoder.apply(&packet, &mut session) {
                            eprintln!("Undecoded packet at {}ms: {error}", packet.elapsed_ms);
                        }
                        packet.elapsed_ms
                    }
                    CaptureEvent::Tick { elapsed_ms } => elapsed_ms,
                };
                sender
                    .send_replace(session.snapshot(Duration::from_millis(elapsed_ms), STALE_AFTER));
                Ok(())
            },
            controls,
        );
        tokio::pin!(monitor);
        loop {
            tokio::select! {
                result = &mut monitor => break result.map(|_| ()).map_err(anyhow::Error::from),
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _)) if clients.len() < 16 => {
                            let token = details.token.clone();
                            let receiver = receiver.clone();
                            let commands = command_tx.clone();
                            clients.spawn(async move { let _ = client_loop(stream, token, receiver, commands).await; });
                        }
                        Ok(_) => {},
                        Err(error) => break Err(error.into()),
                    }
                }
                Some(_) = clients.join_next(), if !clients.is_empty() => {},
            }
        }
    };
    session.disconnect();
    sender.send_replace(session.snapshot(started.elapsed(), STALE_AFTER));
    let cleanup = connection.disconnect().await;
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while clients.join_next().await.is_some() {}
    })
    .await;
    clients.abort_all();
    if let Some(file) = recording.as_mut() {
        write_record(
            file,
            &CaptureRecord::End {
                elapsed_ms: walkingpad::telemetry::millis(started.elapsed()),
                reason: if result.is_ok() {
                    "bridge_ended"
                } else {
                    "bridge_error"
                }
                .into(),
            },
        )?;
    }
    result?;
    cleanup?;
    Ok(())
}

pub async fn serve(
    path: PathBuf,
    mut speed_kmh: f64,
    seconds: u64,
    demo_faults: bool,
    limits: Option<SpeedLimits>,
) -> Result<()> {
    if !speed_kmh.is_finite() || !(0.0..=20.0).contains(&speed_kmh) {
        bail!("simulation speed must be finite and between 0 and 20 km/h");
    }
    // No configurable external bind address: LAN access is a separate milestone.
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let details = ConnectionDetails {
        schema_version: 1,
        url: format!("ws://{}/telemetry", listener.local_addr()?),
        token: Uuid::new_v4().simple().to_string(),
        input_source: InputSource::Simulation,
        speed_control: limits,
    };
    let _file = ConnectionFile::create(path.clone(), &details)?;
    eprintln!(
        "Simulated telemetry bridge ready at {}. Credentials are in {}. Ctrl-C ends the session.",
        details.url,
        path.display()
    );
    let started = Instant::now();
    let mut previous = Duration::ZERO;
    let mut distance = 0.0;
    let mut session = TelemetrySession::new(Quality::Simulated);
    let (sender, receiver) = watch::channel(session.snapshot(Duration::ZERO, STALE_AFTER));
    let (command_tx, command_rx) = mpsc::channel(8);
    let mut controls =
        limits.map(|limits| SpeedControlHost::new(limits, command_rx, receiver.clone()));
    let command_tx = limits.map(|_| command_tx);
    let mut clients = JoinSet::new();
    let mut ticks = tokio::time::interval(Duration::from_millis(250));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    let cancel = tokio::signal::ctrl_c();
    tokio::pin!(deadline, cancel);
    let result = async {
        loop {
            tokio::select! {
                _ = &mut deadline => break Ok::<_, anyhow::Error>(()),
                _ = &mut cancel => break Ok(()),
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    if clients.len() < 16 {
                        let token = details.token.clone();
                        let receiver = receiver.clone();
                        let commands = command_tx.clone();
                        clients.spawn(async move { let _ = client_loop(stream, token, receiver, commands).await; });
                    }
                }
                Some(_) = clients.join_next(), if !clients.is_empty() => {},
                Some(work) = async {
                    match controls.as_mut() { Some(host) => host.requests.recv().await, None => std::future::pending().await }
                } => {
                    sender.send_replace(session.snapshot(started.elapsed(), STALE_AFTER));
                    if let Some(target) = controls.as_mut().unwrap().begin(work, started.elapsed()) {
                        speed_kmh = f64::from(target) / 10.0;
                    }
                }
                _ = ticks.tick() => {
                    let elapsed = started.elapsed();
                    let at = elapsed.as_secs_f64();
                    let stopped = demo_faults && (12.0..15.0).contains(&at);
                    let speed = if stopped { 0.0 } else { speed_kmh / 3.6 };
                    distance += speed * elapsed.saturating_sub(previous).as_secs_f64();
                    previous = elapsed;
                    if !(demo_faults && (8.0..12.0).contains(&at)) {
                        session.apply(TelemetryUpdate {
                            speed_mps: Some(speed), distance_m: Some(distance.floor() as u64),
                            elapsed_s: Some(elapsed.as_secs()),
                            belt_state: Some(if speed == 0.0 { BeltState::Stopped } else { BeltState::Running }),
                            ..Default::default()
                        }, elapsed)?;
                    }
                    // watch keeps only the latest snapshot for slow subscribers.
                    sender.send_replace(session.snapshot(elapsed, STALE_AFTER));
                    if let Some(host) = controls.as_mut() { host.observe(elapsed); }
                }
            }
        }
    }.await;
    session.disconnect();
    sender.send_replace(session.snapshot(started.elapsed(), STALE_AFTER));
    // Allow subscribers to receive the final invalidation, then bound shutdown.
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while clients.join_next().await.is_some() {}
    })
    .await;
    clients.abort_all();
    result
}

#[allow(
    clippy::result_large_err,
    reason = "Tungstenite requires an unboxed HTTP error response in its handshake callback"
)]
async fn client_loop(
    stream: TcpStream,
    token: String,
    mut receiver: watch::Receiver<TelemetrySnapshot>,
    commands: Option<mpsc::Sender<SpeedWork>>,
) -> Result<()> {
    let expected = format!("Bearer {token}");
    let is_control = Arc::new(AtomicBool::new(false));
    let route = is_control.clone();
    let control_enabled = commands.is_some();
    let mut config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    config.max_message_size = Some(1024);
    config.max_frame_size = Some(1024);
    let handshake = accept_hdr_async_with_config(
        stream,
        move |request: &Request, response: Response| {
            let authorized = request
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                == Some(expected.as_str());
            let control = request.uri().path() == "/control" && control_enabled;
            if !authorized
                || (request.uri().path() != "/telemetry" && !control)
                || request.uri().query().is_some()
            {
                let mut error = tokio_tungstenite::tungstenite::http::Response::new(Some(
                    "Unauthorized".to_owned(),
                ));
                *error.status_mut() =
                    tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED;
                return Err(error);
            }
            route.store(control, Ordering::SeqCst);
            Ok(response)
        },
        Some(config),
    );
    let mut socket = tokio::time::timeout(Duration::from_secs(3), handshake).await??;
    if is_control.load(Ordering::SeqCst) {
        let incoming = tokio::time::timeout(Duration::from_secs(3), socket.next())
            .await?
            .context("Missing speed request")??;
        let Message::Text(text) = incoming else {
            bail!("Expected a speed request");
        };
        if text.len() > 1024 {
            bail!("Speed request too large");
        }
        let request: SpeedRequest = serde_json::from_str(&text)?;
        let id = request.request_id;
        eprintln!(
            "Speed request {id}: {} tenths from {} tenths (sample {})",
            request.delta_tenths, request.expected_speed_tenths, request.sequence
        );
        let cancelled = Arc::new(AtomicBool::new(false));
        let (reply, mut result) = oneshot::channel();
        let work = SpeedWork {
            request,
            arrived: Instant::now(),
            cancelled: cancelled.clone(),
            reply,
        };
        let response = if commands.unwrap().try_send(work).is_err() {
            SpeedReply::rejected(id, "Bridge busy; request discarded")
        } else {
            let deadline = tokio::time::sleep(Duration::from_secs(11));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    result = &mut result => break result.unwrap_or_else(|_| SpeedReply { request_id: id, status: "unconfirmed".into(), target_kmh: None, message: "Control connection ended before confirmation; do not retry".into(), control_locked: true }),
                    _ = &mut deadline => {
                        cancelled.store(true, Ordering::SeqCst);
                        break SpeedReply { request_id: id, status: "unconfirmed".into(), target_kmh: None, message: "No command result; do not retry automatically".into(), control_locked: true };
                    }
                    incoming = socket.next() => match incoming {
                        Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {},
                        _ => { cancelled.store(true, Ordering::SeqCst); return Ok(()); }
                    }
                }
            }
        };
        eprintln!(
            "Speed result {id}: {} target {:?} — {}",
            response.status, response.target_kmh, response.message
        );
        tokio::time::timeout(
            Duration::from_secs(1),
            socket.send(Message::Text(serde_json::to_string(&response)?.into())),
        )
        .await??;
        let _ = tokio::time::timeout(Duration::from_millis(250), socket.close(None)).await;
        return Ok(());
    }
    loop {
        let snapshot = receiver.borrow_and_update().clone();
        let message = Message::Text(serde_json::to_string(&snapshot)?.into());
        tokio::time::timeout(Duration::from_secs(1), socket.send(message)).await??;
        if !snapshot.connected {
            let _ = tokio::time::timeout(Duration::from_millis(250), socket.close(None)).await;
            return Ok(());
        }
        loop {
            tokio::select! {
                changed = receiver.changed() => { changed?; break; },
                incoming = socket.next() => match incoming {
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Err(error)) => return Err(error.into()),
                    Some(Ok(_)) => {},
                }
            }
        }
    }
}

fn read_details(path: &Path) -> Result<ConnectionDetails> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .context("cannot read bridge connection file")?
        .take(4097)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        bail!("connection file exceeds 4 KiB");
    }
    let details: ConnectionDetails = serde_json::from_slice(&bytes)?;
    // Do not transmit credentials from an edited file to a remote endpoint.
    let request = details.url.as_str().into_client_request()?;
    if details.schema_version != 1
        || request.uri().scheme_str() != Some("ws")
        || request.uri().host() != Some("127.0.0.1")
        || request.uri().path() != "/telemetry"
    {
        bail!("expected a version-1 loopback telemetry connection");
    }
    Ok(details)
}

pub async fn subscribe(path: PathBuf) -> Result<()> {
    let details = read_details(&path)?;
    let mut request = details.url.as_str().into_client_request()?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", details.token).parse()?,
    );
    let (mut socket, _) =
        tokio::time::timeout(Duration::from_secs(5), connect_async(request)).await??;
    let mut last: Option<TelemetrySnapshot> = None;
    let mut received_at = Instant::now();
    let result = async {
        loop {
            let remaining = STALE_AFTER.saturating_sub(received_at.elapsed());
            let incoming = tokio::time::timeout(remaining, socket.next())
                .await
                .context("bridge stopped delivering telemetry")?
                .context("bridge disconnected without a final snapshot")??;
            match incoming {
                Message::Text(text) => {
                    let snapshot: TelemetrySnapshot = serde_json::from_str(&text)?;
                    if snapshot.schema_version != 1 {
                        bail!("unsupported telemetry schema");
                    }
                    if snapshot.input_source != details.input_source {
                        bail!("telemetry source does not match the bridge connection file");
                    }
                    received_at = Instant::now();
                    let ended = !snapshot.connected;
                    last = Some(snapshot.clone());
                    crate::print_json(&snapshot)?;
                    if ended {
                        return Ok::<_, anyhow::Error>(());
                    }
                }
                Message::Close(_) => bail!("bridge closed without a final snapshot"),
                _ => {}
            }
        }
    }
    .await;
    if result.is_err()
        && let Some(mut snapshot) = last
    {
        let age = walkingpad::telemetry::millis(received_at.elapsed());
        snapshot.connected = false;
        snapshot.stale = true;
        snapshot.usable_speed_mps = None;
        snapshot.sample_age_ms = snapshot
            .sample_age_ms
            .map(|value| value.saturating_add(age));
        for field_age in snapshot.field_age_ms.values_mut() {
            *field_age = field_age.saturating_add(age);
        }
        crate::print_json(&snapshot)?;
    }
    result
}
