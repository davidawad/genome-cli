//! Small shared helpers.

use chrono::{DateTime, Utc};

/// Current UTC time, or `SOURCE_DATE_EPOCH` (seconds) when set, so sample
/// output (README transcripts, tests) is reproducible.
pub fn now_iso() -> String {
    let fixed = std::env::var("SOURCE_DATE_EPOCH").ok().and_then(|s| s.trim().parse::<i64>().ok());
    format_iso(fixed.and_then(|s| DateTime::from_timestamp(s, 0)).unwrap_or_else(Utc::now))
}

fn format_iso(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Create DIR if missing and make it owner-only (0700 on Unix), so the
/// sealed files inside are not even listable by other local users.
pub fn private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_epoch_seconds() {
        assert_eq!(format_iso(DateTime::from_timestamp(1_790_000_000, 0).unwrap()), "2026-09-21T14:13:20Z");
    }
}
