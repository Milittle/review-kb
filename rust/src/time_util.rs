//! UTC timestamp formatting matching Python's `datetime.now(timezone.utc).isoformat()`:
//! `+00:00` offset, microsecond precision, and the fractional part omitted when
//! the microsecond value is zero (Python omits it in that case).

use chrono::Utc;

pub fn now_iso() -> String {
    let now = Utc::now();
    let micros = now.timestamp_subsec_micros();
    if micros == 0 {
        now.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
    } else {
        now.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
    }
}

/// Stamp used by `db restore` safety backups: `%Y%m%dT%H%M%S%fZ` (microseconds).
pub fn restore_stamp() -> String {
    Utc::now().format("%Y%m%dT%H%M%S%6fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_iso_shape() {
        let s = now_iso();
        assert!(s.ends_with("+00:00"));
        assert!(!s.ends_with('Z'));
        // YYYY-MM-DDTHH:MM:SS(.ffffff)?+00:00
        let head = s.strip_suffix("+00:00").unwrap();
        assert!(head.len() >= 19); // at least the whole-second form
    }
}
