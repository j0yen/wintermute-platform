# wintermute-platform

> Power-on to "Hi, I'm here" in ~15 seconds, with no human in the loop
> after bootstrap. Provides greetd autologin, a `wintermute.target`
> systemd user target that pulls in all Fleet 1 services in the right
> order, a small Rust supervisor `wmd-init` that owns lifecycle and
> restarts crashed children within 1 s, and a `wm` CLI for status,
> mute, restart, and logs.

This is the load-bearing scaffold for the rest of the wintermute fleet
(`wintermute-audio`, `wintermute-stt`, `wintermute-tts`,
`wintermute-dialog`, `wintermute-brain`). Once shipped, adding a new
child service is a one-line addition to the target unit.

Built with Rust 2024 / `rustc 1.85`. No daemon, no network. The
supervisor talks to the `wm` CLI over a UDS at
`$XDG_RUNTIME_DIR/wintermute/init.sock`.

## Install

### Manual install

```sh
git clone --depth 1 https://github.com/j0yen/wintermute-platform.git
cd wintermute-platform
cargo install --path . --root ~/.local
sudo ./pkg/install-system.sh
```

`cargo install` puts `wmd-init` and `wm` into `~/.local/bin/`.
`pkg/install-system.sh` drops the systemd user units, the
`wintermute-session` script, and an example greetd config; it is
idempotent and prints exactly what it touched.

### Prerequisites

- `cargo` / `rustc 1.85+`
- A working systemd-user instance (`loginctl enable-linger $USER` if
  you want children to keep running after logout — and on this
  laptop, you do)
- Optional: `greetd` for autologin. Without it, the bare `getty@tty1`
  autologin path is documented under `pkg/greetd/config.toml.example`.
- A bootstrap env file at `/etc/wintermute/conf.d/00-bootstrap.env`
  (produced by [`wintermute-bootstrap`](https://github.com/j0yen/wintermute-bootstrap)).
  Without it, `wmd-init` exits cleanly with `no bootstrap config;
  halting` rather than crash-looping (PRD AC9).

## Quick start

```sh
# Start the target manually (the autologin path does this for you):
systemctl --user start wintermute.target

# Inspect children:
wm status
wm logs wm-audio --tail 20
wm restart wm-stt
wm mute            # halts active TTS within 200 ms
wm unmute
```

## What's in the box

| Artifact | Path | Role |
|---|---|---|
| `wmd-init` | `~/.local/bin/wmd-init` | Tokio supervisor; owns child lifecycle and restart policy. |
| `wm` | `~/.local/bin/wm` | CLI; talks to `wmd-init` over a UDS. |
| `wintermute.target` | `/usr/lib/systemd/user/` | systemd user target the rest of the fleet attaches to. |
| `wmd-init.service` | `/usr/lib/systemd/user/` | `Type=notify` service; `Restart=always`, `RestartSec=2`. |
| `wintermute-session` | `/usr/local/bin/` | greetd shell-entry: starts X if needed, then blocks on `wintermute.target`. |
| `greetd/config.toml.example` | `pkg/` | Drop-in for `/etc/greetd/config.toml`. |

## Crash policy (PRD §2.3)

- Restart child within 1 s on unexpected exit.
- After 5 restarts in a 60 s window, back off and emit
  `init.backoff` on agorabus.
- After 20 minutes of failed restarts, play a spoken error via
  `wm-tts` (if still up) and keep trying.

Child startup order is `wm-audio → wm-tts → wm-stt → wm-dialog →
wmd`; audio first because everything subscribes to its events, tts
before stt so the first-boot greeting can play immediately, dialog
before wmd so the brain can gate verbal confirmations.

## Hardware reality verification

ACs 1, 2, 5, 8 are OS/hardware-bound (bringing up the live systemd-user
Fleet 1 target in dependency order, cold-reboot-to-greeting wall-clock
timing, real TTS halt within 200 ms, supervisor backoff under a real
restart storm). They are declared in the PRD's `deferred_acs:` +
`mock_unjustified_for:` frontmatter with a one-sentence justification
each, because an in-process fake would reimplement systemd's transaction
engine and assert our own math rather than the OS's real lifecycle
behavior.

To validate them against real hardware, run:

```sh
cargo test --features=real-hardware
```

This feature is opt-in and off by default, so `cargo test` stays green on
hosts without the live systemd-user target. The drift-report sweep that
compares mock vs. real-hardware outcomes (`hardware-drift.json`) is
scaffolded as a follow-on PRD and is not invoked by default.

## License

Dual-licensed MIT or Apache-2.0 at your option.
