use std::time::Duration;

use lgtv_companion_core::streaming::{
    Action, EndMode, Generation, HostDisplay, Mode, StreamSessions, Streaming, Transition, TvPower,
};

fn generation(action: &Action) -> Generation {
    match action {
        Action::PowerOffWithCapture { generation, .. } => *generation,
        other => panic!("Expected a capture action, got {other:?}"),
    }
}

fn wake(device: &str, allow_hdmi_switch: bool) -> Action {
    Action::Wake {
        device: device.into(),
        allow_hdmi_switch,
    }
}

#[test]
fn restore_wakes_active_and_screen_off_tvs_only() {
    let devices = ["active", "blank", "standby", "unknown", "no reply"].map(String::from);
    let mut policy = Streaming::default();
    let actions = policy.connect(Mode::PowerOff(EndMode::Restore), HostDisplay::On, &devices);
    let stream = generation(&actions[0]);

    assert_eq!(
        actions,
        devices
            .iter()
            .cloned()
            .map(|device| Action::PowerOffWithCapture {
                generation: stream,
                device,
            })
            .collect::<Vec<_>>()
    );

    policy.record_pre_stream_power(stream, "active", TvPower::Active);
    policy.record_pre_stream_power(stream, "blank", TvPower::ScreenOff);
    policy.record_pre_stream_power(stream, "standby", TvPower::Standby);
    policy.record_pre_stream_power(stream, "unknown", TvPower::Unknown);
    policy.record_powered_off(stream, "active");
    // Later replies must leave the first snapshot intact.
    policy.record_pre_stream_power(stream, "active", TvPower::Standby);

    assert_eq!(
        policy.disconnect(HostDisplay::On, &devices),
        [wake("active", true), wake("blank", true)]
    );
    assert!(!policy.is_active());
    assert!(policy.disconnect(HostDisplay::On, &devices).is_empty());
}

#[test]
fn keep_off_requests_host_display_off_only_after_verified_tv_power_off() {
    let devices = ["tv".to_owned()];
    let mut policy = Streaming::default();
    let mode = Mode::PowerOff(EndMode::KeepOff);
    let actions = policy.connect(mode, HostDisplay::On, &devices);
    policy.record_pre_stream_power(generation(&actions[0]), "tv", TvPower::Active);
    assert!(policy.disconnect(HostDisplay::On, &devices).is_empty());

    let actions = policy.connect(mode, HostDisplay::On, &devices);
    policy.record_powered_off(generation(&actions[0]), "tv");
    assert_eq!(
        policy.disconnect(HostDisplay::On, &devices),
        [Action::HostDisplaysOff]
    );
}

#[test]
fn power_on_mode_wakes_managed_tvs_without_a_snapshot() {
    let devices = ["tv".to_owned()];
    let mut policy = Streaming::default();
    policy.connect(Mode::PowerOff(EndMode::PowerOn), HostDisplay::On, &devices);
    assert_eq!(
        policy.disconnect(HostDisplay::On, &devices),
        [wake("tv", true)]
    );
}

#[test]
fn power_on_mode_never_turns_host_displays_off_after_an_override() {
    let devices = ["tv".to_owned()];
    let mut policy = Streaming::default();
    let actions = policy.connect(Mode::PowerOff(EndMode::PowerOn), HostDisplay::On, &devices);
    policy.record_powered_off(generation(&actions[0]), "tv");
    assert!(policy.disconnect(HostDisplay::On, &[]).is_empty());
}

#[test]
fn blank_mode_restores_without_switching_hdmi() {
    let devices = ["tv".to_owned()];
    let mut policy = Streaming::default();
    assert!(matches!(
        policy.connect(Mode::Blank, HostDisplay::On, &devices).as_slice(),
        [Action::Blank { device, .. }] if device == "tv"
    ));
    assert_eq!(
        policy.disconnect(HostDisplay::On, &devices),
        [wake("tv", false)]
    );
}

#[test]
fn only_an_on_display_allows_stream_power_actions() {
    let devices = ["tv".to_owned()];
    let mut policy = Streaming::default();
    for display in [HostDisplay::Off, HostDisplay::Unknown] {
        assert!(policy.connect(Mode::Blank, display, &devices).is_empty());
        assert!(policy.is_active());
        assert!(policy.disconnect(display, &devices).is_empty());
    }
}

#[test]
fn suspend_invalidates_capture_and_power_off_replies_from_the_old_stream() {
    let devices = ["tv".to_owned()];
    let mode = Mode::PowerOff(EndMode::Restore);
    let mut policy = Streaming::default();
    let old = generation(&policy.connect(mode, HostDisplay::On, &devices)[0]);
    assert!(policy.connect(mode, HostDisplay::On, &devices).is_empty());
    policy.cancel();
    assert!(policy.disconnect(HostDisplay::On, &devices).is_empty());

    let new = generation(&policy.connect(mode, HostDisplay::On, &devices)[0]);
    assert_ne!(old, new);
    policy.record_pre_stream_power(old, "tv", TvPower::Active);
    policy.record_powered_off(old, "tv");
    assert!(policy.disconnect(HostDisplay::On, &devices).is_empty());
}

#[test]
fn session_transitions_debounce_first_connect_and_last_disconnect() {
    let mut sessions = StreamSessions::default();
    let seconds = Duration::from_secs;

    sessions.report("Sunshine", "1", true, seconds(0));
    sessions.report("Steam", "1", true, seconds(4));
    sessions.report("Sunshine", "1", false, seconds(6));
    assert_eq!(sessions.deadline(), Some(seconds(10)));
    assert_eq!(sessions.advance(seconds(9)), None);
    assert_eq!(sessions.advance(seconds(10)), Some(Transition::Connected));
    assert_eq!(sessions.advance(seconds(10)), None);

    sessions.report("Sunshine", "1", false, seconds(11));
    assert_eq!(sessions.deadline(), None);
    sessions.report("Steam", "1", false, seconds(12));
    assert_eq!(sessions.deadline(), Some(seconds(13)));
    assert_eq!(
        sessions.advance(seconds(13)),
        Some(Transition::Disconnected)
    );
}

#[test]
fn session_transitions_cancel_when_state_reverses_within_the_delay() {
    let mut sessions = StreamSessions::default();
    let seconds = Duration::from_secs;

    sessions.report("Sunshine", "1", true, seconds(0));
    sessions.report("Sunshine", "1", false, seconds(2));
    assert_eq!(sessions.advance(seconds(10)), None);

    sessions.report("Sunshine", "2", true, seconds(11));
    assert_eq!(sessions.advance(seconds(21)), Some(Transition::Connected));
    sessions.report("Sunshine", "2", false, seconds(22));
    sessions.report("Steam", "1", true, Duration::from_millis(22_500));
    assert_eq!(sessions.advance(seconds(23)), None);
}
