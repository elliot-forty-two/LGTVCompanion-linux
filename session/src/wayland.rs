use std::collections::{BTreeMap, BTreeSet};

use lgtv_companion_runtime::{
    ipc::{DisplayPower, SessionSnapshot},
    tv::Result,
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{wl_output, wl_registry, wl_seat},
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1 as idle, ext_idle_notifier_v1 as notifier,
};
use wayland_protocols_plasma::dpms::client::{
    org_kde_kwin_dpms as kde, org_kde_kwin_dpms_manager as kde_manager,
};
use wayland_protocols_wlr::output_power_management::v1::client::{
    zwlr_output_power_manager_v1 as wlr_manager, zwlr_output_power_v1 as wlr,
};

pub struct Backend {
    queue: EventQueue<State>,
    state: State,
}

struct Output {
    proxy: wl_output::WlOutput,
    name: Option<String>,
    power: DisplayPower,
    kde_supported: bool,
    kde_mode: DisplayPower,
    observer: Option<Observer>,
}

enum Observer {
    Wlr(wlr::ZwlrOutputPowerV1),
    Kde(kde::OrgKdeKwinDpms),
}

#[derive(Default)]
struct State {
    outputs: BTreeMap<u32, Output>,
    seats: BTreeMap<u32, wl_seat::WlSeat>,
    wlr: Option<(u32, wlr_manager::ZwlrOutputPowerManagerV1)>,
    kde: Option<(u32, kde_manager::OrgKdeKwinDpmsManager)>,
    notifier: Option<(u32, notifier::ExtIdleNotifierV1)>,
    idle_watch: Option<(u32, u32, idle::ExtIdleNotificationV1)>,
    is_idle: Option<bool>,
}

impl Backend {
    pub fn turn_displays_off(&mut self) -> Result<bool> {
        let mut requested = false;
        for output in self.state.outputs.values() {
            if output.power != DisplayPower::On {
                continue;
            }
            match &output.observer {
                Some(Observer::Wlr(observer)) => observer.set_mode(wlr::Mode::Off),
                Some(Observer::Kde(observer)) => observer.set(3),
                None => continue,
            }
            requested = true;
        }
        if requested {
            self.queue.roundtrip(&mut self.state)?;
        }
        Ok(requested)
    }

    pub fn connect() -> Result<Self> {
        Self::from_connection(Connection::connect_to_env()?)
    }

    fn from_connection(connection: Connection) -> Result<Self> {
        let mut queue = connection.new_event_queue();
        let mut state = State::default();
        connection.display().get_registry(&queue.handle(), ());
        queue.roundtrip(&mut state)?;
        Ok(Self { queue, state })
    }

    pub fn sample(&mut self, idle_timeout_ms: Option<u32>) -> Result<SessionSnapshot> {
        self.queue.roundtrip(&mut self.state)?;
        let qh = self.queue.handle();
        self.state.ensure_observers(&qh);
        self.state.set_idle_watch(idle_timeout_ms, &qh);
        self.queue.roundtrip(&mut self.state)?;
        Ok(self.state.snapshot())
    }
}

#[cfg(test)]
#[path = "wayland_fixture.rs"]
mod fixture;

impl State {
    fn ensure_observers(&mut self, qh: &QueueHandle<Self>) {
        for (id, output) in &mut self.outputs {
            if output.observer.is_none() {
                output.observer = if let Some((_, manager)) = &self.wlr {
                    Some(Observer::Wlr(manager.get_output_power(
                        &output.proxy,
                        qh,
                        *id,
                    )))
                } else if let Some((_, manager)) = &self.kde {
                    Some(Observer::Kde(manager.get(&output.proxy, qh, *id)))
                } else {
                    None
                };
            }
        }
    }

    fn set_idle_watch(&mut self, timeout: Option<u32>, qh: &QueueHandle<Self>) {
        let seat_id = (self.seats.len() == 1)
            .then(|| self.seats.first_key_value())
            .flatten()
            .map(|(id, _)| *id);
        let desired = timeout.zip(seat_id).filter(|_| self.notifier.is_some());
        if self
            .idle_watch
            .as_ref()
            .map(|(timeout, seat, _)| (*timeout, *seat))
            == desired
        {
            return;
        }
        self.clear_idle_watch();
        if let Some((timeout, seat)) = desired
            && let Some((_, notifier)) = &self.notifier
        {
            let watch = notifier.get_idle_notification(timeout, &self.seats[&seat], qh, ());
            self.idle_watch = Some((timeout, seat, watch));
            self.is_idle = Some(false);
        }
    }

    fn clear_idle_watch(&mut self) {
        if let Some((_, _, watch)) = self.idle_watch.take() {
            watch.destroy();
        }
        self.is_idle = None;
    }

    fn snapshot(&self) -> SessionSnapshot {
        let names: Option<BTreeSet<_>> = self
            .outputs
            .values()
            .map(|output| output.name.clone())
            .collect();
        let names_are_unique = names
            .as_ref()
            .is_some_and(|names| names.len() == self.outputs.len());
        let have_power_protocol = self.wlr.is_some() || self.kde.is_some();
        let output_power = (names_are_unique && have_power_protocol).then(|| {
            self.outputs
                .values()
                .filter_map(|output| output.name.clone().map(|name| (name, output.power)))
                .collect()
        });
        SessionSnapshot {
            power: aggregate_power(self.outputs.values().map(|output| output.power)),
            active_outputs: names
                .filter(|_| names_are_unique)
                .map(|names| names.into_iter().collect()),
            output_power,
            idle: self.is_idle,
        }
    }
}

fn aggregate_power(outputs: impl IntoIterator<Item = DisplayPower>) -> DisplayPower {
    let mut seen_off = false;
    let mut seen_unknown = false;
    for power in outputs {
        match power {
            DisplayPower::On => return DisplayPower::On,
            DisplayPower::Off => seen_off = true,
            DisplayPower::Unknown => seen_unknown = true,
        }
    }
    if seen_off && !seen_unknown {
        DisplayPower::Off
    } else {
        DisplayPower::Unknown
    }
}

fn remove_observer(output: &mut Output) {
    match output.observer.take() {
        Some(Observer::Wlr(observer)) => observer.destroy(),
        Some(Observer::Kde(observer)) => observer.release(),
        None => {}
    }
    output.power = DisplayPower::Unknown;
    output.kde_supported = false;
    output.kde_mode = DisplayPower::Unknown;
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_output" => {
                    state.outputs.insert(
                        name,
                        Output {
                            proxy: registry.bind(name, version.min(4), qh, name),
                            name: None,
                            power: DisplayPower::Unknown,
                            kde_supported: false,
                            kde_mode: DisplayPower::Unknown,
                            observer: None,
                        },
                    );
                }
                "wl_seat" => {
                    state
                        .seats
                        .insert(name, registry.bind(name, version.min(2), qh, ()));
                }
                "zwlr_output_power_manager_v1" if state.wlr.is_none() => {
                    for output in state.outputs.values_mut() {
                        remove_observer(output);
                    }
                    state.wlr = Some((name, registry.bind(name, 1, qh, ())));
                }
                "org_kde_kwin_dpms_manager" if state.kde.is_none() => {
                    state.kde = Some((name, registry.bind(name, 1, qh, ())));
                }
                "ext_idle_notifier_v1" if state.notifier.is_none() => {
                    state.notifier = Some((name, registry.bind(name, 1, qh, ())));
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(mut output) = state.outputs.remove(&name) {
                    remove_observer(&mut output);
                    if output.proxy.version() >= 3 {
                        output.proxy.release();
                    }
                }
                if state.seats.remove(&name).is_some() {
                    state.clear_idle_watch();
                }
                if state.notifier.as_ref().is_some_and(|(id, _)| *id == name) {
                    state.clear_idle_watch();
                    if let Some((_, notifier)) = state.notifier.take() {
                        notifier.destroy();
                    }
                }
                if state.wlr.as_ref().is_some_and(|(id, _)| *id == name) {
                    for output in state.outputs.values_mut() {
                        remove_observer(output);
                    }
                    if let Some((_, manager)) = state.wlr.take() {
                        manager.destroy();
                    }
                }
                if state.kde.as_ref().is_some_and(|(id, _)| *id == name) {
                    state.kde = None;
                    if state.wlr.is_none() {
                        for output in state.outputs.values_mut() {
                            remove_observer(output);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(output) = state.outputs.get_mut(id)
        {
            output.name = Some(name);
        }
    }
}

impl Dispatch<wlr::ZwlrOutputPowerV1, u32> for State {
    fn event(
        state: &mut Self,
        observer: &wlr::ZwlrOutputPowerV1,
        event: wlr::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(id) else {
            return;
        };
        if !matches!(&output.observer, Some(Observer::Wlr(current)) if current == observer) {
            return;
        }
        output.power = match event {
            wlr::Event::Mode {
                mode: WEnum::Value(wlr::Mode::On),
            } => DisplayPower::On,
            wlr::Event::Mode {
                mode: WEnum::Value(wlr::Mode::Off),
            } => DisplayPower::Off,
            _ => DisplayPower::Unknown,
        };
    }
}

impl Dispatch<kde::OrgKdeKwinDpms, u32> for State {
    fn event(
        state: &mut Self,
        observer: &kde::OrgKdeKwinDpms,
        event: kde::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(id) else {
            return;
        };
        if !matches!(&output.observer, Some(Observer::Kde(current)) if current == observer) {
            return;
        }
        match event {
            kde::Event::Supported { supported } => output.kde_supported = supported == 1,
            kde::Event::Mode { mode } => output.kde_mode = normalize_dpms(mode as i32),
            kde::Event::Done => {
                output.power = if output.kde_supported {
                    output.kde_mode
                } else {
                    DisplayPower::Unknown
                };
            }
            _ => {}
        }
    }
}

impl Dispatch<idle::ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        watch: &idle::ExtIdleNotificationV1,
        event: idle::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !state
            .idle_watch
            .as_ref()
            .is_some_and(|(_, _, current)| current == watch)
        {
            return;
        }
        state.is_idle = match event {
            idle::Event::Idled => Some(true),
            idle::Event::Resumed => Some(false),
            _ => state.is_idle,
        };
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore wlr_manager::ZwlrOutputPowerManagerV1);
delegate_noop!(State: ignore kde_manager::OrgKdeKwinDpmsManager);
delegate_noop!(State: ignore notifier::ExtIdleNotifierV1);

pub fn normalize_dpms(value: i32) -> DisplayPower {
    match value {
        0 => DisplayPower::On,
        1..=3 => DisplayPower::Off,
        _ => DisplayPower::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_power_does_not_treat_unobservable_outputs_as_off() {
        use DisplayPower::*;
        assert_eq!(aggregate_power([Off, Unknown]), Unknown);
        assert_eq!(aggregate_power([On, Unknown]), On);
        assert_eq!(aggregate_power([Off, Off]), Off);
        assert_eq!(aggregate_power([]), Unknown);
    }
}
