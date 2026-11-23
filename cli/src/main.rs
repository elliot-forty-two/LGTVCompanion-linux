use std::{
    env,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use lgtv_companion_runtime::{
    batch::{self, Output, Step},
    config::{self, Config},
    control::{default_state_dir, execute, failed, pair},
    discovery::{self, preview},
    ipc::{self, Reply},
    pairing::Store,
    tv::Result,
};
use serde_json::{Map, Value, json};
use tokio::{net::UnixStream, time::timeout};

const HELP: &str = "Usage:
  lgtvcli --config FILE [--state-dir DIR] --pair [DEVICE ...]
  lgtvcli [--direct | --socket PATH] -COMMAND [ARG ...] [DEVICE ...] ...
  lgtvcli [--direct] --config FILE [--state-dir DIR] -COMMAND [ARG ...] [DEVICE ...] ...
  lgtvcli [--socket PATH] -autoenable|-autodisable [DEVICE ...]
  lgtvcli [--socket PATH] --watch-events [--legacy]
  lgtvcli [--socket PATH] -streaming_connect [SOURCE SESSION]
  lgtvcli [--socket PATH] -streaming_disconnect [SOURCE SESSION]
  lgtvcli --import-windows FILE
  lgtvcli --discover [--interface NAME] ...
  lgtvcli --config FILE --scan-devices refresh|append|replace [--interface NAME] ...

Devices default to all configured devices. See linux/README.md for supported commands.";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("lgtvcli: {error}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<bool> {
    let args: Vec<_> = env::args_os()
        .skip(1)
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "Arguments must be valid UTF-8")
        })
        .collect::<std::result::Result<_, _>>()?;
    if args.is_empty()
        || (args.len() == 1 && matches!(args[0].as_str(), "--help" | "-help" | "-h" | "-?"))
    {
        println!("{HELP}");
        return Ok(true);
    }
    if let [flag, path] = args.as_slice()
        && flag == "--import-windows"
    {
        serde_json::to_writer_pretty(
            io::stdout().lock(),
            &config::windows_preview(&PathBuf::from(path))?,
        )?;
        println!();
        return Ok(true);
    }

    let mut config = None;
    let mut state_dir = None;
    let mut socket = None;
    let mut direct = false;
    let mut remaining = args.as_slice();
    while let Some(flag) = remaining.first() {
        if flag == "--direct" && !direct {
            direct = true;
            remaining = &remaining[1..];
            continue;
        }
        let [_, path, rest @ ..] = remaining else {
            break;
        };
        match flag.as_str() {
            "--config" if config.is_none() => config = Some(PathBuf::from(path)),
            "--state-dir" if state_dir.is_none() => state_dir = Some(PathBuf::from(path)),
            "--socket" if socket.is_none() => socket = Some(PathBuf::from(path)),
            _ => break,
        }
        remaining = rest;
    }
    let pairing = remaining
        .first()
        .is_some_and(|argument| argument == "--pair");
    if let Some(operation) = remaining.first()
        && matches!(operation.as_str(), "--discover" | "--scan-devices")
    {
        if socket.is_some() || state_dir.is_some() {
            return Err(
                "Discovery uses local interfaces and does not accept --socket or --state-dir"
                    .into(),
            );
        }
        return discovery_command(config, remaining).await;
    }
    let direct = direct || pairing || config.is_some() || state_dir.is_some();
    if direct && socket.is_some() {
        return Err("--socket requires daemon mode. Supply the config and state options to the daemon itself".into());
    }
    if let [flag, options @ ..] = remaining
        && flag == "--watch-events"
    {
        if direct {
            return Err("Watching host events requires daemon mode".into());
        }
        let legacy = match options {
            [] => false,
            [option] if option == "--legacy" => true,
            _ => return Err("Usage: lgtvcli [--socket PATH] --watch-events [--legacy]".into()),
        };
        let path = socket.unwrap_or_else(|| PathBuf::from(ipc::DEFAULT_SOCKET));
        watch_events(&path, legacy).await?;
        return Ok(true);
    }
    if let Some(hook) = remaining.first().map(|word| word.to_ascii_lowercase())
        && matches!(
            hook.as_str(),
            "-streaming_connect" | "-streaming_disconnect"
        )
    {
        if direct {
            return Err("Streaming hooks require daemon mode".into());
        }
        let (source, session) = match &remaining[1..] {
            [] => ("manual", "default"),
            [source, session] => (source.as_str(), session.as_str()),
            _ => return Err("Supply both SOURCE and SESSION, or omit both".into()),
        };
        let path = socket.unwrap_or_else(|| PathBuf::from(ipc::DEFAULT_SOCKET));
        let mut stream = timeout(Duration::from_secs(2), UnixStream::connect(&path))
            .await?
            .map_err(|error| format!("Cannot connect to {}: {error}", path.display()))?;
        let request = ipc::Request {
            version: ipc::VERSION,
            id: 1,
            arguments: Vec::new(),
            method: ipc::Method::ReportStream {
                source: source.into(),
                session: session.into(),
                connected: hook == "-streaming_connect",
            },
        };
        timeout(Duration::from_secs(5), ipc::write(&mut stream, &request)).await??;
        return match timeout(Duration::from_secs(5), read_reply(&mut stream, request.id)).await?? {
            Reply::Complete => {
                println!("{{\"accepted\":true}}");
                Ok(true)
            }
            Reply::Error { message } => Err(message.into()),
            _ => Err("Unexpected daemon reply to streaming hook".into()),
        };
    }
    if !direct {
        let path = socket
            .clone()
            .unwrap_or_else(|| PathBuf::from(ipc::DEFAULT_SOCKET));
        match timeout(Duration::from_secs(2), UnixStream::connect(&path)).await? {
            Ok(stream) => return daemon_batch(stream, remaining).await,
            Err(error)
                if socket.is_none()
                    && matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) => {}
            Err(error) => {
                return Err(format!("Cannot connect to {}: {error}", path.display()).into());
            }
        }
    }
    let config_path = config.unwrap_or_else(|| PathBuf::from(ipc::DEFAULT_CONFIG));
    let config = Config::load(&config_path)
        .map_err(|error| format!("Cannot load {}: {error}", config_path.display()))?;
    let state_dir = state_dir
        .or_else(default_state_dir)
        .ok_or("Specify a state directory with --state-dir DIR")?;

    if let [flag, selectors @ ..] = remaining
        && flag == "--pair"
    {
        let targets = config.targets(selectors)?;
        let store = Store::open(state_dir)?;
        let mut results = serde_json::Map::new();
        let mut success = true;
        for target in targets {
            let device = &config.devices[target];
            let result = pair(device, &store, || {
                eprintln!("{}: accept the pairing prompt on the TV", device.id)
            })
            .await;
            results.insert(device.id.clone(), result_value(result));
        }
        print_result(&Output::Compact, results, &mut success)?;
        return Ok(success);
    }

    let steps = batch::parse(remaining, false)?;
    if !steps
        .iter()
        .any(|step| matches!(step, Step::Command { .. }))
    {
        return Err("Supply at least one TV command".into());
    }
    // Validate every selector before dispatching the batch.
    let targets = steps
        .iter()
        .map(|step| match step {
            Step::Format(_) => Ok(Vec::new()),
            Step::Command { selectors, .. } => config.targets(selectors),
        })
        .collect::<Result<Vec<_>>>()?;
    let store = Store::open(state_dir)?;
    let mut output = Output::Compact;
    let mut success = true;
    for (step, targets) in steps.into_iter().zip(targets) {
        match step {
            Step::Format(format) => output = format,
            Step::Command { action, .. } => {
                let mut results = serde_json::Map::new();
                for target in targets {
                    let device = &config.devices[target];
                    let result =
                        execute(device, &store, &action, config.wake_timeout_seconds).await;
                    results.insert(device.id.clone(), result_value(result));
                }
                print_result(&output, results, &mut success)?;
            }
        }
    }
    Ok(success)
}

async fn watch_events(path: &Path, legacy: bool) -> Result<()> {
    let mut stream = timeout(Duration::from_secs(2), UnixStream::connect(path))
        .await?
        .map_err(|error| format!("Cannot connect to {}: {error}", path.display()))?;
    let request = ipc::Request {
        version: ipc::VERSION,
        id: 1,
        method: ipc::Method::WatchEvents,
        arguments: Vec::new(),
    };
    timeout(Duration::from_secs(5), ipc::write(&mut stream, &request)).await??;
    match timeout(Duration::from_secs(5), event_reply(&mut stream, request.id)).await?? {
        Reply::Subscribed => {}
        Reply::Error { message } => return Err(message.into()),
        _ => return Err("Daemon did not acknowledge the event subscription".into()),
    }
    let stdout = io::stdout();
    loop {
        match event_reply(&mut stream, request.id).await? {
            Reply::Event { event } => {
                let mut out = stdout.lock();
                let line = if legacy {
                    event.legacy_token().to_owned()
                } else {
                    serde_json::to_string(&json!({"event": event}))?
                };
                match writeln!(out, "{line}").and_then(|()| out.flush()) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
                    Err(error) => return Err(error.into()),
                }
            }
            Reply::Error { message } => return Err(message.into()),
            _ => return Err("Unexpected reply on the host event stream".into()),
        }
    }
}

async fn event_reply(stream: &mut UnixStream, id: u64) -> Result<Reply> {
    let response = ipc::read(stream).await.map_err(|error| {
        format!("Host event stream ended: {error}. Reconnect to receive new events")
    })?;
    checked_reply(response, id)
}

async fn daemon_batch(mut stream: UnixStream, arguments: &[String]) -> Result<bool> {
    let steps = batch::parse(arguments, true)?;
    let request = ipc::Request {
        version: ipc::VERSION,
        id: 1,
        method: ipc::Method::Execute,
        arguments: arguments.to_vec(),
    };
    timeout(Duration::from_secs(5), ipc::write(&mut stream, &request)).await??;
    let mut output = Output::Compact;
    let mut success = true;
    for step in steps {
        match step {
            Step::Format(format) => output = format,
            Step::Command { .. } => match read_reply(&mut stream, request.id).await? {
                Reply::Result { devices } => print_result(&output, devices, &mut success)?,
                Reply::Error { message } => return Err(message.into()),
                Reply::Complete => {
                    return Err("Daemon ended the batch before returning every result".into());
                }
                _ => return Err("Daemon sent an unexpected result".into()),
            },
        }
    }
    match read_reply(&mut stream, request.id).await? {
        Reply::Complete => Ok(success),
        Reply::Error { message } => Err(message.into()),
        _ => Err("Daemon sent an unexpected result".into()),
    }
}

async fn read_reply(stream: &mut UnixStream, id: u64) -> Result<Reply> {
    let response: ipc::Response = ipc::read(stream).await.map_err(|error| {
        format!("Daemon reply failed: {error}. Check the result before retrying")
    })?;
    checked_reply(response, id)
}

fn checked_reply(response: ipc::Response, id: u64) -> Result<Reply> {
    if response.version != ipc::VERSION || response.id != id {
        return Err("Unexpected daemon reply version or request ID".into());
    }
    Ok(response.reply)
}

fn result_value(result: Result<Value>) -> Value {
    result.unwrap_or_else(|error| json!({"error": error.to_string()}))
}

async fn discovery_command(config: Option<PathBuf>, args: &[String]) -> Result<bool> {
    let (plan, options) = if args[0] == "--scan-devices" {
        let Some((mode, tail)) = args[1..].split_first() else {
            return Err("--scan-devices requires refresh, append, or replace".into());
        };
        let mode = match mode.as_str() {
            "refresh" => preview::Mode::Refresh,
            "append" => preview::Mode::Append,
            "replace" => preview::Mode::Replace,
            _ => return Err("--scan-devices requires refresh, append, or replace".into()),
        };
        let config = config.ok_or("Supply --config FILE before --scan-devices")?;
        (Some((Config::load_for_preview(&config)?, mode)), tail)
    } else {
        if config.is_some() {
            return Err("Use --scan-devices to review changes to a configuration".into());
        }
        (None, &args[1..])
    };
    let mut interfaces = Vec::new();
    let mut options = options.iter();
    while let Some(option) = options.next() {
        if option != "--interface" {
            return Err(format!("Unexpected discovery option {option:?}").into());
        }
        let interface = options
            .next()
            .filter(|value| !value.starts_with('-'))
            .ok_or("--interface requires a Linux interface name")?;
        interfaces.push(interface.clone());
    }
    let found = discovery::scan(&interfaces).await?;
    let output = match plan {
        Some((config, mode)) => {
            json!({"plan": preview::stage(config, &found, mode), "discovery": found})
        }
        None => serde_json::to_value(found)?,
    };
    serde_json::to_writer_pretty(io::stdout().lock(), &output)?;
    println!();
    Ok(true)
}

fn print_result(
    output: &Output,
    devices: Map<String, Value>,
    success: &mut bool,
) -> io::Result<()> {
    for (device, value) in &devices {
        if failed(value) {
            *success = false;
            if matches!(output, Output::Key(_)) {
                eprintln!(
                    "{device}: {}",
                    value["error"].as_str().unwrap_or("TV rejected the request")
                );
            }
        }
    }
    output.write(io::stdout().lock(), &Value::Object(devices))
}
