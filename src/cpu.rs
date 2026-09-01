//! CPU usage from `/proc/stat` aggregate row — pure, timer-free.
//!
//! `Drag` owns the sampler's 1Hz `glib::timeout_add_seconds_local` lifecycle
//! (slice 008: started/stopped from `set_visual` so no `/proc/stat` read
//! happens while the info section is off screen). This module supplies the
//! pure pieces it builds on: reading + parsing the aggregate row, the delta
//! percentage, and the [`Sample`] baseline type. The first reading after a
//! (re)start has no prior sample, so `Drag` records a fresh baseline and
//! emits `0%`; subsequent ticks emit real percentages.

/// Snapshot of `/proc/stat`'s `cpu` aggregate row, in jiffies. Fields after
/// `softirq` are kernel ≥ 2.6.11 / 2.6.24 / 2.6.33 additions; we tolerate
/// short rows by treating missing fields as zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Sample {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
    steal: u64,
}

impl Sample {
    fn idle_total(&self) -> u64 {
        self.idle.saturating_add(self.iowait)
    }
    fn busy_total(&self) -> u64 {
        // guest / guest_nice are already counted in user / nice, so we
        // don't sum them in. See kernel `Documentation/filesystems/proc.rst`.
        self.user
            .saturating_add(self.nice)
            .saturating_add(self.system)
            .saturating_add(self.irq)
            .saturating_add(self.softirq)
            .saturating_add(self.steal)
    }
    fn total(&self) -> u64 {
        self.idle_total().saturating_add(self.busy_total())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuInfo {
    pub percent: u32,
}

/// Read + parse the current `/proc/stat` aggregate row. `None` when the file
/// is unreadable or its first line isn't the `cpu` aggregate — the sampler
/// then skips this tick (the prior baseline stays put).
pub(crate) fn read_sample() -> Option<Sample> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    parse_aggregate(&text)
}

fn parse_aggregate(text: &str) -> Option<Sample> {
    let line = text.lines().next()?;
    let mut fields = line.split_ascii_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let mut nums = fields.map(|f| f.parse::<u64>().unwrap_or(0));
    Some(Sample {
        user: nums.next().unwrap_or(0),
        nice: nums.next().unwrap_or(0),
        system: nums.next().unwrap_or(0),
        idle: nums.next().unwrap_or(0),
        iowait: nums.next().unwrap_or(0),
        irq: nums.next().unwrap_or(0),
        softirq: nums.next().unwrap_or(0),
        steal: nums.next().unwrap_or(0),
    })
}

pub(crate) fn percent_between(prev: Sample, now: Sample) -> u32 {
    let total_d = now.total().saturating_sub(prev.total());
    if total_d == 0 {
        return 0;
    }
    let busy_d = now.busy_total().saturating_sub(prev.busy_total());
    let pct = (busy_d as f64 / total_d as f64 * 100.0).round();
    pct.clamp(0.0, 100.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(user: u64, nice: u64, system: u64, idle: u64, iowait: u64) -> Sample {
        Sample {
            user,
            nice,
            system,
            idle,
            iowait,
            irq: 0,
            softirq: 0,
            steal: 0,
        }
    }

    #[test]
    fn aggregate_row_parses() {
        let text = "cpu  100 2 50 800 10 5 3 0 0 0\ncpu0 …\n";
        let parsed = parse_aggregate(text).unwrap();
        assert_eq!(parsed.user, 100);
        assert_eq!(parsed.nice, 2);
        assert_eq!(parsed.system, 50);
        assert_eq!(parsed.idle, 800);
        assert_eq!(parsed.iowait, 10);
        assert_eq!(parsed.irq, 5);
        assert_eq!(parsed.softirq, 3);
        assert_eq!(parsed.steal, 0);
    }

    #[test]
    fn short_row_tolerated() {
        let text = "cpu  100 0 50 800\n";
        let parsed = parse_aggregate(text).unwrap();
        assert_eq!(parsed.idle, 800);
        assert_eq!(parsed.iowait, 0);
        assert_eq!(parsed.softirq, 0);
    }

    #[test]
    fn rejects_non_cpu_line() {
        assert!(parse_aggregate("intr 1 2 3\n").is_none());
    }

    #[test]
    fn percent_zero_when_no_progress() {
        let prev = s(100, 0, 50, 800, 10);
        assert_eq!(percent_between(prev, prev), 0);
    }

    #[test]
    fn percent_full_busy() {
        let prev = s(0, 0, 0, 1000, 0);
        let now = s(100, 0, 0, 1000, 0); // 100 jiffies of user, no idle gain
        assert_eq!(percent_between(prev, now), 100);
    }

    #[test]
    fn percent_full_idle() {
        let prev = s(100, 0, 50, 1000, 0);
        let now = s(100, 0, 50, 1100, 0); // only idle moved
        assert_eq!(percent_between(prev, now), 0);
    }

    #[test]
    fn percent_half_load() {
        let prev = s(0, 0, 0, 0, 0);
        let now = s(50, 0, 0, 50, 0);
        assert_eq!(percent_between(prev, now), 50);
    }

    #[test]
    fn percent_clamps_negative_skew() {
        // If somehow `now` < `prev` (counter wrap, suspended jiffies),
        // saturating_sub keeps deltas non-negative and pct clamps to 0.
        let prev = s(100, 0, 50, 800, 10);
        let now = s(50, 0, 30, 400, 5);
        assert_eq!(percent_between(prev, now), 0);
    }
}
