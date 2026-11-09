mod bridge;

use std::{
    fs::{File, OpenOptions},
    io::{self, BufReader, BufWriter, Write},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use uuid::Uuid;
use walkingpad::{
    ble::{BleClient, CaptureEvent},
    device::{FTMS_SERVICE, TREADMILL_DATA},
    kingsmith,
    protocol::decode_ftms_treadmill,
    recording::{CaptureRecord, NotificationRecord, RecordingReader, write_record},
    telemetry::{Quality, TelemetrySession, TelemetryUpdate},
};

#[derive(Parser)]
#[command(
    version,
    about = "WalkingPad telemetry with opt-in bounded speed bonuses",
    long_about = "Read telemetry by default. Bridge --allow-speed-control enables bounded +/-0.1 km/h requests on an already-running belt. No start/stop or mode commands."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve live BLE or simulated telemetry over an authenticated loopback WebSocket.
    Bridge {
        /// Explicitly select synthetic input (never used as a live-input fallback).
        #[arg(long, required_unless_present = "device", conflicts_with = "device")]
        simulate: bool,
        /// Exact device ID or unique name substring for live telemetry.
        #[arg(long, required_unless_present = "simulate")]
        device: Option<String>,
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u64).range(1..=30))]
        scan_seconds: u64,
        #[arg(long, requires = "simulate", conflicts_with = "device")]
        speed_kmh: Option<f64>,
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..=3600))]
        seconds: u64,
        /// New private file containing the local endpoint and session token.
        #[arg(long)]
        connection_file: PathBuf,
        /// Add a telemetry gap at 8–12s and a stopped belt at 12–15s.
        #[arg(long, requires = "simulate", conflicts_with = "device")]
        demo_faults: bool,
        /// Enable authenticated +/-0.1 km/h requests on an already-running belt.
        #[arg(long)]
        allow_speed_control: bool,
        #[arg(long, default_value_t = 1.0)]
        min_speed_kmh: f64,
        #[arg(long, default_value_t = 5.0)]
        max_speed_kmh: f64,
        /// Record live notification bytes for local diagnostics (may contain device IDs).
        #[arg(long, requires = "device", conflicts_with = "simulate")]
        record: Option<PathBuf>,
    },
    /// Consume a local bridge connection file and print live JSON snapshots.
    Subscribe {
        #[arg(long)]
        connection_file: PathBuf,
    },
    /// Discover nearby walking-pad candidates. Does not connect.
    Scan {
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=30))]
        seconds: u64,
        /// Show all BLE advertisements instead of just treadmill candidates.
        #[arg(long)]
        all: bool,
    },
    /// Connect, enumerate GATT services, and disconnect. No telemetry commands.
    Inspect {
        #[command(flatten)]
        device: DeviceArgs,
        /// Save the profile to a new file; existing files are never overwritten.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Record raw notifications in JSONL. Passive unless --telemetry is selected.
    Capture {
        #[command(flatten)]
        device: DeviceArgs,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        seconds: u64,
        #[arg(long, requires = "characteristic")]
        service: Option<Uuid>,
        #[arg(long, requires = "service")]
        characteristic: Option<Uuid>,
        /// Enable the driver's read-only handshake/property requests.
        #[arg(long, conflicts_with_all = ["service", "characteristic"])]
        telemetry: bool,
    },
    /// Display normalized live telemetry, using read-only queries where needed.
    Watch {
        #[command(flatten)]
        device: DeviceArgs,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        seconds: u64,
    },
    /// Decode one standard FTMS Treadmill Data payload (hex), without Bluetooth.
    Decode { hex: String },
    /// Replay a capture offline, preserving field ages and source provenance.
    Replay { file: PathBuf },
    /// Emit a short synthetic session, including a stale-data gap and a stop.
    Simulate {
        #[arg(long, default_value_t = 3.0)]
        speed_kmh: f64,
    },
}

#[derive(Args)]
struct DeviceArgs {
    /// Exact ID from scan, or a unique device-name substring such as KS-X21.
    #[arg(long)]
    device: String,
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u64).range(1..=30))]
    scan_seconds: u64,
}

// The inspected X21 reports about every two seconds; permit one second of jitter.
const STALE_AFTER: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Bridge {
            simulate: _,
            device,
            scan_seconds,
            speed_kmh,
            seconds,
            connection_file,
            demo_faults,
            allow_speed_control,
            min_speed_kmh,
            max_speed_kmh,
            record,
        } => {
            let limits = if allow_speed_control {
                Some(
                    walkingpad::control::SpeedLimits::new(min_speed_kmh, max_speed_kmh)
                        .map_err(anyhow::Error::msg)?,
                )
            } else {
                None
            };
            if let Some(device) = device {
                bridge::serve_live(
                    connection_file,
                    device,
                    scan_seconds,
                    seconds,
                    limits,
                    record,
                )
                .await?;
            } else {
                bridge::serve(
                    connection_file,
                    speed_kmh.unwrap_or(3.0),
                    seconds,
                    demo_faults,
                    limits,
                )
                .await?;
            }
        }
        Command::Subscribe { connection_file } => bridge::subscribe(connection_file).await?,
        Command::Scan { seconds, all } => {
            let client = BleClient::new().await?;
            let devices = client.scan(Duration::from_secs(seconds)).await?;
            let devices: Vec<_> = devices
                .into_iter()
                .filter(|device| all || device.is_treadmill_candidate())
                .collect();
            if devices.is_empty() {
                eprintln!(
                    "No matching advertisements. Check power, Bluetooth permissions, and other connected apps. Use --all to inspect unrecognized device names."
                );
            }
            print_json(&devices)?;
        }
        Command::Inspect { device, output } => {
            let client = BleClient::new().await?;
            let connection = client
                .connect(&device.device, Duration::from_secs(device.scan_seconds))
                .await?;
            let result = (|| -> Result<()> {
                print_json(&connection.profile)?;
                if let Some(path) = output {
                    let mut file = new_file(&path)?;
                    serde_json::to_writer_pretty(&mut file, &connection.profile)?;
                    writeln!(file)?;
                }
                Ok(())
            })();
            let cleanup = connection.disconnect().await;
            result?;
            cleanup?;
        }
        Command::Capture {
            device,
            output,
            seconds,
            service,
            characteristic,
            telemetry,
        } => {
            capture(
                device,
                seconds,
                Some(output),
                service,
                characteristic,
                telemetry,
            )
            .await?;
        }
        Command::Watch { device, seconds } => {
            capture(device, seconds, None, None, None, true).await?;
        }
        Command::Decode { hex: data } => {
            let bytes = hex::decode(data.trim()).context("payload must be hexadecimal")?;
            print_json(&decode_ftms_treadmill(&bytes)?)?;
        }
        Command::Replay { file } => replay(file)?,
        Command::Simulate { speed_kmh } => simulate(speed_kmh)?,
    }
    Ok(())
}

fn new_file(path: &PathBuf) -> Result<BufWriter<File>> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "cannot create {}; create its parent directory first, and choose a new filename",
                path.display()
            )
        })?;
    Ok(BufWriter::new(file))
}

fn print_json(value: &impl Serialize) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

#[derive(Default)]
struct PacketDecoder {
    kingsmith: kingsmith::Decoder,
}

impl PacketDecoder {
    fn apply(
        &mut self,
        packet: &NotificationRecord,
        session: &mut TelemetrySession,
    ) -> Result<bool> {
        let bytes = hex::decode(&packet.payload_hex)?;
        let at = Duration::from_millis(packet.elapsed_ms);
        if packet.service_uuid == FTMS_SERVICE && packet.characteristic_uuid == TREADMILL_DATA {
            session.apply(decode_ftms_treadmill(&bytes)?, at)?;
        } else if kingsmith::channels(packet.service_uuid)
            .is_some_and(|(_, notify)| notify == packet.characteristic_uuid)
        {
            for report in self.kingsmith.feed(&bytes) {
                match report {
                    Ok(update) => session.apply(update, at)?,
                    Err(error) => eprintln!(
                        "Undecoded KingSmith frame at {}ms: {error}",
                        packet.elapsed_ms
                    ),
                }
            }
        } else {
            return Ok(false);
        }
        Ok(true)
    }
}

async fn capture(
    device: DeviceArgs,
    seconds: u64,
    output: Option<PathBuf>,
    service: Option<Uuid>,
    characteristic: Option<Uuid>,
    telemetry: bool,
) -> Result<()> {
    // Check output creation before acquiring hardware; never truncate a prior capture.
    let mut writer = output.as_ref().map(new_file).transpose()?;
    let client = BleClient::new().await?;
    let connection = client
        .connect(&device.device, Duration::from_secs(device.scan_seconds))
        .await?;
    let mut session = TelemetrySession::new(Quality::DeviceObservation);
    let mut last_status = None;
    let mut packets = 0u64;
    let mut decoder = PacketDecoder::default();
    let result = async {
        let characteristic = connection.notification_characteristic(service, characteristic)?;
        if let Some(file) = writer.as_mut() {
            write_record(file, &CaptureRecord::Profile { format_version: 1, profile: connection.profile.clone(), quality: Quality::DeviceObservation })?;
        }
        eprintln!("Connected to {}. Reading notifications for up to {seconds}s; Ctrl-C disconnects. No motor commands are sent.", connection.profile.device.name.as_deref().unwrap_or("selected device"));
        let cancel = async { let _ = tokio::signal::ctrl_c().await; };
        let on_event = |event| {
            let result = (|| -> Result<()> {
                let elapsed_ms = match event {
                    CaptureEvent::Notification(packet) => {
                        packets += 1;
                        if let Some(file) = writer.as_mut() {
                            write_record(file, &CaptureRecord::Notification { packet: packet.clone() })?;
                        }
                        match decoder.apply(&packet, &mut session) {
                            Ok(true) => {}
                            Ok(false) => { print_json(&CaptureRecord::Notification { packet: packet.clone() })?; }
                            Err(error) => eprintln!("Undecoded packet at {}ms: {error}", packet.elapsed_ms),
                        }
                        packet.elapsed_ms
                    }
                    CaptureEvent::Tick { elapsed_ms } => elapsed_ms,
                };
                let snapshot = session.snapshot(Duration::from_millis(elapsed_ms), STALE_AFTER);
                let status = (snapshot.sequence, snapshot.stale);
                if last_status != Some(status) {
                    print_json(&snapshot)?;
                    last_status = Some(status);
                }
                Ok(())
            })();
            result.map_err(|error| error.to_string())
        };
        let outcome = if telemetry {
            connection.monitor(Duration::from_secs(seconds), cancel, on_event).await?
        } else {
            connection.capture(&characteristic, Duration::from_secs(seconds), cancel, on_event).await?
        };
        if let Some(file) = writer.as_mut() {
            write_record(file, &CaptureRecord::End { elapsed_ms: outcome.elapsed_ms, reason: outcome.reason.into() })?;
        }
        session.disconnect();
        print_json(&session.snapshot(Duration::from_millis(outcome.elapsed_ms), STALE_AFTER))?;
        eprintln!("Capture ended: {}; {packets} notifications received.", outcome.reason);
        if packets == 0 { eprintln!("No notifications received. Check the driver, device state and app contention. Passive capture sends no handshake; --telemetry enables read-only queries."); }
        Ok::<_, anyhow::Error>(())
    }.await;
    let cleanup = connection.disconnect().await;
    result?;
    cleanup?;
    Ok(())
}

fn replay(path: PathBuf) -> Result<()> {
    let file = File::open(&path).with_context(|| format!("cannot open {}", path.display()))?;
    let reader = RecordingReader::new(BufReader::new(file));
    let mut session = None;
    let mut last_elapsed = 0;
    let mut ended = false;
    let mut decoder = PacketDecoder::default();
    for record in reader {
        match record? {
            CaptureRecord::Profile {
                quality, profile, ..
            } => {
                eprintln!(
                    "Replaying {} ({quality:?}); this is recorded input, not live hardware.",
                    profile.device.name.as_deref().unwrap_or("device")
                );
                session = Some(TelemetrySession::from_recording(quality));
            }
            CaptureRecord::Notification { packet } => {
                last_elapsed = packet.elapsed_ms;
                let session = session.as_mut().context("missing recording header")?;
                match decoder.apply(&packet, session) {
                    Ok(true) => print_json(
                        &session.snapshot(Duration::from_millis(last_elapsed), STALE_AFTER),
                    )?,
                    Ok(false) => print_json(&CaptureRecord::Notification { packet })?,
                    Err(error) => {
                        eprintln!("Undecoded packet at {last_elapsed}ms: {error}");
                        print_json(
                            &session.snapshot(Duration::from_millis(last_elapsed), STALE_AFTER),
                        )?;
                    }
                }
            }
            CaptureRecord::End { elapsed_ms, .. } => {
                last_elapsed = elapsed_ms;
                let session = session.as_mut().context("missing recording header")?;
                session.disconnect();
                print_json(&session.snapshot(Duration::from_millis(last_elapsed), STALE_AFTER))?;
                ended = true;
            }
        }
    }
    if !ended {
        eprintln!("Recording has no end marker; treating EOF as a disconnected input.");
        if let Some(mut session) = session {
            session.disconnect();
            print_json(&session.snapshot(Duration::from_millis(last_elapsed), STALE_AFTER))?;
        }
    }
    Ok(())
}

fn simulate(speed_kmh: f64) -> Result<()> {
    if !speed_kmh.is_finite() || !(0.0..=20.0).contains(&speed_kmh) {
        bail!("simulation speed must be finite and between 0 and 20 km/h");
    }
    let mut session = TelemetrySession::new(Quality::Simulated);
    session.apply(
        TelemetryUpdate {
            speed_mps: Some(speed_kmh / 3.6),
            ..Default::default()
        },
        Duration::ZERO,
    )?;
    for elapsed in [0, 1000, 2000, 3000] {
        print_json(&session.snapshot(Duration::from_millis(elapsed), STALE_AFTER))?;
    }
    session.apply(
        TelemetryUpdate {
            speed_mps: Some(0.0),
            ..Default::default()
        },
        Duration::from_secs(4),
    )?;
    print_json(&session.snapshot(Duration::from_secs(4), STALE_AFTER))?;
    session.disconnect();
    print_json(&session.snapshot(Duration::from_secs(5), STALE_AFTER))?;
    Ok(())
}
