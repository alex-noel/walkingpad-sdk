use std::{collections::BTreeMap, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BeltState {
    Running,
    Stopped,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    DeviceObservation,
    Simulated,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    LiveDevice,
    Replay,
    Simulation,
}

/// A partial device report. None means this packet did not provide the field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetryUpdate {
    pub speed_mps: Option<f64>,
    pub distance_m: Option<u64>,
    pub elapsed_s: Option<u64>,
    pub steps: Option<u64>,
    pub incline_percent: Option<f64>,
    pub energy_kcal: Option<u64>,
    pub heart_rate_bpm: Option<u16>,
    pub belt_state: Option<BeltState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    pub schema_version: u8,
    pub connection_id: Uuid,
    pub sequence: u64,
    pub connected: bool,
    pub quality: Quality,
    pub input_source: InputSource,
    #[serde(flatten)]
    pub metrics: TelemetryUpdate,
    /// Device values are not assumed to be independently measured belt velocity.
    pub speed_semantics: String,
    pub sample_age_ms: Option<u64>,
    pub field_age_ms: BTreeMap<String, u64>,
    pub stale: bool,
    /// Consumers use this for movement: None means input is unavailable.
    pub usable_speed_mps: Option<f64>,
    pub counter_reset: bool,
}

#[derive(Debug, Error, PartialEq)]
pub enum TelemetryError {
    #[error("telemetry timestamps must be monotonic within a connection")]
    OutOfOrder,
    #[error("speed must be finite and non-negative; incline must be finite")]
    InvalidMeasurement,
    #[error("this session has disconnected; create a new connection epoch")]
    Disconnected,
}

/// Stateful aggregation with independent field freshness, using caller-supplied
/// monotonic elapsed times. It performs no gameplay smoothing or speed changes.
pub struct TelemetrySession {
    id: Uuid,
    sequence: u64,
    connected: bool,
    quality: Quality,
    input_source: InputSource,
    values: TelemetryUpdate,
    observed: BTreeMap<String, Duration>,
    last_report: Option<Duration>,
    counter_reset: bool,
}

impl TelemetrySession {
    pub fn new(quality: Quality) -> Self {
        Self {
            id: Uuid::new_v4(),
            sequence: 0,
            connected: true,
            quality,
            input_source: match quality {
                Quality::DeviceObservation => InputSource::LiveDevice,
                Quality::Simulated => InputSource::Simulation,
            },
            values: TelemetryUpdate::default(),
            observed: BTreeMap::new(),
            last_report: None,
            counter_reset: false,
        }
    }

    pub fn from_recording(quality: Quality) -> Self {
        let mut session = Self::new(quality);
        session.input_source = InputSource::Replay;
        session
    }

    pub fn apply(&mut self, update: TelemetryUpdate, at: Duration) -> Result<(), TelemetryError> {
        if !self.connected {
            return Err(TelemetryError::Disconnected);
        }
        if self.last_report.is_some_and(|last| at < last) {
            return Err(TelemetryError::OutOfOrder);
        }
        if update.speed_mps.is_some_and(|v| !v.is_finite() || v < 0.0)
            || update.incline_percent.is_some_and(|v| !v.is_finite())
        {
            return Err(TelemetryError::InvalidMeasurement);
        }
        self.counter_reset = [
            (self.values.distance_m, update.distance_m),
            (self.values.elapsed_s, update.elapsed_s),
            (self.values.steps, update.steps),
        ]
        .iter()
        .any(|(old, new)| matches!((old, new), (Some(a), Some(b)) if b < a));
        let mut changed = false;
        macro_rules! update_field {
            ($($field:ident),+) => { $(
                if let Some(value) = update.$field {
                    self.values.$field = Some(value);
                    self.observed.insert(stringify!($field).into(), at);
                    changed = true;
                }
            )+ };
        }
        update_field!(
            speed_mps,
            distance_m,
            elapsed_s,
            steps,
            incline_percent,
            energy_kcal,
            heart_rate_bpm,
            belt_state
        );
        if changed {
            self.last_report = Some(at);
            self.sequence += 1;
        }
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.connected = false;
    }

    pub fn snapshot(&self, now: Duration, stale_after: Duration) -> TelemetrySnapshot {
        let field_age_ms: BTreeMap<String, u64> = self
            .observed
            .iter()
            .map(|(name, at)| (name.clone(), millis(now.saturating_sub(*at))))
            .collect();
        let speed_fresh = self
            .observed
            .get("speed_mps")
            .is_some_and(|at| now >= *at && now.saturating_sub(*at) < stale_after);
        let stale = !self.connected || !speed_fresh;
        let usable_speed_mps = if stale {
            None
        } else if self.values.belt_state == Some(BeltState::Stopped) {
            Some(0.0)
        } else {
            self.values.speed_mps
        };
        TelemetrySnapshot {
            schema_version: 1,
            connection_id: self.id,
            sequence: self.sequence,
            connected: self.connected,
            quality: self.quality,
            input_source: self.input_source,
            metrics: self.values.clone(),
            speed_semantics: match self.quality {
                Quality::DeviceObservation => "device_reported_unverified",
                Quality::Simulated => "simulated",
            }
            .into(),
            sample_age_ms: self.last_report.map(|at| millis(now.saturating_sub(at))),
            field_age_ms,
            stale,
            usable_speed_mps,
            counter_reset: self.counter_reset,
        }
    }
}

pub fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speed(value: f64) -> TelemetryUpdate {
        TelemetryUpdate {
            speed_mps: Some(value),
            ..Default::default()
        }
    }

    #[test]
    fn recorded_device_values_are_never_labeled_live() {
        let session = TelemetrySession::from_recording(Quality::DeviceObservation);
        let snapshot = session.snapshot(Duration::ZERO, Duration::from_secs(2));
        assert_eq!(snapshot.input_source, InputSource::Replay);
        assert_eq!(snapshot.quality, Quality::DeviceObservation);
    }

    #[test]
    fn distance_packet_does_not_refresh_old_speed() {
        let mut session = TelemetrySession::new(Quality::DeviceObservation);
        session.apply(speed(1.0), Duration::ZERO).unwrap();
        session
            .apply(
                TelemetryUpdate {
                    distance_m: Some(20),
                    ..Default::default()
                },
                Duration::from_millis(1900),
            )
            .unwrap();
        let snapshot = session.snapshot(Duration::from_secs(2), Duration::from_secs(2));
        assert!(snapshot.stale);
        assert_eq!(snapshot.usable_speed_mps, None);
        assert_eq!(snapshot.field_age_ms["speed_mps"], 2000);
        assert_eq!(snapshot.field_age_ms["distance_m"], 100);
        assert_eq!(snapshot.metrics.steps, None);
    }

    #[test]
    fn disconnected_session_cannot_be_revived_by_late_packets() {
        let mut session = TelemetrySession::new(Quality::DeviceObservation);
        session.apply(speed(1.0), Duration::ZERO).unwrap();
        session.disconnect();
        assert_eq!(
            session
                .snapshot(Duration::ZERO, Duration::from_secs(2))
                .usable_speed_mps,
            None
        );
        assert_eq!(
            session.apply(speed(1.0), Duration::from_secs(1)),
            Err(TelemetryError::Disconnected)
        );
    }

    #[test]
    fn stop_overrides_cached_nonzero_speed() {
        let mut session = TelemetrySession::new(Quality::DeviceObservation);
        session.apply(speed(1.0), Duration::ZERO).unwrap();
        session
            .apply(
                TelemetryUpdate {
                    belt_state: Some(BeltState::Stopped),
                    ..Default::default()
                },
                Duration::from_millis(100),
            )
            .unwrap();
        assert_eq!(
            session
                .snapshot(Duration::from_millis(100), Duration::from_secs(2))
                .usable_speed_mps,
            Some(0.0)
        );
    }

    #[test]
    fn invalid_reports_are_atomic_and_cannot_refresh_data() {
        let mut session = TelemetrySession::new(Quality::Simulated);
        session.apply(speed(1.0), Duration::from_secs(1)).unwrap();
        assert_eq!(
            session.apply(speed(f64::NAN), Duration::from_secs(2)),
            Err(TelemetryError::InvalidMeasurement)
        );
        assert_eq!(
            session.apply(speed(2.0), Duration::ZERO),
            Err(TelemetryError::OutOfOrder)
        );
        assert_eq!(
            session
                .snapshot(Duration::from_secs(3), Duration::from_secs(2))
                .sequence,
            1
        );
    }

    #[test]
    fn counter_resets_preserve_device_values() {
        let mut session = TelemetrySession::new(Quality::DeviceObservation);
        session
            .apply(
                TelemetryUpdate {
                    distance_m: Some(100),
                    ..Default::default()
                },
                Duration::ZERO,
            )
            .unwrap();
        session
            .apply(
                TelemetryUpdate {
                    distance_m: Some(0),
                    ..Default::default()
                },
                Duration::from_secs(1),
            )
            .unwrap();
        let snapshot = session.snapshot(Duration::from_secs(1), Duration::from_secs(2));
        assert!(snapshot.counter_reset);
        assert_eq!(snapshot.metrics.distance_m, Some(0));
    }
}
