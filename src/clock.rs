use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix_seconds() -> i64 {
    let Ok(duration) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return 0;
    };

    duration.as_secs().min(i64::MAX as u64) as i64
}
