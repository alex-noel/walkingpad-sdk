//! KingSmith text telemetry (v1/v5 substitution alphabets). Protocol references and
//! observed hardware scope are documented in docs/protocol-notes.md.
//! Speed targets are a separate explicit API from the read-query allowlist.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    device::bluetooth_uuid32,
    telemetry::{BeltState, TelemetryUpdate},
};

const STANDARD_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const WIRE_ALPHABET: &[u8; 64] =
    b"SaCw4FGHIJqLhN+P9RVTU/WcY6ObDdefgEijklmnopQrsBuvMxXz1yA2t5078KZ3";
const MAX_LINE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alphabet {
    V1,
    V5,
}

impl Alphabet {
    fn table(self) -> &'static [u8; 64] {
        match self {
            Self::V1 => WIRE_ALPHABET,
            Self::V5 => b"iaCw4FGHIJqLhN+P9RVTU/WcY6ObDdefgEZjklmnopQrsBuvMxXz1yA2t5078KS3",
        }
    }
}

/// Known GATT aliases; the alphabet still has to match the device firmware.
pub fn channels(service: Uuid) -> Option<(Uuid, Uuid)> {
    (0..=2).find_map(|variant| {
        let prefix = variant << 16;
        (service == bluetooth_uuid32(prefix | 0x1234)).then_some((
            bluetooth_uuid32(prefix | 0xfed7),
            bluetooth_uuid32(prefix | 0xfed8),
        ))
    })
}

#[derive(Debug, Clone, Copy)]
pub enum ReadQuery {
    Handshake,
    Network,
    DeviceName,
    ProductKey,
    Version,
    Configuration,
    Properties,
}

impl ReadQuery {
    pub fn encode(self, alphabet: Alphabet) -> Vec<u8> {
        encode_line(
            match self {
                Self::Handshake => "shake",
                Self::Network => "net",
                Self::DeviceName => "get_dn",
                Self::ProductKey => "get_pk",
                Self::Version => "version",
                Self::Configuration => "servers getProp 1 3 7 8 9 16 17 18 19 21 22 23 24 13 15",
                Self::Properties => "servers getProp 1 9 15 2 10 11 12 13 14",
            },
            alphabet,
        )
    }
}

fn encode_line(text: &str, alphabet: Alphabet) -> Vec<u8> {
    let mut bytes = STANDARD.encode(text).into_bytes();
    for byte in &mut bytes {
        if let Some(index) = STANDARD_ALPHABET.iter().position(|value| value == byte) {
            *byte = alphabet.table()[index];
        }
    }
    bytes.push(b'\r');
    bytes
}

/// Exact 0.1 km/h target; callers must apply SpeedGuard before any device write.
/// Does not change run state or control mode.
pub fn encode_speed_target(tenths: u16, alphabet: Alphabet) -> Result<Vec<u8>, &'static str> {
    if !(5..=60).contains(&tenths) {
        return Err("Walking speed target outside 0.5–6.0 km/h");
    }
    Ok(encode_line(
        &format!("props CurrentSpeed {}.{}", tenths / 10, tenths % 10),
        alphabet,
    ))
}

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("non-text KingSmith notification")]
    BinaryNotification,
    #[error("KingSmith line exceeds 4096 bytes")]
    OversizedLine,
    #[error("invalid KingSmith base64 or UTF-8 line")]
    InvalidEncoding,
    #[error("invalid KingSmith telemetry property: {0}")]
    InvalidProperty(String),
    #[error("KingSmith alphabet not established by a successful handshake")]
    UnverifiedAlphabet,
}

/// Reassembles fragmented and coalesced CR-terminated notifications. Binary
/// sidecars do not contaminate a pending text frame. No partial line emits data.
#[derive(Default)]
pub struct Decoder {
    pending: Vec<u8>,
    discarding: bool,
    alphabet: Option<Alphabet>,
}

impl Decoder {
    pub fn with_alphabet(alphabet: Alphabet) -> Self {
        Self {
            alphabet: Some(alphabet),
            ..Self::default()
        }
    }

    pub fn alphabet(&self) -> Option<Alphabet> {
        self.alphabet
    }

    fn decode_pending(&mut self) -> Result<Option<TelemetryUpdate>, DecodeError> {
        if let Some(alphabet) = self.alphabet {
            return parse_properties(&decode_line(&self.pending, alphabet)?);
        }
        // The harmless "shake" request has identical bytes in both tables.
        // Select a table only from a complete, successful handshake response.
        for alphabet in [Alphabet::V1, Alphabet::V5] {
            if decode_line(&self.pending, alphabet)
                .is_ok_and(|text| matches!(text.trim(), "shake 00" | "shake 0"))
            {
                self.alphabet = Some(alphabet);
                return Ok(None);
            }
        }
        Err(DecodeError::UnverifiedAlphabet)
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Result<TelemetryUpdate, DecodeError>> {
        if bytes
            .iter()
            .any(|b| *b != b'\r' && *b != b'=' && !WIRE_ALPHABET.contains(b))
        {
            return vec![Err(DecodeError::BinaryNotification)];
        }
        let mut reports = Vec::new();
        for &byte in bytes {
            if byte == b'\r' {
                if !self.discarding && !self.pending.is_empty() {
                    match self.decode_pending() {
                        Ok(Some(update)) => reports.push(Ok(update)),
                        Ok(None) => {}
                        Err(error) => reports.push(Err(error)),
                    }
                }
                self.pending.clear();
                self.discarding = false;
            } else if !self.discarding {
                if self.pending.len() == MAX_LINE {
                    self.pending.clear();
                    self.discarding = true;
                    reports.push(Err(DecodeError::OversizedLine));
                } else {
                    self.pending.push(byte);
                }
            }
        }
        reports
    }
}

fn decode_line(bytes: &[u8], alphabet: Alphabet) -> Result<String, DecodeError> {
    let mut mapped: Vec<u8> = bytes
        .iter()
        .map(|byte| {
            alphabet
                .table()
                .iter()
                .position(|value| value == byte)
                .map_or(*byte, |index| STANDARD_ALPHABET[index])
        })
        .collect();
    while !mapped.len().is_multiple_of(4) {
        mapped.push(b'=');
    }
    let decoded = STANDARD
        .decode(mapped)
        .map_err(|_| DecodeError::InvalidEncoding)?;
    String::from_utf8(decoded).map_err(|_| DecodeError::InvalidEncoding)
}

fn parse_properties(text: &str) -> Result<Option<TelemetryUpdate>, DecodeError> {
    let mut fields = text.split_whitespace();
    if fields.next() != Some("props") {
        return Ok(None);
    }
    let mut update = TelemetryUpdate::default();
    let mut any = false;
    while let Some(key) = fields.next() {
        if key == "Error" {
            break;
        }
        let value = fields
            .next()
            .ok_or_else(|| DecodeError::InvalidProperty(key.into()))?;
        let invalid = || DecodeError::InvalidProperty(key.into());
        match key {
            "CurrentSpeed" => {
                let speed = value.parse::<f64>().map_err(|_| invalid())?;
                if !speed.is_finite() || !(0.0..=30.0).contains(&speed) {
                    return Err(invalid());
                }
                update.speed_mps = Some(speed / 3.6);
            }
            "RunningDistance" => update.distance_m = Some(value.parse().map_err(|_| invalid())?),
            "RunningTotalTime" => update.elapsed_s = Some(value.parse().map_err(|_| invalid())?),
            "RunningSteps" => update.steps = Some(value.parse().map_err(|_| invalid())?),
            "runState" => {
                update.belt_state = Some(match value {
                    "0" => BeltState::Stopped,
                    "1" => BeltState::Running,
                    _ => return Err(invalid()),
                })
            }
            // BurnCalories scaling and spm are not part of the validated contract.
            _ => continue,
        }
        any = true;
    }
    Ok(any.then_some(update))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_line(text: &str) -> Vec<u8> {
        super::encode_line(text, Alphabet::V1)
    }

    #[test]
    fn captured_x21_handshake_selects_v5_before_decoding_metrics() {
        let mut decoder = Decoder::default();
        assert!(decoder.feed(&encode_line("props CurrentSpeed 3.6"))[0].is_err());
        assert_eq!(decoder.alphabet(), None);
        // Actual KS-NACH-X21C response captured on macOS; plaintext "shake 00\r".
        assert!(decoder.feed(b"DAEEOAUghwiN\r").is_empty());
        assert_eq!(decoder.alphabet(), Some(Alphabet::V5));
        assert_eq!(
            ReadQuery::Handshake.encode(Alphabet::V1),
            ReadQuery::Handshake.encode(Alphabet::V5)
        );
        let reports = decoder.feed(&super::encode_line(
            "props CurrentSpeed 0 runState 0",
            Alphabet::V5,
        ));
        assert_eq!(reports[0].as_ref().unwrap().speed_mps, Some(0.0));
        assert_eq!(
            reports[0].as_ref().unwrap().belt_state,
            Some(BeltState::Stopped)
        );
    }

    #[test]
    fn v1_handshake_and_unpadded_reports_are_supported() {
        let mut decoder = Decoder::default();
        assert!(decoder.feed(&encode_line("shake 00\r")).is_empty());
        assert_eq!(decoder.alphabet(), Some(Alphabet::V1));
        let mut wire = encode_line("props CurrentSpeed 1.1");
        wire.retain(|b| *b != b'=');
        let reports = decoder.feed(&wire);
        assert!((reports[0].as_ref().unwrap().speed_mps.unwrap() - 1.1 / 3.6).abs() < 1e-9);
    }

    #[test]
    fn query_wire_format_and_aliases() {
        assert_eq!(ReadQuery::Handshake.encode(Alphabet::V1), b"DAEEOAU=\r");
        let query = ReadQuery::Properties.encode(Alphabet::V1);
        assert_eq!(
            decode_line(&query[..query.len() - 1], Alphabet::V1).unwrap(),
            "servers getProp 1 9 15 2 10 11 12 13 14"
        );
        let service = bluetooth_uuid32(0x00011234);
        assert_eq!(
            channels(service).unwrap().1.to_string(),
            "0001fed8-0000-1000-8000-00805f9b34fb"
        );
        assert!(channels(bluetooth_uuid32(0x1826)).is_none());
        for alphabet in [Alphabet::V1, Alphabet::V5] {
            let wire = encode_speed_target(31, alphabet).unwrap();
            assert_eq!(
                decode_line(&wire[..wire.len() - 1], alphabet).unwrap(),
                "props CurrentSpeed 3.1"
            );
            assert!(encode_speed_target(0, alphabet).is_err());
            assert!(encode_speed_target(61, alphabet).is_err());
        }
    }

    #[test]
    fn fragments_coalesced_lines_and_binary_sidecars() {
        let mut decoder = Decoder::with_alphabet(Alphabet::V1);
        let wire = encode_line(
            "props CurrentSpeed 3.6 RunningDistance 120 RunningSteps 250 RunningTotalTime 180 runState 1",
        );
        assert!(decoder.feed(&wire[..10]).is_empty());
        assert!(decoder.feed(&[0xff, 0, 2])[0].is_err());
        let mut rest = wire[10..].to_vec();
        rest.extend(encode_line("props CurrentSpeed 0 runState 0"));
        let reports = decoder.feed(&rest);
        assert_eq!(reports.len(), 2);
        let report = reports[0].as_ref().unwrap();
        assert_eq!(report.speed_mps, Some(1.0));
        assert_eq!(report.distance_m, Some(120));
        assert_eq!(report.steps, Some(250));
        assert_eq!(report.elapsed_s, Some(180));
        assert_eq!(report.energy_kcal, None);
        assert_eq!(
            reports[1].as_ref().unwrap().belt_state,
            Some(BeltState::Stopped)
        );
    }

    #[test]
    fn malformed_and_partial_reports_do_not_invent_speed() {
        let mut decoder = Decoder::with_alphabet(Alphabet::V1);
        assert!(decoder.feed(&encode_line("shake 00")).is_empty());
        for invalid in ["NaN", "-1", "500", "oops"] {
            assert!(
                decoder.feed(&encode_line(&format!("props CurrentSpeed {invalid}")))[0].is_err()
            );
        }
        let reports = decoder.feed(&encode_line("props RunningSteps 42"));
        assert_eq!(reports[0].as_ref().unwrap().speed_mps, None);
        assert!(decoder.feed(&vec![b'A'; MAX_LINE + 10])[0].is_err());
        assert!(decoder.feed(b"AAAA\r").is_empty());
        assert!(decoder.feed(&encode_line("props CurrentSpeed 0"))[0].is_ok());
    }
}
