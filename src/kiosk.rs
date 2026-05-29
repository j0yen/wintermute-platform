//! Kiosk-mode install support (PRD-wintermute-companion-boot).
//!
//! `install.sh --kiosk` is responsible for wiring autologin (greetd),
//! `loginctl enable-linger`, a tmpfiles.d rule, and a boot-recovery
//! service so the laptop boots straight into `wintermute.target` with
//! no keyboard input. This module owns the *data* that drives the
//! script: the kiosk plan, the production paths, and the renderable
//! systemd / tmpfiles / greetd payloads.
//!
//! Keeping the bodies of every installable artifact here (rather than
//! inline strings in `install.sh`) makes them unit-testable and
//! deterministic — `install.sh --kiosk --dry-run` re-emits exactly the
//! same bytes the wet run would write.

use std::fmt::Write as _;

/// Production install prefix for kiosk deployments (PRD §2.2).
///
/// All `wm-*` binaries land here on `install.sh --kiosk`. Developer
/// installs (no `--kiosk`) continue to use `~/.local/bin/` and are
/// considered a convenience fallback, not a deployment target.
pub const KIOSK_BIN_PREFIX: &str = "/usr/local/bin";

/// Developer install prefix (no `--kiosk`).
pub const DEV_BIN_PREFIX: &str = "~/.local/bin";

/// Canonical wm-* binaries that ship with wintermute-platform plus the
/// downstream fleet binaries `install.sh --kiosk` will look for (PRD AC6).
///
/// The PRD enumerates seven binaries:
/// `wm-tts wm-stt wm-audio wm-dialog wmd wmd-init wm-bootstrap`.
pub const KIOSK_BINARIES: &[&str] = &[
    "wm-tts",
    "wm-stt",
    "wm-audio",
    "wm-dialog",
    "wmd",
    "wmd-init",
    "wm-bootstrap",
];

/// Installed-marker path. Presence determines `wm.boot.first` vs
/// `wm.boot.recovered` in `wmd-init` (PRD §2.3).
pub const INSTALLED_MARKER_PATH: &str = "/var/lib/wintermute/installed";

/// Recovery service unit path (system-level, per PRD §5 default).
pub const RECOVERY_SERVICE_PATH: &str = "/usr/lib/systemd/system/wintermute-boot-recovery.service";

/// tmpfiles.d rule path (PRD §2.1 step 4).
pub const TMPFILES_RULE_PATH: &str = "/usr/lib/tmpfiles.d/wintermute.conf";

/// greetd config path (PRD §2.1 step 1).
pub const GREETD_CONFIG_PATH: &str = "/etc/greetd/config.toml";

/// Default kiosk user (per PRD §2.1 step 1).
pub const DEFAULT_KIOSK_USER: &str = "wintermute";

/// agorabus topic for the first boot of a freshly-installed device.
pub const TOPIC_BOOT_FIRST: &str = "wm.boot.first";

/// agorabus topic for every subsequent boot (incl. power-loss recovery).
pub const TOPIC_BOOT_RECOVERED: &str = "wm.boot.recovered";

/// Discrete step in the kiosk install pipeline (PRD §2.1).
///
/// Each step is named, has a short rationale, and can answer whether
/// it requires root. Steps are emitted in execution order so the
/// dry-run printer matches what a wet run would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KioskStep {
    /// Stable identifier (e.g. `greetd-config`).
    pub id: &'static str,
    /// One-line human description.
    pub description: String,
    /// Filesystem paths this step writes to, in order.
    pub writes: Vec<String>,
    /// `true` if the step modifies system state (units, /etc, /usr).
    pub requires_root: bool,
}

/// Render-only plan of what `install.sh --kiosk` will do.
///
/// Built from the live config (which user, which mode), serialized
/// for `--dry-run`. The wet code path enacts the steps in the same
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KioskPlan {
    /// Kiosk user (greetd autologin target).
    pub user: String,
    /// Where wm-* binaries should be installed.
    pub bin_prefix: String,
    /// Steps in execution order.
    pub steps: Vec<KioskStep>,
}

impl KioskPlan {
    /// Build the canonical 7-step plan for a given user.
    #[must_use]
    pub fn for_user(user: &str) -> Self {
        let steps = vec![
            KioskStep {
                id: "install-binaries",
                description: format!(
                    "install wm-* binaries to {KIOSK_BIN_PREFIX}/ (kiosk-mode path convention)"
                ),
                writes: KIOSK_BINARIES
                    .iter()
                    .map(|b| format!("{KIOSK_BIN_PREFIX}/{b}"))
                    .collect(),
                requires_root: true,
            },
            KioskStep {
                id: "greetd-config",
                description: format!("write greetd autologin config for user={user}"),
                writes: vec![GREETD_CONFIG_PATH.to_string()],
                requires_root: true,
            },
            KioskStep {
                id: "enable-linger",
                description: format!("loginctl enable-linger {user}"),
                writes: vec![format!("/var/lib/systemd/linger/{user}")],
                requires_root: true,
            },
            KioskStep {
                id: "tmpfiles-rule",
                description: "install tmpfiles.d rule for /run/wintermute/".to_string(),
                writes: vec![TMPFILES_RULE_PATH.to_string()],
                requires_root: true,
            },
            KioskStep {
                id: "recovery-service",
                description: "install (do not enable) wintermute-boot-recovery.service"
                    .to_string(),
                writes: vec![RECOVERY_SERVICE_PATH.to_string()],
                requires_root: true,
            },
            KioskStep {
                id: "installed-marker",
                description: format!(
                    "create installed marker at {INSTALLED_MARKER_PATH} (drives first-vs-recovered boot)"
                ),
                writes: vec![INSTALLED_MARKER_PATH.to_string()],
                requires_root: true,
            },
            KioskStep {
                id: "enable-user-target",
                description: format!(
                    "enable wintermute.target on user {user} via --machine= or PAM session"
                ),
                writes: vec![format!(
                    "/var/lib/systemd/linger/{user} (target-default-symlink)"
                )],
                requires_root: true,
            },
        ];
        Self {
            user: user.to_string(),
            bin_prefix: KIOSK_BIN_PREFIX.to_string(),
            steps,
        }
    }

    /// Render the plan as a human-readable dry-run report.
    ///
    /// Stable line-oriented format so `install.sh --kiosk --dry-run`
    /// output is easy to diff across versions and easy to embed in
    /// receipts.
    #[must_use]
    pub fn dry_run_report(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "kiosk-install plan for user={} bin_prefix={}",
            self.user, self.bin_prefix
        );
        let _ = writeln!(out, "steps: {}", self.steps.len());
        for (i, s) in self.steps.iter().enumerate() {
            let _ = writeln!(
                out,
                "  [{}/{}] {} (root={}): {}",
                i + 1,
                self.steps.len(),
                s.id,
                s.requires_root,
                s.description
            );
            for w in &s.writes {
                let _ = writeln!(out, "         writes: {w}");
            }
        }
        out
    }

    /// `true` if no plan step depends on stdin or interactive prompts.
    ///
    /// Structural check for AC4 (no keyboard required). Every step's
    /// `id` must be in the deterministic, non-interactive set.
    #[must_use]
    pub fn is_non_interactive(&self) -> bool {
        // The complete set of step ids the constructor emits. Anything
        // outside this set (e.g. a future `read -p` step) would flip
        // this to false at the type level.
        const NON_INTERACTIVE_IDS: &[&str] = &[
            "install-binaries",
            "greetd-config",
            "enable-linger",
            "tmpfiles-rule",
            "recovery-service",
            "installed-marker",
            "enable-user-target",
        ];
        self.steps
            .iter()
            .all(|s| NON_INTERACTIVE_IDS.contains(&s.id))
    }
}

/// Render the systemd unit body for `wintermute-boot-recovery.service`
/// (PRD §2.4, §5).
///
/// System-level service that, on every boot, waits 30 s and if
/// `wintermute.target` is not active for `user`, asks user-systemd to
/// start it. Idempotent: if the target is already active, the
/// `start-if-not-active` shell runs cleanly with no effect.
#[must_use]
pub fn recovery_service_unit(user: &str) -> String {
    format!(
        "[Unit]\n\
         Description=wintermute boot-recovery (start wintermute.target if it failed at login)\n\
         Documentation=https://github.com/j0yen/wintermute-platform\n\
         After=multi-user.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         # Wait 30s for the autologin chain to settle, then verify\n\
         # wintermute.target is active for the kiosk user. If not,\n\
         # poke it into life via the user's systemd manager.\n\
         ExecStart=/bin/sh -c 'sleep 30; \
runuser -u {user} -- systemctl --user is-active --quiet wintermute.target \
|| runuser -u {user} -- systemctl --user start wintermute.target'\n\
         RemainAfterExit=no\n\
         # Do not block boot; recovery is opportunistic.\n\
         TimeoutStartSec=120\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

/// Render the tmpfiles.d rule for `/run/wintermute/` (PRD §2.1 step 4).
#[must_use]
pub fn tmpfiles_rule(user: &str) -> String {
    // Standard systemd-tmpfiles syntax: type path mode user group age arg
    format!("d /run/wintermute 0755 {user} {user} - -\n")
}

/// Render the production greetd config (PRD §2.1 step 1).
///
/// Replaces the shipped `config.toml.example` for kiosk installs. The
/// `initial_session` field is what produces a no-keyboard autologin on
/// the first VT.
#[must_use]
pub fn greetd_config(user: &str) -> String {
    format!(
        "# greetd config — wintermute kiosk autologin\n\
         # Installed by install.sh --kiosk for user={user}.\n\
         # Reverted by install.sh --uninstall-kiosk.\n\
         \n\
         [terminal]\n\
         vt = 1\n\
         \n\
         [default_session]\n\
         command = \"agreety --cmd wintermute-session\"\n\
         user = \"{user}\"\n\
         \n\
         [initial_session]\n\
         command = \"wintermute-session\"\n\
         user = \"{user}\"\n"
    )
}

/// Resolve the production install path for a `wm-*` binary in kiosk mode.
///
/// Always `/usr/local/bin/<name>`. Standardizes the path-drift fix
/// from the PRD (today binaries land at both `~/.cargo/bin/` and
/// `~/.local/bin/`).
#[must_use]
pub fn kiosk_install_path(binary: &str) -> String {
    format!("{KIOSK_BIN_PREFIX}/{binary}")
}

/// Boot-marker state (drives wm.boot.first vs wm.boot.recovered).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMarkerState {
    /// Marker absent → this is the device's first boot after install.
    /// Next `wmd-init` activation publishes `wm.boot.first` and the
    /// installed marker is written.
    First,
    /// Marker present → ordinary boot or power-loss recovery.
    /// `wmd-init` publishes `wm.boot.recovered` and stays silent.
    Recovered,
}

impl BootMarkerState {
    /// Classify based on a marker-path existence probe.
    ///
    /// The caller is expected to wrap `Path::exists()` so this stays
    /// pure and testable from the unit suite.
    #[must_use]
    pub const fn from_marker_present(present: bool) -> Self {
        if present {
            Self::Recovered
        } else {
            Self::First
        }
    }

    /// agorabus topic to publish on this boot.
    #[must_use]
    pub const fn topic(self) -> &'static str {
        match self {
            Self::First => TOPIC_BOOT_FIRST,
            Self::Recovered => TOPIC_BOOT_RECOVERED,
        }
    }

    /// `true` when wmd-init should ask wm-dialog to speak the boot phrase.
    ///
    /// First boot speaks; recovered boots stay silent (PRD §2.3, AC5).
    #[must_use]
    pub const fn should_speak(self) -> bool {
        matches!(self, Self::First)
    }
}

/// CLI surface of `install.sh` — what flags we recognise.
///
/// Modelled in Rust so we can exercise the flag parser without
/// shelling out. `install.sh` is a thin pass-through that prints the
/// rendered plan and (when not `--dry-run`) writes the files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallArgs {
    /// `--kiosk`: do the boot-resilience steps from PRD §2.1.
    pub kiosk: bool,
    /// `--uninstall-kiosk`: revert greetd / recovery / tmpfiles.
    /// Mutually exclusive with `--kiosk`.
    pub uninstall_kiosk: bool,
    /// `--dry-run`: render the plan, write nothing.
    pub dry_run: bool,
    /// `--user <name>`: override the kiosk user.
    pub user: String,
}

impl Default for InstallArgs {
    fn default() -> Self {
        Self {
            kiosk: false,
            uninstall_kiosk: false,
            dry_run: false,
            user: DEFAULT_KIOSK_USER.to_string(),
        }
    }
}

/// Parse the install.sh-style argv slice.
///
/// # Errors
/// Returns an error describing the bad flag if mutually-exclusive
/// flags collide, an unknown flag is passed, or a `--user` flag is
/// missing its value.
pub fn parse_install_args<I, S>(args: I) -> Result<InstallArgs, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = InstallArgs::default();
    let mut it = args.into_iter();
    // Skip argv[0]; if it's actually a flag the caller passed args
    // pre-stripped, the loop below handles it.
    let mut peek: Option<String> = it.next().map(|s| s.as_ref().to_string());
    if peek
        .as_deref()
        .is_some_and(|s| s.starts_with("--") || s.starts_with('-'))
    {
        // Caller passed flags directly; don't drop the first one.
    } else {
        peek = it.next().map(|s| s.as_ref().to_string());
    }
    loop {
        let Some(a) = peek.take() else { break };
        match a.as_str() {
            "--kiosk" => out.kiosk = true,
            "--uninstall-kiosk" => out.uninstall_kiosk = true,
            "--dry-run" => out.dry_run = true,
            "--user" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--user requires a value".to_string())?;
                out.user = v.as_ref().to_string();
            }
            "-h" | "--help" => {
                return Err(
                    "usage: install.sh [--kiosk|--uninstall-kiosk] [--dry-run] [--user NAME]"
                        .into(),
                );
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        peek = it.next().map(|s| s.as_ref().to_string());
    }
    if out.kiosk && out.uninstall_kiosk {
        return Err("--kiosk and --uninstall-kiosk are mutually exclusive".into());
    }
    Ok(out)
}

/// Steps for the uninstall path (PRD AC9).
///
/// Reverts what `--kiosk` wrote, leaves the wm-* binaries intact.
#[must_use]
pub fn uninstall_steps(user: &str) -> Vec<KioskStep> {
    vec![
        KioskStep {
            id: "remove-recovery-service",
            description: "remove wintermute-boot-recovery.service".to_string(),
            writes: vec![RECOVERY_SERVICE_PATH.to_string()],
            requires_root: true,
        },
        KioskStep {
            id: "remove-greetd-config",
            description: format!(
                "restore greetd config to upstream default (was: kiosk for user={user})"
            ),
            writes: vec![GREETD_CONFIG_PATH.to_string()],
            requires_root: true,
        },
        KioskStep {
            id: "remove-tmpfiles-rule",
            description: "remove tmpfiles.d rule for /run/wintermute/".to_string(),
            writes: vec![TMPFILES_RULE_PATH.to_string()],
            requires_root: true,
        },
        KioskStep {
            id: "disable-linger",
            description: format!("loginctl disable-linger {user}"),
            writes: vec![format!("/var/lib/systemd/linger/{user}")],
            requires_root: true,
        },
    ]
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn kiosk_install_path_uses_usr_local_bin() {
        // AC6: which wm-tts wm-stt ... returns /usr/local/bin/...
        assert_eq!(kiosk_install_path("wm-tts"), "/usr/local/bin/wm-tts");
        assert_eq!(kiosk_install_path("wmd-init"), "/usr/local/bin/wmd-init");
        assert_eq!(
            kiosk_install_path("wm-bootstrap"),
            "/usr/local/bin/wm-bootstrap"
        );
    }

    #[test]
    fn kiosk_plan_has_seven_canonical_steps() {
        let p = KioskPlan::for_user("wintermute");
        assert_eq!(p.steps.len(), 7);
        let ids: Vec<&str> = p.steps.iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec![
                "install-binaries",
                "greetd-config",
                "enable-linger",
                "tmpfiles-rule",
                "recovery-service",
                "installed-marker",
                "enable-user-target",
            ]
        );
    }

    #[test]
    fn kiosk_plan_is_non_interactive() {
        // AC4: structural check — no plan step requires keyboard input.
        let p = KioskPlan::for_user("wintermute");
        assert!(
            p.is_non_interactive(),
            "kiosk plan must be fully non-interactive (AC4)"
        );
    }

    #[test]
    fn kiosk_plan_install_binaries_step_lists_all_seven_kiosk_binaries() {
        // AC6: kiosk install standardizes wm-tts, wm-stt, wm-audio,
        // wm-dialog, wmd, wmd-init, wm-bootstrap at /usr/local/bin/.
        let p = KioskPlan::for_user("wintermute");
        let install_step = p
            .steps
            .iter()
            .find(|s| s.id == "install-binaries")
            .expect("install-binaries step present");
        assert_eq!(install_step.writes.len(), 7);
        for bin in KIOSK_BINARIES {
            let expected = format!("/usr/local/bin/{bin}");
            assert!(
                install_step.writes.contains(&expected),
                "missing {expected} in install-binaries writes"
            );
        }
    }

    #[test]
    fn kiosk_plan_propagates_user_into_greetd_step() {
        let p = KioskPlan::for_user("mary");
        let greetd_step = p
            .steps
            .iter()
            .find(|s| s.id == "greetd-config")
            .expect("greetd-config step");
        assert!(greetd_step.description.contains("user=mary"));
        assert_eq!(p.user, "mary");
    }

    #[test]
    fn dry_run_report_is_stable_line_oriented() {
        let r = KioskPlan::for_user("wintermute").dry_run_report();
        // Headline + step count + 7 numbered steps + writes lines.
        assert!(r.starts_with("kiosk-install plan for user=wintermute"));
        assert!(r.contains("steps: 7"));
        assert!(r.contains("[1/7] install-binaries"));
        assert!(r.contains("[7/7] enable-user-target"));
        assert!(r.contains("writes: /usr/local/bin/wmd-init"));
    }

    #[test]
    fn recovery_service_unit_targets_correct_user_and_target() {
        let body = recovery_service_unit("wintermute");
        // Structural: it's a [Unit]/[Service]/[Install] file.
        assert!(body.contains("[Unit]"));
        assert!(body.contains("[Service]"));
        assert!(body.contains("[Install]"));
        // Type=oneshot is the right pattern for a one-poke job.
        assert!(body.contains("Type=oneshot"));
        // 30-second settle window per PRD §2.1 step 7.
        assert!(body.contains("sleep 30"));
        // Targets the right systemd-user target on the right user.
        assert!(body.contains("runuser -u wintermute"));
        assert!(body.contains("systemctl --user start wintermute.target"));
        // Boot-time wiring.
        assert!(body.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn recovery_service_unit_user_is_parameterised() {
        let body = recovery_service_unit("mary");
        assert!(body.contains("runuser -u mary"));
        assert!(!body.contains("runuser -u wintermute"));
    }

    #[test]
    fn tmpfiles_rule_creates_run_wintermute_dir() {
        let r = tmpfiles_rule("wintermute");
        // type=d, path, mode, user, group present and correctly ordered.
        assert!(r.starts_with("d /run/wintermute 0755 wintermute wintermute"));
        assert!(r.ends_with('\n'));
    }

    #[test]
    fn greetd_config_renders_autologin_for_user() {
        let cfg = greetd_config("wintermute");
        // greetd's [initial_session] is the autologin hook.
        assert!(cfg.contains("[initial_session]"));
        assert!(cfg.contains("command = \"wintermute-session\""));
        assert!(cfg.contains("user = \"wintermute\""));
        // [terminal] vt = 1 keeps the autologin on VT1.
        assert!(cfg.contains("vt = 1"));
    }

    #[test]
    fn boot_marker_first_when_marker_absent() {
        // AC5: first activation publishes wm.boot.first (and speaks).
        let m = BootMarkerState::from_marker_present(false);
        assert_eq!(m, BootMarkerState::First);
        assert_eq!(m.topic(), "wm.boot.first");
        assert!(m.should_speak());
    }

    #[test]
    fn boot_marker_recovered_when_marker_present() {
        // AC5: second activation publishes wm.boot.recovered, silent.
        let m = BootMarkerState::from_marker_present(true);
        assert_eq!(m, BootMarkerState::Recovered);
        assert_eq!(m.topic(), "wm.boot.recovered");
        assert!(!m.should_speak());
    }

    #[test]
    fn parse_install_args_default_no_flags() {
        let a = parse_install_args(["install.sh"]).expect("parse");
        assert_eq!(a, InstallArgs::default());
        assert!(!a.kiosk);
        assert!(!a.uninstall_kiosk);
        assert!(!a.dry_run);
        assert_eq!(a.user, "wintermute");
    }

    #[test]
    fn parse_install_args_kiosk_and_dry_run() {
        let a = parse_install_args(["install.sh", "--kiosk", "--dry-run"]).expect("parse");
        assert!(a.kiosk);
        assert!(a.dry_run);
        assert!(!a.uninstall_kiosk);
    }

    #[test]
    fn parse_install_args_user_override() {
        let a = parse_install_args(["install.sh", "--kiosk", "--user", "mary"]).expect("parse");
        assert!(a.kiosk);
        assert_eq!(a.user, "mary");
    }

    #[test]
    fn parse_install_args_kiosk_and_uninstall_kiosk_collide() {
        let err =
            parse_install_args(["install.sh", "--kiosk", "--uninstall-kiosk"]).unwrap_err();
        assert!(err.contains("mutually exclusive"));
    }

    #[test]
    fn parse_install_args_user_without_value_errors() {
        let err = parse_install_args(["install.sh", "--user"]).unwrap_err();
        assert!(err.contains("--user requires a value"));
    }

    #[test]
    fn parse_install_args_unknown_flag_errors() {
        let err = parse_install_args(["install.sh", "--whatever"]).unwrap_err();
        assert!(err.contains("unknown argument"));
    }

    #[test]
    fn uninstall_steps_revert_kiosk_artifacts_but_not_binaries() {
        let steps = uninstall_steps("wintermute");
        let ids: Vec<&str> = steps.iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec![
                "remove-recovery-service",
                "remove-greetd-config",
                "remove-tmpfiles-rule",
                "disable-linger",
            ]
        );
        // PRD AC9: wm-* binaries are intentionally left intact.
        assert!(
            !steps.iter().any(|s| s.id == "remove-binaries"),
            "uninstall must not remove wm-* binaries (AC9)"
        );
    }

    #[test]
    fn installed_marker_path_lives_under_var_lib() {
        // PRD §6 references /var/lib/wintermute/installed.
        assert_eq!(INSTALLED_MARKER_PATH, "/var/lib/wintermute/installed");
    }

    #[test]
    fn recovery_service_path_is_system_level() {
        // PRD §5: defaults to system-level for the recovery service.
        assert!(
            RECOVERY_SERVICE_PATH.starts_with("/usr/lib/systemd/system/"),
            "recovery service must be system-level per PRD §5 default"
        );
    }

    #[test]
    fn kiosk_bin_prefix_is_usr_local_bin() {
        // AC6 + PRD §2.2: standardize on /usr/local/bin/.
        assert_eq!(KIOSK_BIN_PREFIX, "/usr/local/bin");
    }

    #[test]
    fn dev_bin_prefix_remains_local_home() {
        // PRD §2.2: developer convenience fallback unchanged.
        assert_eq!(DEV_BIN_PREFIX, "~/.local/bin");
    }
}
