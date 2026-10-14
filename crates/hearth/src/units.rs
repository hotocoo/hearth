//! Human formatting of byte counts and ratios.

#[allow(clippy::cast_precision_loss)]
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[allow(clippy::cast_precision_loss)]
pub fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "n/a".into();
    }
    format!("{:.1}%", part as f64 / whole as f64 * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.50 KiB");
        assert_eq!(bytes(64 * 1024 * 1024 * 1024), "64.00 GiB");
        assert_eq!(percent(1, 4), "25.0%");
        assert_eq!(percent(1, 0), "n/a");
    }
}
