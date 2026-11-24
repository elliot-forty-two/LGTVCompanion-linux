# LGTV Companion for Linux

LGTV Companion for Linux lets you control LG webOS TVs with the command line, automate power and screen behavior, and connect scripts to host events.

## Edit the configuration

Start with [devices.example.json](devices.example.json) or edit your existing `devices.json`. Replace the sample IPv4 address, MAC address, and certificate filename with your TV's values. Use an empty `macs` list if you only want to control the TV while it is reachable. Add another entry to `devices` for each TV and give each one a unique ID. Keep an existing TV's ID to reuse its saved pairing key.

The sample starts with manual control. Choose the settings you want from [TV connection modes](#tv-connection-modes), [automatic TV power-off](#automatic-tv-power-off), [desktop session automation](#desktop-session-automation), and [remote streaming](#remote-streaming). A `null` field means the setting is unset. JSON uses no comments or trailing commas.

Pass your config path with `--config` for direct CLI or foreground daemon use. For the installed systemd service, edit `/etc/lgtv-companion/devices.json` and restart `lgtv-companion.service` to apply changes.


## Direct TV control

WSS defaults to port 3001 and requires a verified copy of the TV's leaf certificate in DER format. Relative certificate paths resolve from the configuration file. If the TV certificate changes, verify the replacement and update the configured pin. For TVs configured to use plaintext, set `"transport": "ws"` and omit `certificate`. WS defaults to port 3000.

To inspect the certificate presented by a powered-on TV, replace `TV_IP` with its IPv4 address and run the commands below. Verify the SHA-256 fingerprint through a trusted source before saving the candidate as the configured certificate.

```sh
openssl s_client -connect TV_IP:3001 -showcerts </dev/null |
  openssl x509 -outform DER -out office-tv.candidate.der
openssl x509 -inform DER -in office-tv.candidate.der -noout -subject -issuer -sha256 -fingerprint
```

```sh
lgtvcli --config devices.json --pair Device1
lgtvcli --config devices.json -poweron Device1 -sethdmi2 Device1
lgtvcli --config devices.json -ok backlight \
  -get_system_settings picture '["backlight"]' "Office TV"
lgtvcli --import-windows /path/to/config.json
```

Accept the prompt on the TV during `--pair`. A successful pairing saves the key under `$XDG_STATE_HOME/lgtv-companion/pairing` or `$HOME/.local/state/lgtv-companion/pairing`. Use `--state-dir` for an isolated state directory. Run `--pair` again to replace a rejected key. The optional Windows import command previews supported TV settings and omits pairing keys.

Omit selectors to act on all configured TVs. Command and selector matching is case-insensitive. Use `--` before an individual selector that starts with a hyphen. Commands execute in argument order, with selected TVs handled in configuration order. `-of`, `-od`, and `-ok KEY` change the format for subsequent commands. Exit status 0 indicates success, 1 indicates at least one failed TV operation, and 2 indicates an invocation or configuration error. The batch continues after a TV operation fails.

Wake mode accepts `automatic`, `limited_broadcast`, `direct`, or `subnet_broadcast`. Subnet broadcast requires a netmask. Set a device's optional `source_ip` to a local IPv4 address to bind its WebSocket and wake traffic to that address. Linux routing still selects the outgoing interface. Direct-IP wake may fail when the TV has been asleep long enough for its neighbor entry to expire. Use a broadcast wake mode in that situation.

## Discover or refresh TVs

Power on the TVs you want to discover. The scanner uses `ip` from iproute2 to enumerate multicast-capable IPv4 interfaces and look up each responder's MAC in the Linux neighbor table. It checks device descriptions on the responding TV's address for LG webOS metadata. Pass `--interface NAME` to narrow the scan, and repeat it to scan multiple interfaces. Use manual setup for TVs that stay undiscovered or have no confirmed MAC. [UPnP discovery specification](https://upnp.org/specs/arch/UPnP-arch-DeviceArchitecture-v1.1.pdf) · [LG webOS discovery filter](https://github.com/ConnectSDK/Connect-SDK-Android-Core/blob/master/src/com/connectsdk/service/WebOSTVService.java)

```sh
lgtvcli --discover
lgtvcli --discover --interface enp3s0
lgtvcli --config devices.json --scan-devices refresh
lgtvcli --config devices.json --scan-devices append
lgtvcli --config devices.json --scan-devices replace
```

- `refresh` proposes only IP address changes for unambiguous normalized MAC matches. It preserves IDs, certificate paths, automation policies, and pairing state.
- `append` also proposes new TV records. New records have MAC-derived IDs, default to WSS, and need their own verified certificate and pairing.
- `replace` stages removal of undiscovered TVs after a usable, complete scan. A failed, incomplete, or empty scan keeps the existing device list. Review the removals carefully when scanning selected interfaces or when some TVs are asleep.

Scans print JSON. They leave configuration files, certificates and pairing keys untouched. For a configuration scan, the proposed configuration is in `plan.preview`. Inspect `plan.changes`, `plan.added`, `plan.removed` and both sets of warnings before applying it. Save the preview in the original configuration directory to keep relative paths valid. Restart the daemon after replacing its configuration.

## Daemon commands

Start the foreground daemon with your existing direct CLI configuration. Keep it running while submitting commands from another terminal. Running both programs as the same user lets the daemon use the existing pairing keys.

```sh
run_dir="${XDG_RUNTIME_DIR:-/tmp}/lgtv-companion-$(id -u)"
lgtv-companiond --config devices.json --socket "$run_dir/command.sock"
```

```sh
run_dir="${XDG_RUNTIME_DIR:-/tmp}/lgtv-companion-$(id -u)"
lgtvcli --socket "$run_dir/command.sock" -request audio/getVolume Device1
```

The CLI uses the running service when available. Use `--direct --config FILE` to control TVs without the service. The service reads `/etc/lgtv-companion/devices.json`.

### Watch host events from scripts

Set `"external_api": true` at the top level of the daemon's `devices.json` and restart the daemon. The watcher uses the same socket permissions as TV commands. Host lifecycle events require `--monitor-power`, which the installed systemd unit enables. Display and idle events come from the session agent in the active local graphical session. Idle events use `idle_blank_minutes` when set, or ten minutes by default. Enabling the feed alone leaves idle blanking disabled.

```sh
lgtvcli --watch-events
lgtvcli --watch-events --legacy
```

JSON mode prints one object per line, for example `{"event":"displays_off"}`. Event names are `displays_on`, `displays_off`, `user_idle`, `user_busy`, `suspend`, `resume`, `reboot`, `shutdown`, and `shutdown_unknown` for a legacy logind signal without a known shutdown type. Legacy mode prints Windows tokens such as `SYSTEM_DISPLAYS_OFF` and uses `SYSTEM_SHUTDOWN` for both shutdown variants. The session agent currently reports display on/off transitions.

```sh
lgtvcli --watch-events --legacy | while IFS= read -r event; do
  case "$event" in
    SYSTEM_SUSPEND|SYSTEM_RESUME) printf '%s\n' "$event" ;;
  esac
done
```

Each subscription receives transitions observed after it connects. The first session snapshot establishes a baseline. If a watcher falls behind or loses its daemon connection, it exits with an error. Run it again to subscribe to new events. Up to eight watchers can connect concurrently.

### TV connection modes

Set `connection_mode` on each TV in `devices.json` and restart the daemon to apply it.

| Value | Daemon behavior |
| --- | --- |
| `off` (default) | Opens and registers a connection for each command. |
| `persistent` | Reuses the registered connection across commands. |
| `keepalive` | Reuses the connection and queries TV power state after 60 seconds without a command. |

Persistent modes connect on demand, discard sockets across host power transitions, and reconnect for the next command after a detected closure or failed keepalive. An idle probe only uses an open socket and never wakes the TV. Direct CLI commands use short-lived connections in every mode.

After a lost or malformed reply, recognized read-only requests retry once on a fresh connection. For supported power and panel commands, the worker can reconnect and confirm the requested state. If it cannot confirm what happened, it reports an unknown outcome. The worker never automatically repeats a write whose reply was lost.

## Systemd service and power monitoring

The system unit runs as the dedicated `lgtv-companion` account. Members of the `lgtv-companion` group can submit TV commands through `/run/lgtv-companion/command.sock`. The service reads `/etc/lgtv-companion/devices.json` and stores pairing keys in `/var/lib/lgtv-companion/pairing`.

Install the binaries, account definition and unit from the repository root.

```sh
cargo build --release --locked
sudo install -Dm0755 target/release/lgtv-companiond /usr/local/bin/lgtv-companiond
sudo install -Dm0755 target/release/lgtvcli /usr/local/bin/lgtvcli
sudo install -Dm0644 systemd/lgtv-companion.conf /etc/sysusers.d/lgtv-companion.conf
sudo systemd-sysusers /etc/sysusers.d/lgtv-companion.conf
sudo install -Dm0644 systemd/lgtv-companion.service /etc/systemd/system/lgtv-companion.service
```

For the `Device1` example above, copy the existing configuration, certificate and pairing key into their service locations. Use your configured certificate filename. Resolve certificate paths within `/etc/lgtv-companion` so the service can read them.

```sh
sudo install -d -o root -g lgtv-companion -m0750 /etc/lgtv-companion
sudo install -o root -g lgtv-companion -m0640 devices.json office-tv.der /etc/lgtv-companion/
sudo install -d -o lgtv-companion -g lgtv-companion -m0700 /var/lib/lgtv-companion/pairing
sudo install -o lgtv-companion -g lgtv-companion -m0600 \
  "${XDG_STATE_HOME:-$HOME/.local/state}/lgtv-companion/pairing/device1.key" \
  /var/lib/lgtv-companion/pairing/device1.key
sudo usermod -aG lgtv-companion "$USER"
sudo systemctl daemon-reload
sudo systemctl enable --now lgtv-companion.service
```

If you need to pair again, run `sudo -u lgtv-companion lgtvcli --config /etc/lgtv-companion/devices.json --state-dir /var/lib/lgtv-companion/pairing --pair Device1` with the TV powered on. Sign out and back in after adding your account to the group. Then run a read-only command through the service or watch the journal.

```sh
lgtvcli --socket /run/lgtv-companion/command.sock -request audio/getVolume Device1
journalctl -u lgtv-companion.service -f
```

The unit enables `--monitor-power`. The daemon records sleep preparation, resume, and shutdown preparation in the journal. On logind versions that supply shutdown metadata, it distinguishes power-off, halt, reboot, kexec and soft reboot. Older versions report an unknown shutdown type. Sleep notifications cover both suspend and hibernation. [logind lifecycle signals](https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html) · [systemd service directories](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)

### Automatic TV power-off

Add a `host_power` object to each TV you want the system service to manage. Existing configurations continue to log power events without acting on them.

```json
"host_power": {
  "hdmi_input": 1,
  "power_off_on_unknown_shutdown": false
}
```

The service powers off an enabled TV during suspend, hibernation, power-off or halt if the TV reports that it is active or screen-off. With `hdmi_input` set, it also requires the TV to confirm that HDMI input is in the foreground. Omit the input to act on any source. Reboot, kexec and soft reboot preserve the TV's power state. Set `power_off_on_unknown_shutdown` to `true` only if you want legacy logind shutdown signals to power off that TV even when the host cannot distinguish a shutdown from a reboot.

Restart the service after editing `/etc/lgtv-companion/devices.json`. If at least one TV has a `host_power` policy, the daemon acquires a logind delay inhibitor so temporary overrides can take effect. It skips automatic TV commands if it cannot get a delay lock. It gives all eligible TVs at most four seconds in parallel, or less when logind's limit requires it, then releases the inhibitor. A TV worker that is busy with an explicit command can run out of time before the automatic command starts. Resume and cancellation discard unfinished lifecycle work and rearm the inhibitor. The session agent supplies the display-on signal that drives wake after resume. [logind delay inhibitor behavior](https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html)

### Temporary automation overrides

```sh
lgtvcli -autodisable Device1
lgtvcli -autoenable Device1
```

These daemon commands accept the normal device selectors. Omit selectors to target all TVs. A TV needs a `host_power` policy before it can be enabled. Set `"enabled": false` inside that policy to start with automation disabled. An override leaves the configuration file alone and expires when the daemon restarts. Disabling a TV cancels its pending lifecycle, display, idle, and streaming actions. Manual power and TV commands remain available.

## Desktop session automation

Install the updated daemon and the session agent from the same build.

```sh
cargo build --release --locked
sudo install -Dm0755 target/release/lgtv-companiond /usr/local/bin/lgtv-companiond
sudo install -Dm0755 target/release/lgtv-companion-session /usr/local/bin/lgtv-companion-session
```

Set `"follow_display_power": true` inside the TV's `host_power` object to follow display power. Add an optional `"output": "HDMI-A-1"` to tie that TV to a particular connector. Use the exact name from the agent's diagnostics. Keep `hdmi_input` set to the host's TV input to protect TV apps and other sources.

To select HDMI after an automatic wake, add `"wake_hdmi": {"input": 2, "delay_seconds": 1}` inside `host_power`. Use inputs 1–4 and a delay of 0–30 seconds, which defaults to one second. The delay begins once the TV reports it is active. The switch applies to display-driven wakes and restores after streaming power-off. Manual wakes, idle restores, and restoration after streaming blanking preserve the TV's current input. A newer effective display-power change, session handoff, host power event, or temporary automation disable cancels the pending switch. The existing `hdmi_input` setting guards automatic off and blanking independently.

For idle blanking, set `"idle_blank_minutes": 10` at the top level of the configuration (range 1–240) and `"blank_when_idle": true` in each participating TV's `host_power` object. Once the session crosses the idle threshold while the chosen display is on, the daemon blanks the TV's panel and restores it on activity if the TV acknowledged the blanking command. Set `"mute_speakers": true` to mute on idle or streaming blank and unmute when idle blanking is restored. The TV stays powered during idle blanking.

Restart the system service after editing its configuration. Configuration changes are applied on service restart. Start the agent from your active local Xorg or Wayland session using an account in the `lgtv-companion` group.

```sh
lgtv-companion-session --diagnose
lgtv-companion-session
```

The agent reports snapshots every two seconds. The daemon accepts reports from the active local graphical session on `seat0`. The first snapshot at startup or session handoff establishes a baseline. After that, a display-off transition or removal of a mapped connector powers off a TV that follows display power. An on transition wakes it. After host resume, the daemon waits for confirmed display-on. Missing display or topology evidence suspends the corresponding action. Diagnostics report the active connector names, aggregate display power and per-output power where available. [X.Org DPMS protocol](https://xorg.freedesktop.org/releases/current/doc/xextproto/dpms.html) · [logind session properties](https://www.freedesktop.org/software/systemd/man/latest/org.freedesktop.login1.html)

On Wayland without a usable display-power interface, the related automatic action remains suspended. Run `lgtv-companion-session --diagnose` to inspect the capabilities available in your session.

For per-user autostart, install the provided desktop entry.

```sh
install -Dm0644 session/autostart/lgtv-companion-session.desktop \
  "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/lgtv-companion-session.desktop"
```

Run `lgtv-companion-session --diagnose` to see which display and idle information your desktop provides.

## Remote streaming

Enable streaming automation with a top-level `streaming` object in the daemon configuration. TVs with `host_power` participate when the session agent confirms that their host display is on. Restart the daemon after editing the configuration.

```json
"streaming": {
  "mode": "power_off",
  "end_mode": "restore",
  "sunshine_log": "/var/log/sunshine/sunshine.log"
}
```

In `power_off` mode, use `end_mode: "power_on"` to wake eligible TVs, `"restore"` to wake only TVs previously known active or screen-off, or `"keep_off"` to leave them powered off. In `blank` mode, the daemon blanks the panel during the stream and uses the normal wake path after the last disconnect. The configured HDMI check still applies. If no TV will be restored and a TV directly reported that it powered off, the agent requests desktop display-off where the compositor allows it. Some TVs close their connection before they can confirm standby, so that outcome does not trigger desktop display sync.

The daemon applies streaming policy ten seconds after the first connect and one second after the last disconnect. While a stream is active it pauses ordinary display and idle automation. Host suspend and shutdown still follow lifecycle policy and clear the old stream state. Reports need a fresh display snapshot, refreshed normally by the graphical session agent.

Set `sunshine_log` or `apollo_log` to a log file readable by the daemon account. Use info-level logs. The log reader starts at the end of the file, then tracks client connect and disconnect lines. A daemon started during a stream waits for fresh stream evidence. The log reader follows rotation and retains partial lines until they are complete. The system service's `ProtectHome` setting requires a path outside home directories. [Sunshine stream events](https://github.com/LizardByte/Sunshine/blob/master/src/stream.cpp) · [Apollo stream events](https://github.com/ClassicOldSong/Apollo/blob/master/src/stream.cpp)

For Steam Remote Play, remote desktop software, or other hosts with session hooks, call the CLI on actual session connect and disconnect. Reuse the same source and session ID for both calls, and give each concurrent client a distinct session ID. The hook account needs access to the daemon socket.

```sh
lgtvcli -streaming_connect steam client-42
lgtvcli -streaming_disconnect steam client-42
```

Omit both identifiers to use one default manual session. Configure either a log reader or hooks for the same provider so each session has a single reporting path. Steam Remote Play and remote desktop can report sessions through the explicit hooks shown above.

For development status and features planned for later releases, see [Linux port future work](Linux-future-work.md).
