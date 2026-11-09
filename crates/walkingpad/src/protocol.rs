//! Standard FTMS Treadmill Data (0x2ACD) decoding. Vendor flags are rejected
//! explicitly until a device-specific decoder is validated against captures.

use crate::telemetry::TelemetryUpdate;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("truncated FTMS packet at byte {offset}: need {needed} more bytes")]
    Truncated { offset: usize, needed: usize },
    #[error("unverified FTMS vendor/reserved flags: 0x{0:04x}")]
    UnsupportedFlags(u16),
    #[error("unexpected trailing FTMS bytes: {0}")]
    TrailingBytes(usize),
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl Cursor<'_> {
    fn take(&mut self, count: usize) -> Result<&[u8], DecodeError> {
        let end = self.offset + count;
        if end > self.bytes.len() {
            return Err(DecodeError::Truncated {
                offset: self.offset,
                needed: end - self.bytes.len(),
            });
        }
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }
    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, DecodeError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn i16(&mut self) -> Result<i16, DecodeError> {
        Ok(self.u16()? as i16)
    }
    fn u24(&mut self) -> Result<u64, DecodeError> {
        let b = self.take(3)?;
        Ok(u64::from(b[0]) | (u64::from(b[1]) << 8) | (u64::from(b[2]) << 16))
    }
}

pub fn decode_ftms_treadmill(bytes: &[u8]) -> Result<TelemetryUpdate, DecodeError> {
    let mut cursor = Cursor { bytes, offset: 0 };
    let flags = cursor.u16()?;
    if flags & 0xe000 != 0 {
        return Err(DecodeError::UnsupportedFlags(flags & 0xe000));
    }
    let mut update = TelemetryUpdate::default();
    // More Data uses inverted logic: speed is present only when bit zero is 0.
    if flags & 1 == 0 {
        update.speed_mps = Some(f64::from(cursor.u16()?) / 360.0);
    }
    if flags & (1 << 1) != 0 {
        cursor.take(2)?;
    }
    if flags & (1 << 2) != 0 {
        update.distance_m = Some(cursor.u24()?);
    }
    if flags & (1 << 3) != 0 {
        update.incline_percent = Some(f64::from(cursor.i16()?) / 10.0);
        cursor.take(2)?; // Ramp angle.
    }
    if flags & (1 << 4) != 0 {
        cursor.take(4)?;
    }
    if flags & (1 << 5) != 0 {
        cursor.take(1)?;
    }
    if flags & (1 << 6) != 0 {
        cursor.take(1)?;
    }
    if flags & (1 << 7) != 0 {
        let total = cursor.u16()?;
        update.energy_kcal = (total != u16::MAX).then_some(u64::from(total));
        cursor.take(3)?; // Hourly and per-minute energy.
    }
    if flags & (1 << 8) != 0 {
        let hr = cursor.u8()?;
        update.heart_rate_bpm = (hr != 0).then_some(u16::from(hr));
    }
    if flags & (1 << 9) != 0 {
        cursor.take(1)?;
    }
    if flags & (1 << 10) != 0 {
        update.elapsed_s = Some(u64::from(cursor.u16()?));
    }
    if flags & (1 << 11) != 0 {
        cursor.take(2)?;
    }
    if flags & (1 << 12) != 0 {
        cursor.take(4)?;
    }
    if cursor.offset != bytes.len() {
        return Err(DecodeError::TrailingBytes(bytes.len() - cursor.offset));
    }
    Ok(update)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_units_and_optional_counters() {
        // Synthetic: flags distance+time, 3 km/h, 1200 metres, 1440 seconds.
        let data = hex::decode("04042c01b00400a005").unwrap();
        let report = decode_ftms_treadmill(&data).unwrap();
        assert!((report.speed_mps.unwrap() - 0.833333333).abs() < 1e-8);
        assert_eq!(report.distance_m, Some(1200));
        assert_eq!(report.elapsed_s, Some(1440));
        assert_eq!(report.steps, None);
        for end in 0..data.len() {
            assert!(decode_ftms_treadmill(&data[..end]).is_err(), "prefix {end}");
        }
    }

    #[test]
    fn partial_record_does_not_invent_speed() {
        let report = decode_ftms_treadmill(&[0x05, 0, 0x34, 0x12, 0]).unwrap();
        assert_eq!(report.speed_mps, None);
        assert_eq!(report.distance_m, Some(0x1234));
    }

    #[test]
    fn parses_signed_incline_and_rejects_unverified_extensions() {
        let report = decode_ftms_treadmill(&hex::decode("08000000f6ff0000").unwrap()).unwrap();
        assert_eq!(report.incline_percent, Some(-1.0));
        assert!(matches!(
            decode_ftms_treadmill(&[0, 0x20, 0, 0]),
            Err(DecodeError::UnsupportedFlags(0x2000))
        ));
        assert_eq!(
            decode_ftms_treadmill(&[0, 0, 0, 0, 1]).unwrap_err(),
            DecodeError::TrailingBytes(1)
        );
    }
}
