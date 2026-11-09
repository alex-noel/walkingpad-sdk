use btleplug::{
    api::{
        Central, CentralEvent, CentralState, CharPropFlags, Characteristic, Manager as _,
        Peripheral as _, ScanFilter, WriteType,
    },
    platform::{Adapter, Manager, Peripheral},
};
use futures::StreamExt;
use std::{
    future::Future,
    time::{Duration, Instant},
};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    control::transport::SpeedControlHost,
    device::{
        CharacteristicInfo, CharacteristicProperties, DeviceProfile, DeviceSummary, FTMS_SERVICE,
        ServiceInfo, TREADMILL_DATA,
    },
    kingsmith::{self, Alphabet, ReadQuery},
    recording::NotificationRecord,
    telemetry::millis,
};

const OPERATION_TIMEOUT: Duration = Duration::from_secs(12);
// KingSmith's line receiver needs time between ATT fragments and complete lines.
// Timing follows the documented BeltControl transport; see protocol-notes.md.
const KINGSMITH_FRAGMENT_GAP: Duration = Duration::from_millis(30);
const KINGSMITH_LINE_GAP: Duration = Duration::from_millis(60);

#[derive(Debug, Error)]
pub enum BleError {
    #[error(
        "Bluetooth error: {0}. Check Bluetooth permission and disconnect other treadmill apps."
    )]
    Backend(#[from] btleplug::Error),
    #[error("Bluetooth operation timed out: {0}; check permissions, power, and app contention")]
    Timeout(&'static str),
    #[error("no Bluetooth adapter is available")]
    NoAdapter,
    #[error("Bluetooth is switched off")]
    PoweredOff,
    #[error("no device matches '{0}'; run scan and use its exact ID")]
    NotFound(String),
    #[error("multiple devices match '{0}'; select an exact ID from scan")]
    Ambiguous(String),
    #[error(
        "no matching subscribable characteristic; inspect the device and specify both service and characteristic UUIDs for raw capture"
    )]
    NoCharacteristic,
    #[error("capture consumer failed: {0}")]
    Consumer(String),
    #[error("notification stream ended unexpectedly")]
    StreamEnded,
    #[error("KingSmith handshake did not identify a supported alphabet; no property queries sent")]
    UnsupportedAlphabet,
}

async fn bounded<T>(
    operation: &'static str,
    future: impl Future<Output = Result<T, btleplug::Error>>,
) -> Result<T, BleError> {
    tokio::time::timeout(OPERATION_TIMEOUT, future)
        .await
        .map_err(|_| BleError::Timeout(operation))?
        .map_err(Into::into)
}

pub struct BleClient {
    adapter: Adapter,
}

impl BleClient {
    pub async fn new() -> Result<Self, BleError> {
        let manager = bounded("create Bluetooth manager", Manager::new()).await?;
        let adapter = bounded("enumerate adapters", manager.adapters())
            .await?
            .into_iter()
            .next()
            .ok_or(BleError::NoAdapter)?;
        if bounded("read adapter state", adapter.adapter_state()).await? == CentralState::PoweredOff
        {
            return Err(BleError::PoweredOff);
        }
        Ok(Self { adapter })
    }

    pub async fn scan(&self, duration: Duration) -> Result<Vec<DeviceSummary>, BleError> {
        bounded("start scan", self.adapter.start_scan(ScanFilter::default())).await?;
        tokio::time::sleep(duration).await;
        bounded("stop scan", self.adapter.stop_scan()).await?;
        let mut devices = Vec::new();
        for peripheral in bounded("list peripherals", self.adapter.peripherals()).await? {
            devices.push(summary(&peripheral).await?);
        }
        devices.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        Ok(devices)
    }

    /// Select by exact OS ID or a unique case-insensitive name substring.
    pub async fn connect(
        &self,
        selector: &str,
        scan_duration: Duration,
    ) -> Result<BleConnection, BleError> {
        if selector.trim().is_empty() {
            return Err(BleError::NotFound(selector.into()));
        }
        let devices = self.scan(scan_duration).await?;
        let exact: Vec<_> = devices.iter().filter(|d| d.id == selector).collect();
        let matches: Vec<_> = if exact.is_empty() {
            let needle = selector.to_ascii_lowercase();
            devices
                .iter()
                .filter(|d| {
                    d.name
                        .as_deref()
                        .unwrap_or_default()
                        .to_ascii_lowercase()
                        .contains(&needle)
                })
                .collect()
        } else {
            exact
        };
        let device = match matches.as_slice() {
            [] => return Err(BleError::NotFound(selector.into())),
            [device] => (*device).clone(),
            _ => return Err(BleError::Ambiguous(selector.into())),
        };
        let peripheral = bounded("list peripherals", self.adapter.peripherals())
            .await?
            .into_iter()
            .find(|p| p.id().to_string() == device.id)
            .ok_or_else(|| BleError::NotFound(selector.into()))?;
        let result = async {
            bounded("connect", peripheral.connect()).await?;
            bounded("discover GATT services", peripheral.discover_services()).await?;
            let services = peripheral
                .services()
                .into_iter()
                .map(|service| ServiceInfo {
                    uuid: service.uuid,
                    primary: service.primary,
                    characteristics: service
                        .characteristics
                        .into_iter()
                        .map(|c| CharacteristicInfo {
                            uuid: c.uuid,
                            properties: CharacteristicProperties {
                                read: c.properties.contains(CharPropFlags::READ),
                                write: c.properties.contains(CharPropFlags::WRITE),
                                write_without_response: c
                                    .properties
                                    .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE),
                                notify: c.properties.contains(CharPropFlags::NOTIFY),
                                indicate: c.properties.contains(CharPropFlags::INDICATE),
                            },
                        })
                        .collect(),
                })
                .collect();
            Ok(DeviceProfile::new(device, services))
        }
        .await;
        match result {
            Ok(profile) => Ok(BleConnection {
                adapter: self.adapter.clone(),
                peripheral,
                profile,
            }),
            Err(error) => {
                let _ = bounded("disconnect after failure", peripheral.disconnect()).await;
                Err(error)
            }
        }
    }
}

async fn summary(peripheral: &Peripheral) -> Result<DeviceSummary, BleError> {
    let properties = bounded("read advertising properties", peripheral.properties()).await?;
    Ok(DeviceSummary {
        id: peripheral.id().to_string(),
        name: properties.as_ref().and_then(|p| {
            p.advertisement_name
                .clone()
                .or_else(|| p.local_name.clone())
        }),
        rssi: properties.as_ref().and_then(|p| p.rssi),
        advertised_services: properties.map(|p| p.services).unwrap_or_default(),
    })
}

pub struct BleConnection {
    adapter: Adapter,
    peripheral: Peripheral,
    pub profile: DeviceProfile,
}

pub enum CaptureEvent {
    Notification(NotificationRecord),
    Tick { elapsed_ms: u64 },
}

pub struct CaptureOutcome {
    pub elapsed_ms: u64,
    pub reason: &'static str,
}

impl BleConnection {
    pub async fn disconnect(&self) -> Result<(), BleError> {
        bounded("disconnect", self.peripheral.disconnect()).await
    }

    pub fn notification_characteristic(
        &self,
        service: Option<Uuid>,
        characteristic: Option<Uuid>,
    ) -> Result<Characteristic, BleError> {
        let (service, characteristic) = match (service, characteristic) {
            (None, None) => {
                let available = self.peripheral.characteristics();
                if available
                    .iter()
                    .any(|c| c.service_uuid == FTMS_SERVICE && c.uuid == TREADMILL_DATA)
                {
                    (FTMS_SERVICE, TREADMILL_DATA)
                } else {
                    available
                        .iter()
                        .find_map(|c| {
                            kingsmith::channels(c.service_uuid)
                                .filter(|(_, notify)| *notify == c.uuid)
                                .map(|(_, notify)| (c.service_uuid, notify))
                        })
                        .ok_or(BleError::NoCharacteristic)?
                }
            }
            (Some(s), Some(c)) => (s, c),
            _ => return Err(BleError::NoCharacteristic),
        };
        self.peripheral
            .characteristics()
            .into_iter()
            .find(|c| {
                c.service_uuid == service
                    && c.uuid == characteristic
                    && c.properties
                        .intersects(CharPropFlags::NOTIFY | CharPropFlags::INDICATE)
            })
            .ok_or(BleError::NoCharacteristic)
    }

    /// Subscribe only; sends no application-level characteristic writes.
    /// Notification subscription itself can write the standard CCC descriptor.
    pub async fn capture<C, F>(
        &self,
        characteristic: &Characteristic,
        duration: Duration,
        cancel: C,
        on_event: F,
    ) -> Result<CaptureOutcome, BleError>
    where
        C: Future<Output = ()>,
        F: FnMut(CaptureEvent) -> Result<(), String>,
    {
        self.capture_inner(characteristic, None, duration, cancel, on_event, None)
            .await
    }

    /// Read telemetry, using only an allowlisted handshake and property queries
    /// for KingSmith text devices. Never writes motor, mode, speed, or clock settings.
    pub async fn monitor<C, F>(
        &self,
        duration: Duration,
        cancel: C,
        on_event: F,
    ) -> Result<CaptureOutcome, BleError>
    where
        C: Future<Output = ()>,
        F: FnMut(CaptureEvent) -> Result<(), String>,
    {
        self.monitor_with_speed_control(duration, cancel, on_event, None)
            .await
    }

    /// Opt-in bounded +/-0.1 km/h requests, serialized with telemetry polling.
    /// No start/stop, mode switch, automatic retry, or speed restoration.
    pub async fn monitor_with_speed_control<C, F>(
        &self,
        duration: Duration,
        cancel: C,
        on_event: F,
        controls: Option<SpeedControlHost>,
    ) -> Result<CaptureOutcome, BleError>
    where
        C: Future<Output = ()>,
        F: FnMut(CaptureEvent) -> Result<(), String>,
    {
        let characteristic = self.notification_characteristic(None, None)?;
        let query_channel =
            if let Some((write, _)) = kingsmith::channels(characteristic.service_uuid) {
                Some(
                    self.peripheral
                        .characteristics()
                        .into_iter()
                        .find(|c| {
                            c.service_uuid == characteristic.service_uuid
                                && c.uuid == write
                                && c.properties.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
                        })
                        .ok_or(BleError::NoCharacteristic)?,
                )
            } else {
                None
            };
        self.capture_inner(
            &characteristic,
            query_channel.as_ref(),
            duration,
            cancel,
            on_event,
            controls,
        )
        .await
    }

    async fn capture_inner<C, F>(
        &self,
        characteristic: &Characteristic,
        query_channel: Option<&Characteristic>,
        duration: Duration,
        cancel: C,
        mut on_event: F,
        mut controls: Option<SpeedControlHost>,
    ) -> Result<CaptureOutcome, BleError>
    where
        C: Future<Output = ()>,
        F: FnMut(CaptureEvent) -> Result<(), String>,
    {
        let mut notifications = bounded(
            "create notification stream",
            self.peripheral.notifications(),
        )
        .await?;
        let mut adapter_events =
            bounded("create adapter event stream", self.adapter.events()).await?;
        bounded(
            "subscribe to telemetry",
            self.peripheral.subscribe(characteristic),
        )
        .await?;
        let started = Instant::now();
        let deadline = tokio::time::sleep(duration);
        tokio::pin!(deadline, cancel);
        let mut ticks = tokio::time::interval(Duration::from_millis(250));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut queries = tokio::time::interval(Duration::from_secs(1));
        queries.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut handshake_attempts = 0;
        let mut query_decoder = kingsmith::Decoder::default();
        let mut initialization_step = 0;
        let initialization = [
            ReadQuery::Network,
            ReadQuery::DeviceName,
            ReadQuery::ProductKey,
            ReadQuery::Version,
            ReadQuery::Configuration,
        ];
        let result = async {
            loop {
                tokio::select! {
                    _ = &mut deadline => break Ok(CaptureOutcome { elapsed_ms: millis(started.elapsed()), reason: "duration_complete" }),
                    _ = &mut cancel => break Ok(CaptureOutcome { elapsed_ms: millis(started.elapsed()), reason: "interrupted" }),
                    Some(work) = async {
                        match controls.as_mut() {
                            Some(host) => host.requests.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        // Refresh ages at execution time, even after a slow BLE read.
                        on_event(CaptureEvent::Tick { elapsed_ms: millis(started.elapsed()) }).map_err(BleError::Consumer)?;
                        let host = controls.as_mut().expect("command receiver");
                        if let Some(target) = host.begin(work, started.elapsed()) {
                            if let (Some(channel), Some(alphabet)) = (query_channel, query_decoder.alphabet()) {
                                let bytes = kingsmith::encode_speed_target(target, alphabet).map_err(|e| BleError::Consumer(e.into()))?;
                                let write = self.write_kingsmith_line(channel, &bytes);
                                if !matches!(tokio::time::timeout(Duration::from_secs(2), write).await, Ok(Ok(()))) {
                                    host.fail("Speed write uncertain; control locked. No retry sent");
                                    // A partial line must not be followed by another command.
                                    break Err(BleError::Consumer("Speed write failed; reconnect required".into()));
                                }
                            } else { host.fail("This device has no verified KingSmith speed channel"); }
                        }
                    }
                    _ = queries.tick(), if query_channel.is_some() => {
                        let (query, alphabet) = if let Some(alphabet) = query_decoder.alphabet() {
                            let query = initialization.get(initialization_step).copied().unwrap_or(ReadQuery::Properties);
                            initialization_step = initialization_step.saturating_add(1);
                            (query, alphabet)
                        } else {
                            if handshake_attempts >= 5 { break Err(BleError::UnsupportedAlphabet); }
                            handshake_attempts += 1;
                            (ReadQuery::Handshake, Alphabet::V1)
                        };
                        bounded("read-only KingSmith query", self.write_kingsmith_line(
                            query_channel.expect("guarded query channel"), &query.encode(alphabet),
                        )).await?;
                    }
                    event = adapter_events.next() => {
                        match event {
                            Some(CentralEvent::DeviceDisconnected(id)) if id == self.peripheral.id() =>
                                break Ok(CaptureOutcome { elapsed_ms: millis(started.elapsed()), reason: "disconnected" }),
                            None => break Err(BleError::StreamEnded),
                            _ => {}
                        }
                    }
                    notification = notifications.next() => {
                        let notification = notification.ok_or(BleError::StreamEnded)?;
                        if notification.uuid == characteristic.uuid && notification.service_uuid == characteristic.service_uuid {
                            if query_channel.is_some() { let _ = query_decoder.feed(&notification.value); }
                            on_event(CaptureEvent::Notification(NotificationRecord {
                                elapsed_ms: millis(started.elapsed()), service_uuid: notification.service_uuid,
                                characteristic_uuid: notification.uuid, payload_hex: hex::encode(notification.value),
                            })).map_err(BleError::Consumer)?;
                        }
                    }
                    _ = ticks.tick() => {
                        on_event(CaptureEvent::Tick { elapsed_ms: millis(started.elapsed()) }).map_err(BleError::Consumer)?;
                        if let Some(host) = controls.as_mut() { host.observe(started.elapsed()); }
                    }
                }
            }
        }.await;
        let cleanup = bounded("unsubscribe", self.peripheral.unsubscribe(characteristic)).await;
        match result {
            Err(error) => Err(error),
            Ok(outcome) if outcome.reason == "disconnected" => Ok(outcome),
            Ok(outcome) => {
                cleanup?;
                Ok(outcome)
            }
        }
    }

    async fn write_kingsmith_line(
        &self,
        channel: &Characteristic,
        bytes: &[u8],
    ) -> Result<(), btleplug::Error> {
        for chunk in bytes.chunks(16) {
            self.peripheral
                .write(channel, chunk, WriteType::WithoutResponse)
                .await?;
            tokio::time::sleep(KINGSMITH_FRAGMENT_GAP).await;
        }
        tokio::time::sleep(KINGSMITH_LINE_GAP).await;
        Ok(())
    }
}
