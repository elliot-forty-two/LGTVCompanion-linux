# Linux port future work

This document tracks Linux features that remain unimplemented or need qualification. The current working configuration format is documented in [the Linux guide](../linux/README.md#edit-the-configuration), and the source-derived behavior inventory remains in [the Linux reimplementation plan](Linux-reimplementation-plan.md).

## Desktop automation

- Add foreground window and fullscreen observations. Use EWMH and XInput2 on Xorg, Mutter session APIs or a versioned Shell adapter on GNOME Wayland, KWin APIs on Plasma, and Sway IPC on Sway. Expose unavailable capabilities through session diagnostics.
- Add per-application idle exceptions for running processes, foreground applications, fullscreen windows, and video or display wake locks. Match configured Linux executable identities and app IDs. Preserve aggregate compositor inhibition as evidence rather than attributing it to an individual process.
- Add ignored-key rules and input activity filtering. Map configured keys to Linux input codes. Handle repeats, noisy devices, controller dead zones, and Steam Controller 2 vendor reports. Prefer compositor activity or session-scoped access. Do not log raw input.
- Add a configurable system-wide idle fallback policy for desktops that only report aggregate input idle.
- Add display identity mapping using EDID plus connector names. Support duplicate displays, clone layouts, hotplug, stale mappings, and a guided identify-and-confirm workflow. Persist approved mappings and optionally restore them at the greeter or boot.
- Qualify speaker mute during blanking on supported TV firmware. The `host_power.mute_speakers` option currently mutes idle and streaming blank actions and unmutes idle restoration. Add and verify the desired behavior for every other wake, restore, or failure path.

## Configuration and daemon lifecycle

- Implement validated live configuration reload. Parse and validate a candidate configuration before applying it, report errors to the requester, and retain the active configuration when validation fails. Reconcile per-TV workers, pairing references, session policies, stream providers, event feed settings, and timers without sending TV power commands as a side effect of reload.
- Add an Apply operation for the JSON configuration workflow with atomic replacement, restrictive permissions, and clear rollback behavior. Keep service startup compatible with manually edited configuration files.
- Expand Windows configuration migration to automation policies, idle exclusions, ignored keys, topology identities, streaming settings, and supported pairing migration. Report values that need Linux-specific remapping.
- Add a graphical settings application if JSON editing stops being sufficient. Cover device discovery and management, automation, topology, streaming, validation, apply/cancel, diagnostics, and pairing workflow.

## Networking and discovery

- Add explicit interface selection for Wake-on-LAN and derive source address and broadcast destinations from the selected route or interface.
- Qualify SSDP discovery on multiple real interfaces and TV models. Verify changed addresses, duplicate responders, missing MAC addresses, and partial scans.
- Direct-IP Wake-on-LAN may fail when the sleeping TV has no neighbor-cache entry. This remains deferred because correcting it may require a privileged helper. Broadcast wake modes remain available.
- Verify wake delivery over Ethernet and Wi-Fi, multiple MAC addresses, subnet broadcast, network loss, and TV pixel-refresh windows.

## Commands, scripting, and streaming

- Complete the command parity review against both Windows executables and capture Windows CLI output fixtures for formatting, errors, multi-TV ordering, and source-specific command behavior.
- Add Linux examples for reading events and for picture-mode changes around application or game launches. The event watcher already supports JSON and legacy tokens.
- Add automatic Steam Remote Play and remote desktop session detection where a stable, verifiable Linux session signal exists. Explicit streaming hooks are available and remain the fallback.
- Qualify display synchronization after stream end on each compositor. Some backends may report display power without providing a reliable way to request it.

## Packaging and release

- Build and verify Debian-family and RPM-family packages. Include the daemon, CLI, session agent, systemd units, desktop autostart, configuration sample, and migration guidance.
- Test clean installation, upgrade, rollback, removal, permissions, pairing-key preservation, and service restart. Confirm package updates and daemon reloads do not unexpectedly change TV power.
- Implement updater modes where an authorized system package backend supports them. Include release checks, notifications, verification, retry, and retained configuration behavior.
- Publish supported distribution, desktop, compositor, and TV firmware coverage. Record whether each behavior is native, compositor-specific, hook-based, unavailable, or still unqualified.

## Live qualification

- Test Device1 with persistent and keepalive connections, delayed HDMI switching, and speaker mute.
- Test suspend, hibernate, shutdown, reboot, and resume on a real systemd host.
- Qualify display power, idle transitions, output mapping, and session ownership on Xorg, GNOME Wayland, KDE Plasma, and Sway where available.
- Verify real-TV stream connect and disconnect behavior, HDMI safety, restoration modes, and desktop display synchronization.
- Confirm SSDP visibility and Wake-on-LAN delivery across the actual network interfaces in use.
