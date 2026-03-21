//! HTTP date formatting, parsing, ETag generation, and Range header parsing.

use std::time::{Duration, SystemTime};

/// Format a `SystemTime` as an HTTP-date string.
pub fn format_http_date(time: SystemTime) -> String {
    let duration = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = duration.as_secs();

    let (year, month, day, hour, minute, second) = unix_to_date(seconds);
    let day_of_week = day_of_week(year, month, day);

    let dow = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][day_of_week as usize];
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(month - 1) as usize];

    format!("{dow}, {day:02} {month} {year} {hour:02}:{minute:02}:{second:02} GMT")
}

/// Parse a subset of HTTP-date formats.
pub fn parse_http_date(input: &str) -> Option<SystemTime> {
    let parts: Vec<&str> = input.split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }

    let day: u32 = parts[1].parse().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: u64 = parts[3].parse().ok()?;
    let time_parts: Vec<&str> = parts[4].split(':').collect();
    if time_parts.len() != 3 {
        return None;
    }
    let hour: u64 = time_parts[0].parse().ok()?;
    let minute: u64 = time_parts[1].parse().ok()?;
    let second: u64 = time_parts[2].parse().ok()?;

    let unix = date_to_unix(year, month, day as u64, hour, minute, second)?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(unix))
}

/// Generate an ETag from file size and modification time.
pub fn generate_etag(size: u64, modified: Option<&SystemTime>) -> String {
    let modified = modified
        .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("\"{modified:x}-{size:x}\"")
}

/// Parse a `"Range: bytes=start-end"` header value.
pub fn parse_range(range: &str, file_size: u64) -> Option<(u64, u64)> {
    let range = range.strip_prefix("bytes=")?;
    let range = range.split(',').next()?.trim();

    if let Some(suffix_length) = range.strip_prefix('-') {
        let suffix_length: u64 = suffix_length.parse().ok()?;
        if suffix_length == 0 || suffix_length > file_size {
            return None;
        }
        Some((file_size - suffix_length, file_size - 1))
    } else if range.ends_with('-') {
        let start: u64 = range.trim_end_matches('-').parse().ok()?;
        if start >= file_size {
            return None;
        }
        Some((start, file_size - 1))
    } else {
        let (start, end) = range.split_once('-')?;
        let start: u64 = start.parse().ok()?;
        let end: u64 = end.parse().ok()?;
        if start > end || start >= file_size {
            return None;
        }
        Some((start, end.min(file_size - 1)))
    }
}

fn unix_to_date(seconds: u64) -> (u64, u64, u64, u64, u64, u64) {
    let second = seconds % 60;
    let minute = (seconds / 60) % 60;
    let hour = (seconds / 3600) % 24;
    let mut days = seconds / 86_400;

    let mut year = 1970;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    let month_days = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 0;
    for (index, days_in_month) in month_days.iter().enumerate() {
        if days < *days_in_month {
            month = index as u64 + 1;
            break;
        }
        days -= days_in_month;
    }

    (year, month, days + 1, hour, minute, second)
}

fn date_to_unix(
    year: u64,
    month: u64,
    day: u64,
    hour: u64,
    minute: u64,
    second: u64,
) -> Option<u64> {
    if year < 1970 || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut total_days = 0;
    for current_year in 1970..year {
        total_days += if is_leap(current_year) { 366 } else { 365 };
    }

    let month_days = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    for days_in_month in month_days.iter().take((month - 1) as usize) {
        total_days += days_in_month;
    }
    total_days += day - 1;

    Some(total_days * 86_400 + hour * 3600 + minute * 60 + second)
}

fn is_leap(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn day_of_week(year: u64, month: u64, day: u64) -> u64 {
    let offsets = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let year = if month < 3 { year - 1 } else { year };
    (year + year / 4 - year / 100 + year / 400 + offsets[(month - 1) as usize] + day) % 7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_http_date() {
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_777);
        let formatted = format_http_date(time);
        let parsed = parse_http_date(&formatted).unwrap();
        let diff = if time > parsed {
            time.duration_since(parsed).unwrap().as_secs()
        } else {
            parsed.duration_since(time).unwrap().as_secs()
        };
        assert!(diff <= 1);
    }

    #[test]
    fn parse_range_explicit() {
        assert_eq!(parse_range("bytes=0-499", 1000), Some((0, 499)));
    }

    #[test]
    fn parse_range_suffix() {
        assert_eq!(parse_range("bytes=-200", 1000), Some((800, 999)));
    }
}
