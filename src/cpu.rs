//! CPU usage from `/proc/stat` aggregate row.
//!
//! `spawn(Drag)` arms a 1Hz `glib::timeout_add_seconds_local` that reads
//! `/proc/stat`, computes a delta against the prior tick, and pushes a
//! `CpuInfo` to `Drag::set_cpu`. No external deps; the file is ~200 bytes
//! and reading it once a second on a Surface Go 3 is negligible.
//!
//! The first tick has no prior sample, so it just records the baseline and
//! pushes `0%`. Subsequent ticks emit real percentages.

use std::cell::Cell;
use std::rc::Rc;

use gtk4::glib;

use crate::drag::Drag;

/// Snapshot of `/proc/stat`'s `cpu` aggregate row, in jiffies. Fields after
/// `softirq` are kernel ≥ 2.6.11 / 2.6.24 / 2.6.33 additions; we tolerate
/// short rows by treating missing fields as zero.
#[derive(Clone, Copy, Debug, Default)]
struct Sample {
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

pub fn spawn(drag: Rc<Drag>) {
    let prior: Rc<Cell<Option<Sample>>> = Rc::new(Cell::new(None));

    // Take the baseline immediately so the first emitted reading (1s later)
    // covers a real interval rather than the time-since-boot accumulation.
    if let Some(s) = read_sample() {
        prior.set(Some(s));
    }

    glib::timeout_add_seconds_local(1, move || {
        let Some(now) = read_sample() else {
            return glib::ControlFlow::Continue;
        };
        let info = match prior.get() {
            Some(prev) => CpuInfo {
                percent: percent_between(prev, now),
            },
            None => CpuInfo { percent: 0 },
        };
        prior.set(Some(now));
        drag.set_cpu(info);
        glib::ControlFlow::Continue
    });
}

fn read_sample() -> Option<Sample> {
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

fn percent_between(prev: Sample, now: Sample) -> u32 {
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
