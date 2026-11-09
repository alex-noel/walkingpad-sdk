//! Bounded relative speed requests. Start/stop and mode changes are not supported.
use crate::telemetry::{BeltState, InputSource, TelemetrySnapshot};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, time::Duration};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SpeedLimits {
    pub min_tenths: u16,
    pub max_tenths: u16,
}
impl SpeedLimits {
    pub fn new(min_kmh: f64, max_kmh: f64) -> Result<Self, String> {
        if !min_kmh.is_finite()
            || !max_kmh.is_finite()
            || min_kmh < 0.5
            || max_kmh > 6.0
            || min_kmh >= max_kmh
            || (min_kmh * 10.0 - (min_kmh * 10.0).round()).abs() > 1e-6
            || (max_kmh * 10.0 - (max_kmh * 10.0).round()).abs() > 1e-6
        {
            return Err(
                "Speed limits must be 0.1 km/h multiples within 0.5–6.0, with min < max".into(),
            );
        }
        Ok(Self {
            min_tenths: (min_kmh * 10.0).round() as u16,
            max_tenths: (max_kmh * 10.0).round() as u16,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeedRequest {
    pub request_id: Uuid,
    pub connection_id: Uuid,
    pub sequence: u64,
    pub expected_speed_tenths: u16,
    pub delta_tenths: i8,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeedReply {
    pub request_id: Uuid,
    pub status: String,
    pub target_kmh: Option<f64>,
    pub message: String,
    #[serde(default)]
    pub control_locked: bool,
}
impl SpeedReply {
    pub fn rejected(id: Uuid, message: impl Into<String>) -> Self {
        Self {
            request_id: id,
            status: "rejected".into(),
            target_kmh: None,
            message: message.into(),
            control_locked: false,
        }
    }
}

pub const COOLDOWN: Duration = Duration::from_secs(10);
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(8);

pub struct SpeedGuard {
    pub limits: SpeedLimits,
    seen: HashSet<Uuid>,
    next_at: Duration,
    pub locked: bool,
}
impl SpeedGuard {
    pub fn new(limits: SpeedLimits) -> Self {
        Self {
            limits,
            seen: HashSet::new(),
            next_at: Duration::ZERO,
            locked: false,
        }
    }
    pub fn prepare(
        &mut self,
        request: &SpeedRequest,
        input: &TelemetrySnapshot,
        now: Duration,
    ) -> Result<u16, &'static str> {
        if self.locked {
            return Err("Speed control locked after an unconfirmed command; restart the bridge");
        }
        if self.seen.len() >= 4096 {
            return Err("Request limit reached; restart the bridge");
        }
        if !self.seen.insert(request.request_id) {
            return Err("Duplicate request; never resent");
        }
        if !matches!(request.delta_tenths, -1 | 1) {
            return Err("Only +0.1 or -0.1 km/h is supported");
        }
        if now < self.next_at {
            return Err("Speed bonus cooldown: ten seconds between requests");
        }
        if !fresh_running(input) {
            return Err("Fresh running-belt telemetry is required");
        }
        if request.connection_id != input.connection_id
            || request.sequence > input.sequence
            || input.sequence.saturating_sub(request.sequence) > 16
        {
            return Err("Walking input changed; request discarded");
        }
        let observed = input.usable_speed_mps.unwrap() * 36.0;
        if (observed - f64::from(request.expected_speed_tenths)).abs() > 0.25 {
            return Err("Pad speed changed since pickup; request discarded");
        }
        let target = i32::from(request.expected_speed_tenths) + i32::from(request.delta_tenths);
        if target < i32::from(self.limits.min_tenths) || target > i32::from(self.limits.max_tenths)
        {
            return Err("Speed limit reached");
        }
        self.next_at = now + COOLDOWN;
        Ok(target as u16)
    }
}
pub fn fresh_running(input: &TelemetrySnapshot) -> bool {
    input.connected
        && !input.stale
        && input.input_source != InputSource::Replay
        && input.metrics.belt_state == Some(BeltState::Running)
        && input
            .usable_speed_mps
            .is_some_and(|s| s.is_finite() && s > 0.0)
        && input
            .field_age_ms
            .get("speed_mps")
            .is_some_and(|age| *age < 3000)
        && input
            .field_age_ms
            .get("belt_state")
            .is_some_and(|age| *age < 3000)
}

#[cfg(feature = "ble")]
pub mod transport {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Instant,
    };
    use tokio::sync::{mpsc, oneshot, watch};
    pub struct SpeedWork {
        pub request: SpeedRequest,
        pub arrived: Instant,
        pub cancelled: Arc<AtomicBool>,
        pub reply: oneshot::Sender<SpeedReply>,
    }
    struct Pending {
        work: SpeedWork,
        target: u16,
        sent_at: Duration,
        sequence: u64,
    }
    pub struct SpeedControlHost {
        pub requests: mpsc::Receiver<SpeedWork>,
        pub telemetry: watch::Receiver<TelemetrySnapshot>,
        guard: SpeedGuard,
        pending: Option<Pending>,
    }
    impl SpeedControlHost {
        pub fn new(
            limits: SpeedLimits,
            requests: mpsc::Receiver<SpeedWork>,
            telemetry: watch::Receiver<TelemetrySnapshot>,
        ) -> Self {
            Self {
                requests,
                telemetry,
                guard: SpeedGuard::new(limits),
                pending: None,
            }
        }
        pub fn begin(&mut self, work: SpeedWork, now: Duration) -> Option<u16> {
            let reject = if work.cancelled.load(Ordering::SeqCst)
                || work.reply.is_closed()
                || work.arrived.elapsed() > Duration::from_secs(1)
            {
                Some("Request expired or cancelled")
            } else if self.pending.is_some() {
                Some("Another speed request is awaiting telemetry")
            } else {
                None
            };
            let input = self.telemetry.borrow().clone();
            let target = reject.map_or_else(|| self.guard.prepare(&work.request, &input, now), Err);
            match target {
                Ok(target) => {
                    self.pending = Some(Pending {
                        work,
                        target,
                        sent_at: now,
                        sequence: input.sequence,
                    });
                    Some(target)
                }
                Err(message) => {
                    let mut reply = SpeedReply::rejected(work.request.request_id, message);
                    reply.control_locked = self.guard.locked;
                    let _ = work.reply.send(reply);
                    None
                }
            }
        }
        pub fn fail(&mut self, message: &str) {
            self.guard.locked = true;
            self.finish("unconfirmed", message);
        }
        fn finish(&mut self, status: &str, message: &str) {
            if let Some(p) = self.pending.take() {
                let _ = p.work.reply.send(SpeedReply {
                    request_id: p.work.request.request_id,
                    status: status.into(),
                    target_kmh: Some(f64::from(p.target) / 10.0),
                    message: message.into(),
                    control_locked: self.guard.locked,
                });
            }
        }
        pub fn observe(&mut self, now: Duration) {
            let Some(p) = &self.pending else {
                return;
            };
            let input = self.telemetry.borrow().clone();
            if !fresh_running(&input) || input.connection_id != p.work.request.connection_id {
                self.fail("Input interrupted; speed command not confirmed. Control is locked");
            } else if now.saturating_sub(p.sent_at) >= CONFIRM_TIMEOUT {
                self.fail(
                    "Pad did not report the requested speed; control is locked. No retry sent",
                );
            } else if now.saturating_sub(p.sent_at) >= Duration::from_secs(1)
                && input.sequence > p.sequence
                && input
                    .field_age_ms
                    .get("speed_mps")
                    .is_some_and(|age| *age <= 500)
                && (input.usable_speed_mps.unwrap() * 36.0 - f64::from(p.target)).abs() < 0.25
            {
                self.finish("confirmed", "Pad reports requested speed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{Quality, TelemetrySession, TelemetryUpdate};
    fn input() -> TelemetrySnapshot {
        let mut s = TelemetrySession::new(Quality::Simulated);
        s.apply(
            TelemetryUpdate {
                speed_mps: Some(3.0 / 3.6),
                belt_state: Some(BeltState::Running),
                ..Default::default()
            },
            Duration::ZERO,
        )
        .unwrap();
        s.snapshot(Duration::ZERO, Duration::from_secs(3))
    }
    fn request(s: &TelemetrySnapshot, delta: i8) -> SpeedRequest {
        SpeedRequest {
            request_id: Uuid::new_v4(),
            connection_id: s.connection_id,
            sequence: s.sequence,
            expected_speed_tenths: 30,
            delta_tenths: delta,
        }
    }
    #[test]
    fn bounded_steps_cooldown_and_duplicates() {
        let s = input();
        let mut g = SpeedGuard::new(SpeedLimits::new(1.0, 5.0).unwrap());
        let r = request(&s, 1);
        assert_eq!(g.prepare(&r, &s, Duration::ZERO), Ok(31));
        assert!(g.prepare(&r, &s, Duration::from_secs(11)).is_err());
        assert!(
            g.prepare(&request(&s, -1), &s, Duration::from_secs(1))
                .is_err()
        );
        assert_eq!(
            g.prepare(&request(&s, -1), &s, Duration::from_secs(11)),
            Ok(29)
        );
    }
    #[test]
    fn rejects_stopped_stale_replay_wrong_epoch_and_limits() {
        let s = input();
        for mode in 0..7 {
            let mut s = s.clone();
            let mut r = request(&s, 1);
            match mode {
                0 => s.stale = true,
                1 => s.metrics.belt_state = Some(BeltState::Stopped),
                2 => s.input_source = InputSource::Replay,
                3 => r.connection_id = Uuid::new_v4(),
                4 => r.expected_speed_tenths = 32,
                5 => r.delta_tenths = 10,
                _ => {
                    s.field_age_ms.insert("belt_state".into(), 3000);
                }
            }
            assert!(
                SpeedGuard::new(SpeedLimits::new(1.0, 5.0).unwrap())
                    .prepare(&r, &s, Duration::ZERO)
                    .is_err()
            );
        }
        assert!(
            SpeedGuard::new(SpeedLimits::new(1.0, 3.0).unwrap())
                .prepare(&request(&s, 1), &s, Duration::ZERO)
                .is_err()
        );
        assert!(SpeedLimits::new(f64::NAN, 5.0).is_err());
        assert!(SpeedLimits::new(0.0, 5.0).is_err());
        assert!(
            SpeedGuard::new(SpeedLimits::new(3.0, 5.0).unwrap())
                .prepare(&request(&s, -1), &s, Duration::ZERO)
                .is_err()
        );
    }

    #[cfg(feature = "ble")]
    #[tokio::test]
    async fn cancelled_requests_never_write_and_timeout_locks_future_commands() {
        use super::transport::*;
        use std::sync::{Arc, atomic::AtomicBool};
        use std::time::Instant;
        use tokio::sync::{mpsc, oneshot, watch};
        let s = input();
        let (_tx, rx) = mpsc::channel(1);
        let (_updates, telemetry) = watch::channel(s.clone());
        let mut host = SpeedControlHost::new(SpeedLimits::new(1.0, 5.0).unwrap(), rx, telemetry);
        let (reply, result) = oneshot::channel();
        assert_eq!(
            host.begin(
                SpeedWork {
                    request: request(&s, 1),
                    arrived: Instant::now(),
                    cancelled: Arc::new(AtomicBool::new(true)),
                    reply
                },
                Duration::ZERO
            ),
            None
        );
        assert_eq!(result.await.unwrap().status, "rejected");
        let (reply, result) = oneshot::channel();
        assert_eq!(
            host.begin(
                SpeedWork {
                    request: request(&s, 1),
                    arrived: Instant::now(),
                    cancelled: Arc::new(AtomicBool::new(false)),
                    reply
                },
                Duration::ZERO
            ),
            Some(31)
        );
        host.observe(Duration::from_secs(9));
        let outcome = result.await.unwrap();
        assert_eq!(outcome.status, "unconfirmed");
        assert!(outcome.control_locked);
        let (reply, result) = oneshot::channel();
        assert_eq!(
            host.begin(
                SpeedWork {
                    request: request(&s, 1),
                    arrived: Instant::now(),
                    cancelled: Arc::new(AtomicBool::new(false)),
                    reply
                },
                Duration::from_secs(20)
            ),
            None
        );
        let outcome = result.await.unwrap();
        assert_eq!(outcome.status, "rejected");
        assert!(outcome.control_locked);
    }
}
