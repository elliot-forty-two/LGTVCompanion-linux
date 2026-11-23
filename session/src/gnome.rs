//! Read-only fallback for Wayland compositors exposing Mutter's session APIs.

use std::{
    collections::{BTreeSet, HashMap},
    time::Duration,
};

use lgtv_companion_runtime::ipc::{DisplayPower, SessionSnapshot};
use tokio::time::timeout;
use zbus::{
    Connection, Proxy,
    proxy::{Builder, CacheProperties},
    zvariant::OwnedValue,
};

type Properties = HashMap<String, OwnedValue>;
type Identity = (String, String, String, String);
type Mode = (String, i32, i32, f64, f64, Vec<f64>, Properties);
type Monitor = (Identity, Vec<Mode>, Properties);
type LogicalMonitor = (i32, i32, f64, u32, bool, Vec<Identity>, Properties);
type Layout = (u32, Vec<Monitor>, Vec<LogicalMonitor>, Properties);

#[derive(Default)]
pub struct Mutter {
    connection: Option<Connection>,
}

impl Mutter {
    pub async fn turn_displays_off(&self) -> lgtv_companion_runtime::tv::Result<()> {
        let connection = self
            .connection
            .as_ref()
            .ok_or("Mutter session bus unavailable")?;
        timeout(Duration::from_millis(600), async {
            let display = proxy(
                connection,
                "org.gnome.Mutter.DisplayConfig",
                "/org/gnome/Mutter/DisplayConfig",
            )
            .await?;
            display.set_property("PowerSaveMode", 3i32).await?;
            Ok(())
        })
        .await?
    }

    pub async fn observe(
        &mut self,
        snapshot: &mut SessionSnapshot,
        idle_timeout_ms: Option<u32>,
    ) -> Option<u64> {
        if self.connection.is_none() {
            self.connection = timeout(Duration::from_millis(600), Connection::session())
                .await
                .ok()?
                .ok();
        }
        let connection = self.connection.as_ref()?;
        let display = async {
            const DISPLAY: &str = "org.gnome.Mutter.DisplayConfig";
            if snapshot.power != DisplayPower::Unknown || snapshot.output_power.is_some() {
                return;
            }
            let Ok(proxy) = proxy(connection, DISPLAY, "/org/gnome/Mutter/DisplayConfig").await
            else {
                return;
            };
            if let Ok(mode) = proxy.get_property::<i32>("PowerSaveMode").await {
                snapshot.power = super::wayland::normalize_dpms(mode);
            }
            if let Ok(layout) = proxy.call::<_, _, Layout>("GetCurrentState", &()).await {
                snapshot.active_outputs = Some(active_names(layout.2));
            }
        };
        let idle = async {
            if snapshot.idle.is_some() {
                return None;
            }
            let proxy = proxy(
                connection,
                "org.gnome.Mutter.IdleMonitor",
                "/org/gnome/Mutter/IdleMonitor/Core",
            )
            .await
            .ok()?;
            proxy.call::<_, _, u64>("GetIdletime", &()).await.ok()
        };
        let inhibitor = async {
            if snapshot.idle.is_some() || idle_timeout_ms.is_none() {
                return None;
            }
            let manager = proxy(
                connection,
                "org.gnome.SessionManager",
                "/org/gnome/SessionManager",
            )
            .await
            .ok()?;
            const IDLE_INHIBIT: u32 = 8;
            manager
                .call::<_, _, bool>("IsInhibited", &(IDLE_INHIBIT,))
                .await
                .ok()
        };
        let (_, elapsed, inhibited) = tokio::join!(
            timeout(Duration::from_millis(600), display),
            timeout(Duration::from_millis(600), idle),
            timeout(Duration::from_millis(600), inhibitor),
        );
        let elapsed = elapsed.ok().flatten();
        if snapshot.idle.is_none() {
            snapshot.idle = match inhibited.ok().flatten() {
                Some(true) => Some(false),
                Some(false) => elapsed
                    .zip(idle_timeout_ms)
                    .map(|(elapsed, limit)| elapsed >= u64::from(limit)),
                None => None,
            };
        }
        elapsed
    }
}

async fn proxy<'a>(
    connection: &Connection,
    interface: &'a str,
    path: &'a str,
) -> zbus::Result<Proxy<'a>> {
    Builder::new(connection)
        .destination(interface)?
        .path(path)?
        .interface(interface)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

fn active_names(monitors: Vec<LogicalMonitor>) -> Vec<String> {
    monitors
        .into_iter()
        .flat_map(|(_, _, _, _, _, monitors, _)| monitors)
        .map(|(connector, _, _, _)| connector)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloned_mutter_monitors_both_count_as_active() {
        let identity = |connector: &str| {
            (
                connector.to_owned(),
                String::new(),
                String::new(),
                String::new(),
            )
        };
        let layout = vec![(
            0,
            0,
            1.0,
            0,
            true,
            vec![identity("DP-1"), identity("HDMI-1")],
            HashMap::new(),
        )];
        assert_eq!(active_names(layout), ["DP-1", "HDMI-1"]);
    }

    #[test]
    fn mutter_layout_matches_its_dbus_signature() {
        use zbus::zvariant::Type;
        assert_eq!(
            Layout::SIGNATURE.to_string(),
            "(ua((ssss)a(siiddada{sv})a{sv})a(iiduba(ssss)a{sv})a{sv})"
        );
    }
}
