//! Acceptance tests for `wintermute-watchdog` (PRD §3, AC1–AC5).
//!
//! These tests exercise the watchdog engine via the [`SystemctlBackend`]
//! trait using in-process stubs — no live systemd is required and no live
//! wm-* daemon is touched.

use std::cell::Cell;
use std::time::{Duration, Instant};

use wintermute_platform::watchdog::{
    GIVE_UP_AFTER, INITIAL_BACKOFF_SECS, MAX_BACKOFF_SECS, WATCHDOG_UNIT_NAME,
    HealthEvent, RecoveryState, SystemctlBackend, WatchdogEngine, WatchdogError,
    is_wintermute_unit,
};

// ── AC1a — Backoff schedule is monotonic and capped ───────────────────────

#[test]
fn ac1a_backoff_schedule_monotonic_and_capped() {
    let mut state = RecoveryState::default();
    let t0 = Instant::now();
    let mut prev_backoff = Duration::ZERO;

    for i in 0..GIVE_UP_AFTER {
        let current = state.current_backoff;
        // Monotonic (allow equal due to ceiling clamping).
        assert!(
            current >= prev_backoff,
            "backoff not monotonic at attempt {i}: {current:?} < {prev_backoff:?}"
        );
        // Ceiling respected.
        assert!(
            current <= Duration::from_secs(MAX_BACKOFF_SECS),
            "backoff {current:?} exceeds MAX_BACKOFF_SECS at attempt {i}"
        );
        prev_backoff = current;
        let gave_up = state.record_failure(t0 + Duration::from_secs(u64::from(i) * 1_000));
        if gave_up {
            assert_eq!(
                i + 1,
                GIVE_UP_AFTER,
                "gave up early at attempt {}",
                i + 1
            );
            return;
        }
    }
    panic!("did not give up after GIVE_UP_AFTER={GIVE_UP_AFTER} failures");
}

// ── AC1b — Per-unit backoff states are independent ────────────────────────

#[test]
fn ac1b_per_unit_backoff_state_independent() {
    struct FixedFailed(Vec<String>);
    impl SystemctlBackend for FixedFailed {
        fn list_failed_units(&self) -> Result<Vec<String>, WatchdogError> {
            Ok(self.0.clone())
        }
        fn reset_failed(&self, _u: &str) -> Result<(), WatchdogError> {
            Ok(())
        }
        fn start_unit(&self, _u: &str) -> Result<(), WatchdogError> {
            Ok(())
        }
        fn unit_is_failed(&self, _u: &str) -> Result<bool, WatchdogError> {
            // Unit A always fails; unit B always succeeds.
            Ok(_u == "wm-audio.service")
        }
    }

    let mut engine = WatchdogEngine::new(FixedFailed(vec![
        "wm-audio.service".into(),
        "wm-tts.service".into(),
    ]));
    let t0 = Instant::now();

    // Run enough ticks to give wm-audio several failures.
    for i in 0..5u64 {
        let _ = engine.tick(t0 + Duration::from_secs(i * MAX_BACKOFF_SECS * 4));
    }

    let audio_failures = engine
        .table()
        .get("wm-audio.service")
        .map(|s| s.consecutive_failures)
        .unwrap_or(0);
    let tts_failures = engine
        .table()
        .get("wm-tts.service")
        .map(|s| s.consecutive_failures)
        .unwrap_or(0);

    // wm-tts always succeeds so its failures must be 0.
    assert_eq!(tts_failures, 0, "wm-tts should have 0 failures (always recovers)");
    // wm-audio always stays failed so it should have accumulated failures.
    assert!(audio_failures > 0, "wm-audio should have >0 failures");
    // They must differ — independent state.
    assert_ne!(
        audio_failures, tts_failures,
        "per-unit states should be independent"
    );
}

// ── AC1c — Give-up after K failures ──────────────────────────────────────

#[test]
fn ac1c_give_up_after_k_consecutive_failures() {
    let mut state = RecoveryState::default();
    let t0 = Instant::now();
    let mut gave_up_at: Option<u32> = None;

    for i in 0..GIVE_UP_AFTER + 5 {
        let gave_up = state.record_failure(t0 + Duration::from_secs(u64::from(i) * 1_000));
        if gave_up && gave_up_at.is_none() {
            gave_up_at = Some(i + 1);
        }
    }

    assert_eq!(
        gave_up_at,
        Some(GIVE_UP_AFTER),
        "give-up should trigger at exactly GIVE_UP_AFTER={GIVE_UP_AFTER} failures"
    );
    assert!(state.gave_up, "state.gave_up must be true");
    // After give-up, may_attempt must always be false.
    assert!(
        !state.may_attempt(t0 + Duration::from_secs(999_999)),
        "gave-up unit must never be attempted again"
    );
}

// ── AC1d — wintermute-unit filter: non-wintermute units ignored ───────────

#[test]
fn ac1d_wintermute_unit_filter_accepts_fleet() {
    let fleet_units = [
        "wmd-init.service",
        "wm-audio.service",
        "wm-tts.service",
        "wm-stt.service",
        "wm-dialog.service",
        "wintermute-ready.service",
    ];
    for &u in &fleet_units {
        assert!(is_wintermute_unit(u), "expected {u} to be a wintermute unit");
    }
}

#[test]
fn ac1d_wintermute_unit_filter_rejects_foreign() {
    let foreign = [
        "sshd.service",
        "NetworkManager.service",
        "pipewire.service",
        "gdm.service",
        "systemd-logind.service",
        "dbus.service",
    ];
    for &u in &foreign {
        assert!(
            !is_wintermute_unit(u),
            "expected {u} to be rejected by wintermute filter"
        );
    }
}

// ── AC1e — wm.health.* event shape on give-up ────────────────────────────

#[test]
fn ac1e_health_event_shape_on_give_up() {
    let ev = HealthEvent {
        unit: "wm-audio.service".into(),
        attempts: GIVE_UP_AFTER,
    };
    let s = ev.format();
    assert!(
        s.starts_with("wm.health."),
        "event must start with wm.health.: {s}"
    );
    assert!(
        s.contains("wm-audio.service"),
        "event must contain unit name: {s}"
    );
    assert!(
        s.contains(&format!("attempts={GIVE_UP_AFTER}")),
        "event must contain attempts count: {s}"
    );
}

// ── AC2 — Backoff gates (verify against backoff table, not wall-clock) ────

#[test]
fn ac2_backoff_gate_respected_by_engine() {
    struct FirstTickFail {
        attempts: Cell<u32>,
    }
    impl SystemctlBackend for FirstTickFail {
        fn list_failed_units(&self) -> Result<Vec<String>, WatchdogError> {
            Ok(vec!["wm-audio.service".into()])
        }
        fn reset_failed(&self, _u: &str) -> Result<(), WatchdogError> {
            Ok(())
        }
        fn start_unit(&self, _u: &str) -> Result<(), WatchdogError> {
            self.attempts.set(self.attempts.get() + 1);
            Ok(())
        }
        fn unit_is_failed(&self, _u: &str) -> Result<bool, WatchdogError> {
            Ok(true) // never recovers
        }
    }

    let backend = FirstTickFail { attempts: Cell::new(0) };
    let mut engine = WatchdogEngine::new(backend);
    let t0 = Instant::now();

    // Tick 0: first attempt happens.
    engine.tick(t0).unwrap();
    let after_first = engine.backend.attempts.get();
    assert_eq!(after_first, 1, "expected exactly 1 attempt on tick 0");

    // Tick 1 at t0 + 1ms: should NOT trigger — backoff not elapsed.
    engine.tick(t0 + Duration::from_millis(1)).unwrap();
    assert_eq!(
        engine.backend.attempts.get(),
        1,
        "should not attempt again before backoff elapses"
    );

    // Tick 2 at t0 + INITIAL_BACKOFF_SECS + 1s: should trigger.
    engine
        .tick(t0 + Duration::from_secs(INITIAL_BACKOFF_SECS + 1))
        .unwrap();
    assert!(
        engine.backend.attempts.get() > 1,
        "expected a second attempt after backoff elapsed"
    );
}

// ── AC3 — Scoped to wintermute: non-wintermute failed unit never touched ──

#[test]
fn ac3_non_wintermute_unit_never_touched() {
    struct SpyBackend {
        touched_units: std::cell::RefCell<Vec<String>>,
    }
    impl SystemctlBackend for SpyBackend {
        fn list_failed_units(&self) -> Result<Vec<String>, WatchdogError> {
            Ok(vec![
                "sshd.service".into(),
                "NetworkManager.service".into(),
                "bluetooth.service".into(),
            ])
        }
        fn reset_failed(&self, u: &str) -> Result<(), WatchdogError> {
            self.touched_units.borrow_mut().push(u.to_string());
            Ok(())
        }
        fn start_unit(&self, u: &str) -> Result<(), WatchdogError> {
            self.touched_units.borrow_mut().push(u.to_string());
            Ok(())
        }
        fn unit_is_failed(&self, _u: &str) -> Result<bool, WatchdogError> {
            Ok(false)
        }
    }

    let spy = SpyBackend {
        touched_units: std::cell::RefCell::new(Vec::new()),
    };
    let mut engine = WatchdogEngine::new(spy);
    engine.tick(Instant::now()).unwrap();

    assert!(
        engine.backend.touched_units.borrow().is_empty(),
        "non-wintermute units were touched: {:?}",
        engine.backend.touched_units.borrow()
    );
}

// ── AC4 — No self-loop: watchdog does not recover its own unit ────────────

#[test]
fn ac4_watchdog_does_not_recover_itself() {
    struct SelfFailedBackend {
        self_touched: Cell<bool>,
    }
    impl SystemctlBackend for SelfFailedBackend {
        fn list_failed_units(&self) -> Result<Vec<String>, WatchdogError> {
            Ok(vec![WATCHDOG_UNIT_NAME.to_string()])
        }
        fn reset_failed(&self, _u: &str) -> Result<(), WatchdogError> {
            self.self_touched.set(true);
            Ok(())
        }
        fn start_unit(&self, _u: &str) -> Result<(), WatchdogError> {
            self.self_touched.set(true);
            Ok(())
        }
        fn unit_is_failed(&self, _u: &str) -> Result<bool, WatchdogError> {
            Ok(false)
        }
    }

    let spy = SelfFailedBackend { self_touched: Cell::new(false) };
    let mut engine = WatchdogEngine::new(spy);
    engine.tick(Instant::now()).unwrap();

    assert!(
        !engine.backend.self_touched.get(),
        "watchdog attempted to recover its own unit {WATCHDOG_UNIT_NAME}"
    );
}

// ── AC5 — Health event emitted exactly once on give-up ───────────────────

#[test]
fn ac5_health_event_emitted_on_give_up() {
    struct AlwaysFailed;
    impl SystemctlBackend for AlwaysFailed {
        fn list_failed_units(&self) -> Result<Vec<String>, WatchdogError> {
            Ok(vec!["wm-dialog.service".into()])
        }
        fn reset_failed(&self, _u: &str) -> Result<(), WatchdogError> {
            Ok(())
        }
        fn start_unit(&self, _u: &str) -> Result<(), WatchdogError> {
            Ok(())
        }
        fn unit_is_failed(&self, _u: &str) -> Result<bool, WatchdogError> {
            Ok(true) // never recovers
        }
    }

    let mut engine = WatchdogEngine::new(AlwaysFailed);
    let t0 = Instant::now();
    let mut all_events: Vec<HealthEvent> = Vec::new();

    // Tick past give-up, giving generous backoff margin.
    for i in 0..u64::from(GIVE_UP_AFTER) + 3 {
        let events = engine
            .tick(t0 + Duration::from_secs(i * MAX_BACKOFF_SECS * 4))
            .unwrap();
        all_events.extend(events);
    }

    let give_up_events: Vec<_> = all_events
        .iter()
        .filter(|e| e.unit == "wm-dialog.service")
        .collect();
    assert!(!give_up_events.is_empty(), "expected at least one give-up event for wm-dialog.service");
    // The give-up event should have attempts == GIVE_UP_AFTER.
    assert_eq!(
        give_up_events[0].attempts, GIVE_UP_AFTER,
        "give-up event must report GIVE_UP_AFTER={GIVE_UP_AFTER} attempts"
    );
}
