//! Battery state via UPower DBus.
//!
//! Subscribes to `org.freedesktop.UPower` `/org/freedesktop/UPower/devices/DisplayDevice`
//! and pushes a fresh `BatteryInfo` to `Drag::set_battery` whenever
//! `Properties.PropertiesChanged` fires. Uses `gio::DBusProxy` (already in tree
//! via gtk4 → glib → gio), no new dep — the Surface Go 3 doesn't need a second
//! DBus runtime when the GLib mainloop already pumps one.
//!
//! Runs entirely on the GTK main thread. The proxy auto-refreshes its property
//! cache on incoming PropertiesChanged signals; our `g-properties-changed`
//! listener simply re-reads from the cache and pushes to `Drag`.

use std::rc::Rc;

use gtk4::gio::{self, prelude::*};
use gtk4::glib;

use crate::drag::Drag;

const UPOWER_BUS: &str = "org.freedesktop.UPower";
const DISPLAY_DEVICE_PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const DEVICE_IFACE: &str = "org.freedesktop.UPower.Device";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatteryState {
    Unknown,
    Charging,
    Discharging,
    Empty,
    FullyCharged,
    PendingCharge,
    PendingDischarge,
}

impl From<u32> for BatteryState {
    fn from(v: u32) -> Self {
        match v {
            1 => Self::Charging,
            2 => Self::Discharging,
            3 => Self::Empty,
            4 => Self::FullyCharged,
            5 => Self::PendingCharge,
            6 => Self::PendingDischarge,
            _ => Self::Unknown,
        }
    }
}

impl BatteryState {
    pub fn is_charging(self) -> bool {
        matches!(self, Self::Charging | Self::PendingCharge)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BatteryInfo {
    pub percentage: f64,
    pub state: BatteryState,
    pub time_to_empty_s: i64,
    pub time_to_full_s: i64,
}

/// Human-readable status line for the Phase 6d battery popover. Format:
///   `Charging — 1h 04m to full`
///   `Discharging — 3h 12m left`
///   `Charging` / `Discharging` (when UPower reports time = 0, i.e. unknown)
///   `Fully charged` / `Empty` / `Unknown`
pub fn format_battery_status(info: &BatteryInfo) -> String {
    match info.state {
        BatteryState::FullyCharged => "Fully charged".to_string(),
        BatteryState::Charging | BatteryState::PendingCharge => {
            if info.time_to_full_s > 0 {
                format!("Charging — {} to full", format_hm(info.time_to_full_s))
            } else {
                "Charging".to_string()
            }
        }
        BatteryState::Discharging | BatteryState::PendingDischarge => {
            if info.time_to_empty_s > 0 {
                format!("Discharging — {} left", format_hm(info.time_to_empty_s))
            } else {
                "Discharging".to_string()
            }
        }
        BatteryState::Empty => "Empty".to_string(),
        BatteryState::Unknown => "Unknown".to_string(),
    }
}

fn format_hm(secs: i64) -> String {
    let s = secs.max(0);
    let h = s / 3600;
    let m = (s % 3600) / 60;
    format!("{h}h {m:02}m")
}

/// Round 0..=100 to nearest 10 with traditional ".5 rounds up". UPower
/// normally reports integer-valued doubles; we truncate to int first so the
/// integer-arithmetic formula `(p + 5) / 10 * 10` from the v1-plan applies
/// verbatim. 54 → 50, 55 → 60, 4.x → 0, 5.0 → 10.
pub fn rounded_to_ten(percentage: f64) -> u32 {
    let p = percentage.clamp(0.0, 100.0) as u32;
    ((p + 5) / 10) * 10
}

/// Spawn the UPower proxy + properties-changed listener on the GTK main loop.
/// Best-effort — if the system bus or UPower isn't reachable, the daemon logs
/// once and continues without battery data. The info section then renders its
/// "no battery" placeholder.
pub fn spawn(drag: Rc<Drag>) {
    glib::spawn_future_local(async move {
        let proxy = match gio::DBusProxy::for_bus_future(
            gio::BusType::System,
            gio::DBusProxyFlags::NONE,
            None,
            UPOWER_BUS,
            DISPLAY_DEVICE_PATH,
            DEVICE_IFACE,
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("sliver-dock: upower proxy failed: {e}");
                return;
            }
        };

        if let Some(info) = read_info(&proxy) {
            drag.set_battery(info);
        }

        let drag_for_signal = drag.clone();
        let proxy_for_signal = proxy.clone();
        // `g-properties-changed` fires after the proxy has already merged the
        // changed dict into its cache, so we ignore the args and just re-read.
        // The cached f64 / u32 / i64 reads are zero-allocation.
        proxy.connect_local("g-properties-changed", false, move |_args| {
            if let Some(info) = read_info(&proxy_for_signal) {
                drag_for_signal.set_battery(info);
            }
            None
        });

        // Hold the proxy for the daemon's lifetime — dropping it would
        // unsubscribe. `spawn_future_local` keeps the future alive across
        // awaits, but past the last await the future ends; we therefore
        // forget the proxy by leaking the closure connection above (the
        // closure owns `proxy_for_signal`, which keeps the proxy alive).
        // Nothing else to do here.
        drop(proxy);
    });
}

fn read_info(proxy: &gio::DBusProxy) -> Option<BatteryInfo> {
    let percentage = proxy.cached_property("Percentage")?.get::<f64>()?;
    let state_u32 = proxy.cached_property("State")?.get::<u32>()?;
    let time_to_empty_s = proxy
        .cached_property("TimeToEmpty")
        .and_then(|v| v.get::<i64>())
        .unwrap_or(0);
    let time_to_full_s = proxy
        .cached_property("TimeToFull")
        .and_then(|v| v.get::<i64>())
        .unwrap_or(0);
    Some(BatteryInfo {
        percentage,
        state: BatteryState::from(state_u32),
        time_to_empty_s,
        time_to_full_s,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_boundaries() {
        assert_eq!(rounded_to_ten(0.0), 0);
        assert_eq!(rounded_to_ten(4.9), 0);
        assert_eq!(rounded_to_ten(5.0), 10);
        assert_eq!(rounded_to_ten(54.0), 50);
        assert_eq!(rounded_to_ten(54.9), 50);
        assert_eq!(rounded_to_ten(55.0), 60);
        assert_eq!(rounded_to_ten(94.0), 90);
        assert_eq!(rounded_to_ten(95.0), 100);
        assert_eq!(rounded_to_ten(99.0), 100);
        assert_eq!(rounded_to_ten(100.0), 100);
        assert_eq!(rounded_to_ten(123.0), 100);
        assert_eq!(rounded_to_ten(-5.0), 0);
    }

    #[test]
    fn state_from_u32() {
        assert_eq!(BatteryState::from(1), BatteryState::Charging);
        assert_eq!(BatteryState::from(2), BatteryState::Discharging);
        assert_eq!(BatteryState::from(99), BatteryState::Unknown);
    }

    fn info(state: BatteryState, ttl_empty: i64, ttl_full: i64) -> BatteryInfo {
        BatteryInfo {
            percentage: 50.0,
            state,
            time_to_empty_s: ttl_empty,
            time_to_full_s: ttl_full,
        }
    }

    #[test]
    fn format_hm_boundaries() {
        assert_eq!(format_hm(0), "0h 00m");
        assert_eq!(format_hm(59), "0h 00m");
        assert_eq!(format_hm(60), "0h 01m");
        assert_eq!(format_hm(3599), "0h 59m");
        assert_eq!(format_hm(3600), "1h 00m");
        assert_eq!(format_hm(3720), "1h 02m");
        assert_eq!(format_hm(11532), "3h 12m");
        assert_eq!(format_hm(-100), "0h 00m");
    }

    #[test]
    fn battery_status_charging() {
        assert_eq!(
            format_battery_status(&info(BatteryState::Charging, 0, 3720)),
            "Charging — 1h 02m to full"
        );
        assert_eq!(
            format_battery_status(&info(BatteryState::PendingCharge, 0, 600)),
            "Charging — 0h 10m to full"
        );
    }

    #[test]
    fn battery_status_discharging() {
        assert_eq!(
            format_battery_status(&info(BatteryState::Discharging, 11532, 0)),
            "Discharging — 3h 12m left"
        );
        assert_eq!(
            format_battery_status(&info(BatteryState::PendingDischarge, 60, 0)),
            "Discharging — 0h 01m left"
        );
    }

    #[test]
    fn battery_status_unknown_time_drops_suffix() {
        assert_eq!(
            format_battery_status(&info(BatteryState::Charging, 0, 0)),
            "Charging"
        );
        assert_eq!(
            format_battery_status(&info(BatteryState::Discharging, 0, 0)),
            "Discharging"
        );
    }

    #[test]
    fn battery_status_static_states() {
        assert_eq!(
            format_battery_status(&info(BatteryState::FullyCharged, 0, 0)),
            "Fully charged"
        );
        assert_eq!(
            format_battery_status(&info(BatteryState::Empty, 0, 0)),
            "Empty"
        );
        assert_eq!(
            format_battery_status(&info(BatteryState::Unknown, 0, 0)),
            "Unknown"
        );
    }
}
