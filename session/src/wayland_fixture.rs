use std::{
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use wayland_protocols::ext::idle_notify::v1::server::{
    ext_idle_notification_v1 as idle, ext_idle_notifier_v1 as notifier,
};
use wayland_protocols_wlr::output_power_management::v1::server::{
    zwlr_output_power_manager_v1 as power_manager, zwlr_output_power_v1 as power,
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New,
    protocol::{wl_output, wl_seat},
};

use super::{Backend, Connection, DisplayPower};

#[derive(Default)]
struct Compositor {
    idle_watch: Option<idle::ExtIdleNotificationV1>,
    idle: bool,
}

macro_rules! global {
    ($kind:path, $init:expr) => {
        impl GlobalDispatch<$kind, ()> for Compositor {
            fn bind(
                _: &mut Self,
                _: &DisplayHandle,
                _: &Client,
                resource: New<$kind>,
                _: &(),
                data: &mut DataInit<'_, Self>,
            ) {
                let resource = data.init(resource, ());
                $init(resource);
            }
        }
    };
}

macro_rules! unused_requests {
    ($kind:path, $request:path) => {
        impl Dispatch<$kind, ()> for Compositor {
            fn request(
                _: &mut Self,
                _: &Client,
                _: &$kind,
                _: $request,
                _: &(),
                _: &DisplayHandle,
                _: &mut DataInit<'_, Self>,
            ) {
            }
        }
    };
}

global!(wl_output::WlOutput, |output: wl_output::WlOutput| {
    output.name("HDMI-A-1".into());
    output.done();
});
global!(wl_seat::WlSeat, |_| {});
global!(power_manager::ZwlrOutputPowerManagerV1, |_| {});
global!(notifier::ExtIdleNotifierV1, |_| {});
unused_requests!(wl_output::WlOutput, wl_output::Request);
unused_requests!(wl_seat::WlSeat, wl_seat::Request);
unused_requests!(idle::ExtIdleNotificationV1, idle::Request);

impl Dispatch<power::ZwlrOutputPowerV1, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        output: &power::ZwlrOutputPowerV1,
        request: power::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let power::Request::SetMode {
            mode: wayland_server::WEnum::Value(mode),
        } = request
        {
            output.mode(mode);
        }
    }
}

impl Dispatch<power_manager::ZwlrOutputPowerManagerV1, ()> for Compositor {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &power_manager::ZwlrOutputPowerManagerV1,
        request: power_manager::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let power_manager::Request::GetOutputPower { id, .. } = request {
            data.init(id, ()).mode(power::Mode::On);
        }
    }
}

impl Dispatch<notifier::ExtIdleNotifierV1, ()> for Compositor {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &notifier::ExtIdleNotifierV1,
        request: notifier::Request,
        _: &(),
        _: &DisplayHandle,
        data: &mut DataInit<'_, Self>,
    ) {
        if let notifier::Request::GetIdleNotification { id, timeout, .. } = request {
            assert_eq!(timeout, 60_000);
            state.idle_watch = Some(data.init(id, ()));
        }
    }
}

#[test]
fn reads_wayland_output_power_and_idle_notifications_over_the_wire() {
    let (client, server) = UnixStream::pair().unwrap();
    let active = Arc::new(AtomicBool::new(true));
    let server_active = Arc::clone(&active);
    let test_idle = Arc::new(AtomicBool::new(false));
    let server_idle = Arc::clone(&test_idle);
    let worker = thread::spawn(move || {
        let mut display = Display::<Compositor>::new().unwrap();
        let mut handle = display.handle();
        handle.create_global::<Compositor, wl_output::WlOutput, _>(4, ());
        handle.create_global::<Compositor, wl_seat::WlSeat, _>(2, ());
        handle.create_global::<Compositor, power_manager::ZwlrOutputPowerManagerV1, _>(1, ());
        handle.create_global::<Compositor, notifier::ExtIdleNotifierV1, _>(1, ());
        handle.insert_client(server, Arc::new(())).unwrap();
        let mut compositor = Compositor::default();
        while server_active.load(Ordering::Relaxed) {
            display.dispatch_clients(&mut compositor).unwrap();
            let idle = server_idle.load(Ordering::Relaxed);
            if let Some(watch) = &compositor.idle_watch
                && compositor.idle != idle
            {
                compositor.idle = idle;
                if idle {
                    watch.idled();
                } else {
                    watch.resumed();
                }
            }
            display.flush_clients().unwrap();
            thread::sleep(Duration::from_millis(1));
        }
    });

    let mut backend = Backend::from_connection(Connection::from_socket(client).unwrap()).unwrap();
    let snapshot = backend.sample(Some(60_000)).unwrap();
    assert_eq!(snapshot.active_outputs.unwrap(), ["HDMI-A-1"]);
    assert_eq!(snapshot.output_power.unwrap()["HDMI-A-1"], DisplayPower::On);
    assert_eq!(snapshot.power, DisplayPower::On);
    assert!(backend.turn_displays_off().unwrap());
    assert_eq!(
        backend.sample(Some(60_000)).unwrap().power,
        DisplayPower::Off
    );
    assert_eq!(snapshot.idle, Some(false));

    test_idle.store(true, Ordering::Relaxed);
    assert!((0..10).any(|_| backend.sample(Some(60_000)).unwrap().idle == Some(true)));
    test_idle.store(false, Ordering::Relaxed);
    assert!((0..10).any(|_| backend.sample(Some(60_000)).unwrap().idle == Some(false)));

    active.store(false, Ordering::Relaxed);
    worker.join().unwrap();
}
