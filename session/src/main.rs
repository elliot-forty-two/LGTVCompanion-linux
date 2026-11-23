mod gnome;
mod wayland;
mod xorg;

use std::{
    env,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use lgtv_companion_runtime::{
    ipc::{self, DisplayPower, Method, Reply, Request, Response, SessionSnapshot},
    tv::Result,
};
use serde_json::json;
use tokio::{
    net::UnixStream,
    time::{MissedTickBehavior, interval, timeout},
};

#[derive(Clone, Copy)]
enum Kind {
    Xorg,
    Wayland,
}

enum Backend {
    Xorg(Box<xorg::Backend>),
    Wayland(Box<wayland::Backend>),
}

impl Backend {
    async fn turn_displays_off(&mut self, mutter: Option<&gnome::Mutter>) -> Result<()> {
        match self {
            Self::Xorg(backend) => backend.turn_displays_off(),
            Self::Wayland(backend) => {
                if backend.turn_displays_off()? {
                    return Ok(());
                }
                match mutter {
                    Some(mutter) => mutter.turn_displays_off().await,
                    None => Err("Wayland compositor has no usable display power control".into()),
                }
            }
        }
    }

    fn connect(kind: Kind) -> Result<Self> {
        match kind {
            Kind::Xorg => Ok(Self::Xorg(Box::new(xorg::Backend::connect()?))),
            Kind::Wayland => Ok(Self::Wayland(Box::new(wayland::Backend::connect()?))),
        }
    }

    async fn sample(
        &mut self,
        idle_timeout_ms: Option<u32>,
        mutter: Option<&mut gnome::Mutter>,
    ) -> Result<(SessionSnapshot, Option<u64>)> {
        match self {
            Self::Xorg(backend) => backend.sample(idle_timeout_ms),
            Self::Wayland(backend) => {
                let mut snapshot = backend.sample(idle_timeout_ms)?;
                let elapsed = match mutter {
                    Some(mutter) => mutter.observe(&mut snapshot, idle_timeout_ms).await,
                    None => None,
                };
                Ok((snapshot, elapsed))
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lgtv-companion-session: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let mut socket = PathBuf::from(ipc::DEFAULT_SOCKET);
    let mut diagnose = false;
    let mut args = env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                println!("Usage: lgtv-companion-session [--socket PATH | --diagnose]");
                return Ok(());
            }
            Some("--socket") => socket = args.next().ok_or("--socket requires a path")?.into(),
            Some("--diagnose") => diagnose = true,
            _ => return Err("Unknown argument. See --help".into()),
        }
    }
    let kind = match env::var("XDG_SESSION_TYPE").as_deref() {
        Ok("x11") => Kind::Xorg,
        Ok("wayland") => Kind::Wayland,
        _ if env::var_os("WAYLAND_DISPLAY").is_some() => Kind::Wayland,
        _ if env::var_os("DISPLAY").is_some() => Kind::Xorg,
        _ => return Err("Start the session agent inside Xorg or Wayland".into()),
    };
    let mut mutter = (matches!(kind, Kind::Wayland)
        && env::var("XDG_CURRENT_DESKTOP").is_ok_and(|names| {
            names
                .split(':')
                .any(|name| name.eq_ignore_ascii_case("gnome"))
        }))
    .then(gnome::Mutter::default);

    if diagnose {
        let (snapshot, idle_ms) = Backend::connect(kind)?
            .sample(Some(600_000), mutter.as_mut())
            .await?;
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "backend": match kind { Kind::Xorg => "xorg", Kind::Wayland => "wayland" },
                "display_power": snapshot.power,
                "idle_ms": idle_ms,
                "idle_notification": snapshot.idle.is_some(),
                "active_outputs": snapshot.active_outputs,
                "output_power": snapshot.output_power,
            }))?
        );
        return Ok(());
    }

    let session_id = env::var("XDG_SESSION_ID").ok();
    let mut backend = None;
    let mut last_error = None;
    let mut idle_timeout_ms = None;
    let mut sample = interval(Duration::from_secs(2));
    sample.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        sample.tick().await;
        if backend.is_none() {
            backend = Backend::connect(kind).ok();
        }
        let snapshot = match backend.as_mut() {
            Some(connected) => connected
                .sample(idle_timeout_ms, mutter.as_mut())
                .await
                .ok()
                .map(|(snapshot, _)| snapshot),
            None => None,
        };
        let snapshot = snapshot.unwrap_or_else(|| {
            backend = None;
            SessionSnapshot {
                power: DisplayPower::Unknown,
                active_outputs: None,
                output_power: None,
                idle: None,
            }
        });
        let error = match report(&socket, snapshot, session_id.clone()).await {
            Ok((setting, turn_displays_off)) => {
                idle_timeout_ms = setting;
                if turn_displays_off
                    && let Some(backend) = backend.as_mut()
                    && let Err(error) = backend.turn_displays_off(mutter.as_ref()).await
                {
                    eprintln!("lgtv-companion-session: display power-off request failed: {error}");
                }
                None
            }
            Err(error) => Some(error.to_string()),
        };
        if error != last_error {
            if let Some(error) = &error {
                eprintln!("lgtv-companion-session: {error}");
            }
            last_error = error;
        }
    }
}

async fn report(
    path: &Path,
    snapshot: SessionSnapshot,
    session_id: Option<String>,
) -> Result<(Option<u32>, bool)> {
    timeout(Duration::from_secs(4), async {
        let mut socket = UnixStream::connect(path)
            .await
            .map_err(|error| format!("Cannot connect to {}: {error}", path.display()))?;
        let request = Request {
            version: ipc::VERSION,
            id: 1,
            method: Method::ReportSession {
                snapshot,
                session_id,
            },
            arguments: Vec::new(),
        };
        ipc::write(&mut socket, &request).await?;
        let response: Response = ipc::read(&mut socket).await?;
        if response.id != request.id || response.version != request.version {
            return Err("Unexpected reply from the daemon".into());
        }
        match response.reply {
            Reply::SessionSettings {
                idle_timeout_ms,
                turn_displays_off,
            } => Ok((idle_timeout_ms, turn_displays_off)),
            Reply::Error { message } => Err(message.into()),
            _ => Err("Unexpected command result".into()),
        }
    })
    .await?
}
