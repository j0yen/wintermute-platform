# Changelog

## v0.6.0 — 2026-05-30

Enforces one install-path convention (~/.local/bin/) across all user-scope
wintermute fleet binaries. Fixes wmd-init dead-service (ExecStart mismatch).
Adds install_convention module with FLEET_INSTALL_DIR constant, idempotent
plan_install, rewrite_unit_exec_starts, check_unit_convention functions.
Updates install.sh with reconcile_unit_file() and wm doctor gate.

## v0.5.0 — 2026-05-30

Extends wintermute-platform's wm binary with a wm doctor subcommand that enumerates every wintermute systemd unit, resolves ExecStart specifiers, verifies binary existence/executability, reports enabled/active/in-target status, and exits nonzero when any unit's binary is missing.

## v0.4.0 — 2026-05-30

Ships wintermute-watchdog: detects wintermute units in failed state, clears
them, and restarts with capped exponential backoff (2s→300s, give-up after 8
attempts, wm.health.* event on give-up). Tunes fleet units with
StartLimitIntervalSec=0 so a transient flap never permanently bricks a unit
before the watchdog can act.

## v0.3.0 — 2026-05-29

Add kiosk-mode boot path for zero-keyboard deployments. `install.sh --kiosk`
wires greetd autologin, enables `wintermute.target` on the wintermute user,
enables loginctl linger, installs `wintermute-boot-recovery.service` for
power-loss recovery, and drops a tmpfiles.d rule. Adds `src/kiosk.rs` with
greetd config management and boot-resilience primitives. On first activation,
wmd-init publishes `wm.boot.first`; on subsequent boots, `wm.boot.recovered`.
Designed for mother's home: plug in power, wintermute speaks its boot phrase,
no keyboard required.

## v0.2.0 — 2026-05-29

Add `wm ready` — a standing device-readiness beacon that joins the
fleet-install-doctor per-unit verdict with what a *working* companion
needs: API key present (or degrade-configured), an audio source and
sink, agorabus reachable, and `wintermute.target` active. Speaks the
verdict in plain language on boot and emits a `wm.health.ready`
envelope for off-device beaconing. Distinct from companion-degrade's
mid-conversation failure voice — this is the deploy/boot readiness voice.
