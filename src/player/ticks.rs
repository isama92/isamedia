//! Jellyfin measures positions in "ticks": 1 tick = 100 ns, so 10_000_000
//! ticks per second.

const TICKS_PER_SECOND: f64 = 10_000_000.0;

pub fn seconds_to_ticks(seconds: f64) -> i64 {
    (seconds * TICKS_PER_SECOND) as i64
}

pub fn ticks_to_seconds(ticks: i64) -> f64 {
    ticks as f64 / TICKS_PER_SECOND
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        assert_eq!(seconds_to_ticks(1.0), 10_000_000);
        assert_eq!(seconds_to_ticks(90.5), 905_000_000);
        assert_eq!(ticks_to_seconds(27_000_000_000), 2700.0);
        assert_eq!(ticks_to_seconds(seconds_to_ticks(123.456)), 123.456);
    }
}
