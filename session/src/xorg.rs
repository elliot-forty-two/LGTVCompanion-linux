use lgtv_companion_runtime::{
    ipc::{DisplayPower, SessionSnapshot},
    tv::Result,
};
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{
        dpms::{self, ConnectionExt as _, DPMSMode},
        randr::{self, ConnectionExt as _},
        screensaver::{self, ConnectionExt as _},
    },
    rust_connection::RustConnection,
};

pub struct Backend {
    connection: RustConnection,
    root: u32,
}

impl Backend {
    pub fn turn_displays_off(&self) -> Result<()> {
        let info = self.connection.dpms_info()?.reply()?;
        if !info.state {
            return Err("Xorg DPMS is disabled".into());
        }
        self.connection.dpms_force_level(DPMSMode::OFF)?.check()?;
        Ok(())
    }

    pub fn connect() -> Result<Self> {
        let display = std::env::var("DISPLAY")
            .map_err(|_| "DISPLAY is missing. Start the agent inside Xorg")?;
        if !display.starts_with(':') && !display.starts_with("unix:") {
            return Err("The session agent requires a local Xorg display".into());
        }
        let (connection, screen) = x11rb::connect(Some(&display))?;
        let root = connection.setup().roots[screen].root;
        Ok(Self { connection, root })
    }

    pub fn sample(&self, timeout_ms: Option<u32>) -> Result<(SessionSnapshot, Option<u64>)> {
        let idle = idle_ms(&self.connection, self.root).ok().flatten();
        Ok((
            SessionSnapshot {
                power: display_power(&self.connection)?,
                active_outputs: active_outputs(&self.connection, self.root).ok().flatten(),
                output_power: None,
                idle: idle
                    .zip(timeout_ms)
                    .map(|(elapsed, limit)| elapsed >= limit),
            },
            idle.map(u64::from),
        ))
    }
}

fn display_power(connection: &RustConnection) -> Result<DisplayPower> {
    if connection
        .extension_information(dpms::X11_EXTENSION_NAME)?
        .is_none()
    {
        return Ok(DisplayPower::Unknown);
    }

    let reply = connection.dpms_info()?.reply()?;
    Ok(normalize_dpms(reply.state, reply.power_level))
}

fn normalize_dpms(enabled: bool, level: DPMSMode) -> DisplayPower {
    if !enabled {
        return DisplayPower::Unknown;
    }
    match level {
        DPMSMode::ON => DisplayPower::On,
        DPMSMode::STANDBY | DPMSMode::SUSPEND | DPMSMode::OFF => DisplayPower::Off,
        _ => DisplayPower::Unknown,
    }
}

fn idle_ms(connection: &RustConnection, root: u32) -> Result<Option<u32>> {
    if connection
        .extension_information(screensaver::X11_EXTENSION_NAME)?
        .is_none()
    {
        return Ok(None);
    }
    Ok(Some(
        connection
            .screensaver_query_info(root)?
            .reply()?
            .ms_since_user_input,
    ))
}

fn active_outputs(connection: &RustConnection, root: u32) -> Result<Option<Vec<String>>> {
    if connection
        .extension_information(randr::X11_EXTENSION_NAME)?
        .is_none()
    {
        return Ok(None);
    }
    let resources = connection
        .randr_get_screen_resources_current(root)?
        .reply()?;
    let mut names = Vec::new();
    for output in resources.outputs {
        let info = connection
            .randr_get_output_info(output, resources.config_timestamp)?
            .reply()?;
        if info.status != randr::SetConfig::SUCCESS {
            return Ok(None);
        }
        if info.crtc != x11rb::NONE {
            names.push(String::from_utf8_lossy(&info.name).into_owned());
        }
    }
    names.sort_unstable();
    Ok(Some(names))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpms_distinguishes_on_off_and_unavailable() {
        assert_eq!(normalize_dpms(true, DPMSMode::ON), DisplayPower::On);
        for level in [DPMSMode::STANDBY, DPMSMode::SUSPEND, DPMSMode::OFF] {
            assert_eq!(normalize_dpms(true, level), DisplayPower::Off);
        }
        assert_eq!(normalize_dpms(false, DPMSMode::OFF), DisplayPower::Unknown);
        assert_eq!(
            normalize_dpms(true, DPMSMode::from(99u16)),
            DisplayPower::Unknown
        );
    }
}
