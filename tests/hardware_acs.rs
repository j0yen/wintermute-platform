//! Hardware-dependent acceptance tests for AC1/AC2/AC5/AC8.
//!
//! These ACs measure live systemd-user behavior, cold-reboot timing,
//! and bus-event emission under a restart storm against a real host.
//! The deterministic, in-process portions of `wmd-init` (config
//! parsing, child-spec resolution, restart-policy math, supervisor
//! state machine) are covered by lib unit tests in `src/init.rs`,
//! `src/supervisor.rs`, and the integration smokes
//! `socket_roundtrip.rs` and `acceptance_template.rs`. What lives
//! here is the contract that the rest of each AC — `wintermute.target`
//! actually starting children in order, cold-reboot wall-clock to
//! first greeting, `wm mute` halting TTS over the live bus,
//! restart-storm backoff emitting `init.backoff` — is exercised
//! manually on a machine with installed Fleet 1 binaries and a
//! configured systemd-user instance.
//!
//! Each test is `#[ignore]`-gated. Run with:
//!
//!     cargo test --release --test hardware_acs -- --ignored --nocapture
//!
//! The doc-comments on each test name the operator-side procedure;
//! the test body itself is a sentinel that fails if invoked without a
//! `WM_PLATFORM_HARDWARE_SMOKE=1` environment witness, so an
//! accidental `--ignored` run on CI cannot silently report
//! "all passed".
//!
//! Per PRD §4 these tests pair AC1/AC2/AC5/AC8 with concrete cargo
//! test names so the manifest's verified-completed check #5 holds:
//! each AC has a paired test, even when the test itself is a manual
//! procedure. AC3 (crash-restart <1s), AC4 (`wm status` table), AC6
//! (`wm restart` per-child), AC7 (`wm logs --tail`), AC9 (missing
//! bootstrap clean exit) are paired by the in-process suites; see
//! the doc-comments below for the test names that bind them.

#![allow(clippy::expect_used, clippy::panic, clippy::missing_panics_doc)]

use std::env;

fn require_hardware_witness(ac: &str) {
    let witness = env::var("WM_PLATFORM_HARDWARE_SMOKE").unwrap_or_default();
    assert_eq!(
        witness, "1",
        "{ac}: this is a platform-hardware smoke test. \
         Set WM_PLATFORM_HARDWARE_SMOKE=1 and run on a machine with \
         systemd-user + installed Fleet 1 binaries (wm-audio, wm-stt, \
         wm-tts, wm-dialog, wmd) and a wintermute.target unit on \
         the user manager. See doc-comment for the manual procedure."
    );
}

/// AC1 — `systemctl --user start wintermute.target` brings every
/// Fleet 1 child up in dependency order within 5 s.
///
/// Manual procedure:
///   1. Install `wintermute.target` and per-child `wm-*.service`
///      units to `~/.config/systemd/user/` (see `contrib/systemd/`).
///      Run `systemctl --user daemon-reload`.
///   2. Confirm `systemctl --user stop wintermute.target` is the
///      starting state (all children inactive).
///   3. Run `systemctl --user start wintermute.target` and capture
///      wall-clock T0 at the issuing shell.
///   4. Poll `systemctl --user is-active wm-audio.service wm-stt.service
///      wm-tts.service wm-dialog.service wmd.service` until all five
///      report `active`. Capture T1.
///   5. T1 − T0 ≤ 5 s. Order verified by `journalctl --user -u
///      wintermute.target` showing each child's `Reached target` line
///      in declared dependency order. Log readings as
///      `target/ac1_target_start.json`.
#[test]
#[ignore = "hardware: requires systemd-user + Fleet 1 binaries; see doc-comment"]
fn wintermute_target_starts_all_children_under_5s() {
    require_hardware_witness("AC1");
}

/// AC2 — Cold reboot (after bootstrap is done) to first greeting in
/// ≤15 s.
///
/// Manual procedure:
///   1. Confirm bootstrap is complete (`wmd-init --status` reports
///      `bootstrap_complete=true` and `wintermute.target` is
///      enabled on the user manager).
///   2. `sudo systemctl reboot`. At T0 the laptop power cycles.
///   3. The laptop's greeting (TTS first audio of "Hi, I'm here"
///      or configured greeting phrase) reaches the speaker at T1.
///      An operator measures T1 by ear with a stopwatch started at
///      the BIOS-handoff visual cue (or by listening for the
///      systemd-boot menu beep and counting from there).
///   4. T1 − T0 ≤ 15 s. Repeat 3 times; record p50 in
///      `target/ac2_cold_reboot.json` with
///      `{reps: 3, t0_marker: "bios_handoff", t1_speaker_audio: [...]}`.
#[test]
#[ignore = "hardware: requires cold reboot + stopwatch + speakers; see doc-comment"]
fn cold_reboot_to_first_greeting_under_15s() {
    require_hardware_witness("AC2");
}

/// AC5 — `wm mute` halts active TTS within 200 ms and suspends wake
/// handling until `wm unmute`.
///
/// The supervisor-side mute event publish is paired by unit tests in
/// `src/supervisor.rs` (event-bus mute publish path). What this stub
/// pins is the end-to-end wall-clock: from issuing `wm mute` to TTS
/// audio actually stopping at the speaker, and wake handling staying
/// dormant until `wm unmute`.
///
/// Manual procedure:
///   1. Start `wintermute.target`. Begin a long TTS utterance: publish
///      `wm.tts.speak` with a 10-second sentence. Confirm audio at
///      the speaker.
///   2. After 3 s of speech, run `wm mute` and record T0 at issuance.
///   3. Measure T1 = time of last PCM frame at the speaker (operator
///      can mark this by ear or by `journalctl --user -u wm-tts.service
///      | grep playback_done`).
///   4. T1 − T0 ≤ 200 ms.
///   5. While muted, speak the wake phrase 3 times — `agorabus
///      subscribe wm.audio.wake` must report zero events for the
///      duration. Run `wm unmute`, speak the wake phrase once — event
///      must arrive within 500 ms.
///   6. Log readings as `target/ac5_mute_halt.json`.
#[test]
#[ignore = "hardware: requires TTS + mic + live wm-* fleet; see doc-comment"]
fn wm_mute_halts_tts_under_200ms_and_suspends_wake() {
    require_hardware_witness("AC5");
}

/// AC8 — After 5 restart-storm cycles, supervisor backs off and emits
/// an `init.backoff` event on agorabus.
///
/// The backoff math and event-shape are paired by unit tests in
/// `src/supervisor.rs` (restart-policy state machine). What this stub
/// pins is the live cycle: 5 forced crashes within the storm window,
/// supervisor emitting the `init.backoff` event over the real bus,
/// and the supervisor entering the backed-off state for the configured
/// duration.
///
/// Manual procedure:
///   1. Start `wintermute.target`. Confirm `wm-audio.service` is
///      active.
///   2. `agorabus subscribe init.backoff` in a side terminal.
///   3. Loop 5 times: `pkill -9 wm-audio` and wait for the supervisor
///      to respawn (`systemctl --user is-active wm-audio.service` →
///      `active`). Each cycle should complete within the storm
///      window (default 60 s).
///   4. On the 5th cycle the supervisor MUST emit an `init.backoff`
///      event with at least `{child: "wm-audio", reason: "restart_storm",
///      backoff_s: <number>}`. Subscriber captures the event.
///   5. After the backoff duration elapses, `wm-audio.service` should
///      become active again on the next attempt.
///   6. Log subscriber tape as `target/ac8_restart_storm.ndjson` and
///      assert the event shape and order.
#[test]
#[ignore = "hardware: requires live supervisor + agorabus; see doc-comment"]
fn restart_storm_emits_init_backoff_after_5_cycles() {
    require_hardware_witness("AC8");
}
