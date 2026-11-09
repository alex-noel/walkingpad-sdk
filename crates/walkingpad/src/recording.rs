use std::io::{self, BufRead, Read, Write};

use crate::{device::DeviceProfile, telemetry::Quality};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const MAX_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationRecord {
    pub elapsed_ms: u64,
    pub service_uuid: Uuid,
    pub characteristic_uuid: Uuid,
    pub payload_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CaptureRecord {
    Profile {
        format_version: u8,
        profile: DeviceProfile,
        quality: Quality,
    },
    Notification {
        #[serde(flatten)]
        packet: NotificationRecord,
    },
    End {
        elapsed_ms: u64,
        reason: String,
    },
}

pub fn write_record(writer: &mut impl Write, record: &CaptureRecord) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, record)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[derive(Debug, Error)]
pub enum RecordingError {
    #[error("recording I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid JSON at line {line}: {source}")]
    Json {
        line: usize,
        source: serde_json::Error,
    },
    #[error("invalid recording at line {line}: {reason}")]
    Invalid { line: usize, reason: String },
}

/// Bounded-memory JSONL reader. Validates ordering and format before replay.
pub struct RecordingReader<R> {
    reader: R,
    line: usize,
    elapsed_ms: u64,
    ended: bool,
    failed: bool,
}

impl<R: BufRead> RecordingReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            line: 0,
            elapsed_ms: 0,
            ended: false,
            failed: false,
        }
    }

    fn read_next(&mut self) -> Result<Option<CaptureRecord>, RecordingError> {
        let mut line = String::new();
        let size = self
            .reader
            .by_ref()
            .take((MAX_LINE_BYTES + 1) as u64)
            .read_line(&mut line)?;
        if size == 0 {
            if self.line == 0 {
                return Err(self.invalid("empty recording"));
            }
            return Ok(None);
        }
        self.line += 1;
        if size > MAX_LINE_BYTES {
            return Err(self.invalid("line exceeds 64 KiB"));
        }
        if self.ended {
            return Err(self.invalid("record after end marker"));
        }
        let record: CaptureRecord =
            serde_json::from_str(&line).map_err(|source| RecordingError::Json {
                line: self.line,
                source,
            })?;
        match &record {
            CaptureRecord::Profile {
                format_version,
                profile,
                ..
            } => {
                if self.line != 1 {
                    return Err(self.invalid("profile must appear only as the first record"));
                }
                if *format_version != 1 || profile.schema_version != 1 {
                    return Err(self.invalid("unsupported format/profile version"));
                }
            }
            CaptureRecord::Notification { packet } => {
                if self.line == 1 {
                    return Err(self.invalid("missing profile header"));
                }
                self.validate_time(packet.elapsed_ms)?;
                if packet.payload_hex.len() > 1024 || hex::decode(&packet.payload_hex).is_err() {
                    return Err(self.invalid("payload must be valid hex of at most 512 bytes"));
                }
            }
            CaptureRecord::End { elapsed_ms, .. } => {
                if self.line == 1 {
                    return Err(self.invalid("missing profile header"));
                }
                self.validate_time(*elapsed_ms)?;
                self.ended = true;
            }
        }
        Ok(Some(record))
    }

    fn invalid(&self, reason: &str) -> RecordingError {
        RecordingError::Invalid {
            line: self.line,
            reason: reason.into(),
        }
    }

    fn validate_time(&mut self, at: u64) -> Result<(), RecordingError> {
        if at < self.elapsed_ms {
            return Err(self.invalid("timestamps moved backwards"));
        }
        self.elapsed_ms = at;
        Ok(())
    }
}

impl<R: BufRead> Iterator for RecordingReader<R> {
    type Item = Result<CaptureRecord, RecordingError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        match self.read_next() {
            Ok(Some(record)) => Some(Ok(record)),
            Ok(None) => {
                self.failed = true;
                None
            }
            Err(error) => {
                self.failed = true;
                Some(Err(error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceSummary;

    fn header() -> Vec<u8> {
        let profile = DeviceProfile::new(
            DeviceSummary {
                id: "synthetic".into(),
                name: None,
                rssi: None,
                advertised_services: vec![],
            },
            vec![],
        );
        let mut bytes = Vec::new();
        write_record(
            &mut bytes,
            &CaptureRecord::Profile {
                format_version: 1,
                profile,
                quality: Quality::Simulated,
            },
        )
        .unwrap();
        bytes
    }

    #[test]
    fn rejects_missing_header_and_oversized_input() {
        assert!(
            RecordingReader::new(io::Cursor::new(b""))
                .next()
                .unwrap()
                .is_err()
        );
        let bytes = vec![b'x'; MAX_LINE_BYTES + 1];
        assert!(
            RecordingReader::new(io::Cursor::new(bytes))
                .next()
                .unwrap()
                .is_err()
        );
        let notification = b"{\"type\":\"end\",\"elapsed_ms\":0,\"reason\":\"done\"}\n";
        assert!(
            RecordingReader::new(io::Cursor::new(notification))
                .next()
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn rejects_backwards_timing_and_bad_hex() {
        for (second_time, second_payload) in [(1, "00"), (3, "zz")] {
            let mut bytes = header();
            for (at, payload) in [(2, "00"), (second_time, second_payload)] {
                write_record(
                    &mut bytes,
                    &CaptureRecord::Notification {
                        packet: NotificationRecord {
                            elapsed_ms: at,
                            service_uuid: Uuid::nil(),
                            characteristic_uuid: Uuid::nil(),
                            payload_hex: payload.into(),
                        },
                    },
                )
                .unwrap();
            }
            let results: Vec<_> = RecordingReader::new(io::Cursor::new(bytes)).collect();
            assert!(results[0].is_ok());
            assert!(results[1].is_ok());
            assert!(results[2].is_err());
        }
    }
}
