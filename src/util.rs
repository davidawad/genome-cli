//! Small shared helpers.

use chrono::Utc;

pub fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
