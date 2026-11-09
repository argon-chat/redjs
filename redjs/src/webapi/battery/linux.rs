//! UPower's display device is the OS's unified view of system batteries / UPS.
//! https://upower.freedesktop.org/docs/UPower/ (GetDisplayDevice)
//! https://upower.freedesktop.org/docs/Device.html (property units / sentinels)

use super::BatteryState;

#[derive(Clone, Copy, Debug)]
struct DisplayDevice {
    present: bool,
    kind: u32,
    state: u32,
    percentage: f64,
    time_to_empty: i64,
    time_to_full: i64,
}

impl Default for DisplayDevice {
    fn default() -> Self {
        Self { present: false, kind: 0, state: 0, percentage: f64::NAN, time_to_empty: 0, time_to_full: 0 }
    }
}

impl From<DisplayDevice> for BatteryState {
    fn from(s: DisplayDevice) -> Self {
        // DisplayDevice already aggregates batteries and excludes peripherals.
        if !s.present || !matches!(s.kind, 2 | 3) {
            return Self::default();
        }
        let full = s.state == 4;
        let empty = s.state == 3;
        let charging = !matches!(s.state, 2 | 3 | 6);
        let level = if full {
            1.0
        } else if empty {
            0.0
        } else if s.percentage.is_finite() {
            s.percentage.clamp(0.0, 100.0).round() / 100.0
        } else {
            1.0
        };
        Self {
            charging,
            charging_time: if full || (s.percentage.is_finite() && s.percentage >= 100.0) {
                0.0
            } else if matches!(s.state, 1 | 5) && s.time_to_full > 0 {
                s.time_to_full as f64
            } else {
                f64::INFINITY
            },
            discharging_time: if empty {
                0.0
            } else if !charging && s.time_to_empty > 0 {
                s.time_to_empty as f64
            } else {
                f64::INFINITY
            },
            level,
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) mod native {
    use super::super::publish;
    use super::*;
    use crate::context::ContextId;
    use futures_lite::{future, stream, StreamExt};
    use std::{collections::HashMap, time::Duration};
    use zbus::{message::Type, zvariant::OwnedValue, Connection, MatchRule, MessageStream};

    const SERVICE: &str = "org.freedesktop.UPower";
    const DISPLAY: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
    const DEVICE: &str = "org.freedesktop.UPower.Device";
    const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

    pub(crate) const ASYNC_INITIAL: bool = true;
    // Acquisition must never issue a D-Bus request on the JS thread.
    pub(crate) fn read() -> BatteryState {
        BatteryState::default()
    }

    pub(crate) struct Subscription {
        stop: async_channel::Sender<()>,
    }
    impl Drop for Subscription {
        fn drop(&mut self) {
            // Closing wakes the cancellation future, even during connection /
            // authentication / GetAll. No join or blocking IPC on the JS thread.
            self.stop.close();
        }
    }

    async fn snapshot(connection: &Connection) -> zbus::Result<BatteryState> {
        let reply = connection.call_method(Some(SERVICE), DISPLAY, Some(PROPERTIES), "GetAll", &(DEVICE,)).await?;
        let props = reply.body().deserialize::<HashMap<String, OwnedValue>>()?;
        Ok(DisplayDevice {
            present: props.get("IsPresent").and_then(|v| bool::try_from(v).ok()).unwrap_or(false),
            kind: props.get("Type").and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
            state: props.get("State").and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
            percentage: props.get("Percentage").and_then(|v| f64::try_from(v).ok()).unwrap_or(f64::NAN),
            time_to_empty: props.get("TimeToEmpty").and_then(|v| i64::try_from(v).ok()).unwrap_or(0),
            time_to_full: props.get("TimeToFull").and_then(|v| i64::try_from(v).ok()).unwrap_or(0),
        }
        .into())
    }

    async fn monitor(id: ContextId) -> zbus::Result<()> {
        let connection = zbus::connection::Builder::system()?.method_timeout(Duration::from_secs(3)).build().await?;
        let property_rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(SERVICE)?
            .path(DISPLAY)?
            .interface(PROPERTIES)?
            .member("PropertiesChanged")?
            .arg(0, DEVICE)?
            .build();
        let owner_rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .sender("org.freedesktop.DBus")?
            .interface("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .arg(0, SERVICE)?
            .build();
        // Subscribe before reading, so a change during the initial GetAll is
        // observed. The two bounded streams also cover daemon exit / restart.
        let properties = MessageStream::for_match_rule(property_rule, &connection, Some(16)).await?;
        let owners = MessageStream::for_match_rule(owner_rule, &connection, Some(4)).await?;
        let mut changes = stream::or(properties, owners);
        publish(id, snapshot(&connection).await.unwrap_or_default());
        while let Some(change) = changes.next().await {
            change?;
            // Fetch all attributes together: invalidated D-Bus properties and
            // multi-property changes must not produce a mixed native snapshot.
            publish(id, snapshot(&connection).await.unwrap_or_default());
        }
        // A clean connection EOF can end both streams without an error item.
        publish(id, BatteryState::default());
        Ok(())
    }

    pub(crate) fn subscribe(id: ContextId) -> Subscription {
        let (stop, cancelled) = async_channel::bounded(1);
        let spawned = std::thread::Builder::new().name("redjs-battery-upower".into()).spawn(move || {
            future::block_on(future::race(
                async {
                    if let Err(error) = monitor(id).await {
                        eprintln!("[redjs] battery: UPower unavailable ({error}); using specification defaults");
                        publish(id, BatteryState::default());
                    }
                },
                async {
                    let _ = cancelled.recv().await;
                },
            ));
            // Dropping the losing future drops signal streams / connection.
        });
        if let Err(error) = spawned {
            eprintln!("[redjs] battery: could not start UPower subscription ({error})");
            publish(id, BatteryState::default());
        }
        Subscription { stop }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn device(state: u32) -> DisplayDevice {
        DisplayDevice { present: true, kind: 2, state, percentage: 49.6, time_to_empty: 7200, time_to_full: 1800 }
    }
    #[test]
    fn upower_states_and_seconds() {
        let dc = BatteryState::from(device(2));
        assert_eq!(dc, BatteryState { charging: false, charging_time: f64::INFINITY, discharging_time: 7200.0, level: 0.5 });
        assert_eq!(BatteryState::from(device(6)), dc);
        for state in [1, 5] {
            assert_eq!(
                BatteryState::from(device(state)),
                BatteryState { charging: true, charging_time: 1800.0, discharging_time: f64::INFINITY, level: 0.5 }
            );
        }
        assert_eq!(BatteryState::from(device(4)), BatteryState::default());
        let empty = BatteryState::from(device(3));
        assert!(!empty.charging && empty.level == 0.0 && empty.discharging_time == 0.0);
    }
    #[test]
    fn upower_absent_peripheral_and_ups() {
        assert_eq!(BatteryState::from(DisplayDevice::default()), BatteryState::default());
        assert_eq!(BatteryState::from(DisplayDevice { present: false, ..device(2) }), BatteryState::default());
        assert_eq!(BatteryState::from(DisplayDevice { kind: 5, ..device(2) }), BatteryState::default());
        assert_eq!(BatteryState::from(DisplayDevice { kind: 3, ..device(2) }), BatteryState::from(device(2)));
    }
    #[test]
    fn upower_missing_and_invalid_values_do_not_become_zero_estimates() {
        let s = BatteryState::from(DisplayDevice { percentage: f64::NAN, time_to_empty: 0, ..device(2) });
        assert!(!s.charging && s.level == 1.0 && s.discharging_time.is_infinite());
        let s = BatteryState::from(DisplayDevice { time_to_full: -1, ..device(1) });
        assert!(s.charging_time.is_infinite());
        assert_eq!(BatteryState::from(DisplayDevice { percentage: -10.0, ..device(2) }).level, 0.0);
        assert_eq!(BatteryState::from(DisplayDevice { percentage: 150.0, ..device(1) }).level, 1.0);
        assert!(BatteryState::from(DisplayDevice { percentage: f64::INFINITY, ..device(0) }).level.is_finite());
    }
}
