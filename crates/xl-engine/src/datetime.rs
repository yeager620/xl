//! Excel serial dates (1900 date system): days since 1899-12-30, with the
//! time of day as the fractional part. Dates before 1900-03-01 are off by one
//! relative to Excel, which treats 1900 as a leap year; that range is rare
//! enough that we accept the difference.

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const EPOCH: i64 = -25_569; // days_from_civil(1899, 12, 30)

pub fn serial_from_date(y: i64, m: u32, d: u32) -> f64 {
    (days_from_civil(y, m, d) - EPOCH) as f64
}

/// Excel's DATE(): months and days overflow into following years/months.
pub fn serial_from_parts(y: i64, m: i64, d: i64) -> f64 {
    let y = if (0..1900).contains(&y) { y + 1900 } else { y };
    let total_m = y * 12 + (m - 1);
    let (yy, mm) = (total_m.div_euclid(12), total_m.rem_euclid(12) as u32 + 1);
    (days_from_civil(yy, mm, 1) - EPOCH + d - 1) as f64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// Milliseconds, for formats that show fractional seconds.
    pub millis: u32,
    /// 0 = Sunday.
    pub weekday: u32,
}

/// Break a serial into calendar parts, rounding to the nearest millisecond.
pub fn datetime_from_serial(serial: f64) -> Option<DateTime> {
    if !serial.is_finite() || !(0.0..2_958_466.0).contains(&serial) {
        return None;
    }
    let total_ms = (serial * 86_400_000.0).round() as i64;
    let days = total_ms.div_euclid(86_400_000);
    let ms = total_ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days + EPOCH);
    Some(DateTime {
        year,
        month,
        day,
        hour: (ms / 3_600_000) as u32,
        minute: (ms / 60_000 % 60) as u32,
        second: (ms / 1000 % 60) as u32,
        millis: (ms % 1000) as u32,
        weekday: ((days + EPOCH + 4).rem_euclid(7)) as u32, // 1970-01-01 was a Thursday
    })
}

pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

pub const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
pub const WEEKDAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_serials() {
        assert_eq!(serial_from_date(1900, 3, 1), 61.0);
        assert_eq!(serial_from_date(2026, 11, 13), 46_339.0);
        assert_eq!(serial_from_date(1970, 1, 1), 25_569.0);
        let dt = datetime_from_serial(46_339.75).unwrap();
        assert_eq!((dt.year, dt.month, dt.day, dt.hour, dt.minute), (2026, 11, 13, 18, 0));
        assert_eq!(dt.weekday, 5); // Friday
    }

    #[test]
    fn date_overflow() {
        assert_eq!(serial_from_parts(2026, 14, 1), serial_from_date(2027, 2, 1));
        assert_eq!(serial_from_parts(2026, 1, 0), serial_from_date(2025, 12, 31));
        assert_eq!(serial_from_parts(126, 1, 1), serial_from_date(2026, 1, 1));
    }

    #[test]
    fn roundtrip_many_days() {
        for z in -100_000..100_000 {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }
}
