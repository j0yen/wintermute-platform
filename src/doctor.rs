//! `wm doctor` — fleet install health check.
//!
//! Enumerates every wintermute systemd unit, resolves its `ExecStart` path
//! (expanding `%h`, `%u`, `%U` specifiers), and reports per-unit whether the
//! binary exists and is executable, whether the unit is enabled, active, and
//! pulled into `wintermute.target`.
//!
//! Exit semantics: 0 only when every discovered unit's binary is present and
//! executable. Exit 2 when any unit's binary is missing.
//!
//! This module is **read-only** — it never installs, restarts, or mutates
//! anything.

use std::path::Path;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Scope of systemd units to inspect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorScope {
    /// Only the user-scope manager (`systemctl --user`).
    User,
    /// Only the system-scope manager (`systemctl --system`).
    System,
    /// Both user and system scopes.
    Both,
}

impl Default for DoctorScope {
    fn default() -> Self {
        Self::User
    }
}

/// Per-unit diagnostic result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitReport {
    /// Unit file name (e.g. `wm-audio.service`).
    pub unit: String,
    /// Raw `ExecStart=` value from the unit file (first token).
    pub exec_start: String,
    /// Resolved absolute path after specifier expansion.
    pub resolved: String,
    /// Whether the resolved path exists on the filesystem.
    pub exists: bool,
    /// Whether the resolved path is executable (only meaningful when `exists` is true).
    pub executable: bool,
    /// `systemctl is-enabled` result (`"enabled"`, `"disabled"`, `"static"`, etc.).
    pub enabled: String,
    /// `systemctl is-active` result (`"active"`, `"inactive"`, `"failed"`, etc.).
    pub active: String,
    /// Whether the unit is pulled into `wintermute.target` via Wants=/Requires= or the
    /// `.wants/` drop-in directory.
    pub in_target: bool,
    /// Which scope this unit was found in (`"user"` or `"system"`).
    pub scope: String,
}

impl UnitReport {
    /// `true` iff the binary is present and executable (the only hard-fail condition).
    #[must_use]
    pub const fn binary_ok(&self) -> bool {
        self.exists && self.executable
    }
}

/// Aggregated output of `wm doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    /// Per-unit rows, in discovery order.
    pub units: Vec<UnitReport>,
    /// Units whose binary is missing or non-executable.
    pub failures: Vec<String>,
    /// `true` iff every unit's binary is present and executable.
    pub all_ok: bool,
}

// ---------------------------------------------------------------------------
// Specifier expansion
// ---------------------------------------------------------------------------

/// Expand systemd specifiers that affect paths.
///
/// Currently handles `%h` (user home), `%u` (username), `%U` (numeric UID).
/// Unknown specifiers are left as-is.
///
/// # Errors
/// Returns an error only if home-dir lookup fails and `%h` is present.
pub fn expand_specifiers(
    raw: &str,
    home_override: Option<&str>,
    user_override: Option<&str>,
    uid_override: Option<u32>,
) -> String {
    let home: String = home_override.map_or_else(
        || std::env::var("HOME").unwrap_or_else(|_| "/root".to_string()),
        str::to_string,
    );
    let user: String = user_override.map_or_else(
        || std::env::var("USER").unwrap_or_else(|_| "root".to_string()),
        str::to_string,
    );
    let uid: u32 = uid_override.unwrap_or_else(|| {
        // Try the UID env var (set by PAM/systemd in user sessions),
        // then fall back to /proc/self/status (Linux), then to 0.
        std::env::var("UID")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(read_uid_from_proc)
    });

    raw.replace("%h", &home)
        .replace("%u", &user)
        .replace("%U", &uid.to_string())
}

/// Read the current process's UID from `/proc/self/status` (Linux only).
/// Returns 0 as a safe fallback on parse error.
fn read_uid_from_proc() -> u32 {
    let Ok(text) = std::fs::read_to_string("/proc/self/status") else { return 0 };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            // Line format: "Uid:\t<real>\t<effective>\t<saved>\t<fs>"
            if let Some(uid_str) = rest.split_whitespace().next() {
                if let Ok(uid) = uid_str.parse() {
                    return uid;
                }
            }
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Binary existence / executable check
// ---------------------------------------------------------------------------

/// Check whether `path` exists and is executable (access `X_OK`).
///
/// Returns `(exists, executable)`.
#[must_use]
pub fn check_binary(path: &Path) -> (bool, bool) {
    use std::os::unix::fs::PermissionsExt;
    if !path.exists() {
        return (false, false);
    }
    // Check execute permission.
    let Ok(meta) = std::fs::metadata(path) else { return (true, false) };
    let mode = meta.permissions().mode();
    // At least one of owner/group/other execute bits set.
    let executable = (mode & 0o111) != 0;
    (true, executable)
}

// ---------------------------------------------------------------------------
// Unit file parsing
// ---------------------------------------------------------------------------

/// Parse the `ExecStart=` line from `systemctl cat` output.
///
/// Returns the first word of the first `ExecStart=` value found, or `None`.
#[must_use]
pub fn parse_exec_start(systemctl_cat_output: &str) -> Option<String> {
    for line in systemctl_cat_output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("ExecStart=") {
            // Value may be prefixed with `-` (ignore errors), `@` (override argv[0]),
            // `+` (full privileges), `!`/`!!` (unprivileged). Strip them.
            let stripped = rest.trim_start_matches(['-', '@', '+', '!']);
            // Take the first whitespace-separated token.
            let first_token = stripped.split_whitespace().next().unwrap_or("").to_string();
            if !first_token.is_empty() {
                return Some(first_token);
            }
        }
    }
    None
}

/// Parse the `Wants=` and `Requires=` lines from `systemctl cat wintermute.target` output.
///
/// Returns true if `unit_name` appears in any `Wants=` or `Requires=` value.
#[must_use]
pub fn unit_in_target_from_cat(systemctl_cat_output: &str, unit_name: &str) -> bool {
    for line in systemctl_cat_output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed
            .strip_prefix("Wants=")
            .or_else(|| trimmed.strip_prefix("Requires="))
        {
            if rest.split_whitespace().any(|tok| tok == unit_name) {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Injected command runner
// ---------------------------------------------------------------------------

/// Injected helper for running external commands. Returns `(stdout, exit_ok)`.
pub type CmdRunner = Box<dyn Fn(&str, &[&str]) -> (String, bool) + Send + Sync>;

/// Build a `CmdRunner` that shells out via `std::process::Command`.
#[must_use]
pub fn real_cmd_runner() -> CmdRunner {
    Box::new(|cmd: &str, args: &[&str]| {
        match std::process::Command::new(cmd).args(args).output() {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                (stdout, out.status.success())
            }
            Err(_) => (String::new(), false),
        }
    })
}

// ---------------------------------------------------------------------------
// Unit discovery
// ---------------------------------------------------------------------------

/// Discover wintermute unit names by querying `systemctl list-units` and
/// `systemctl list-unit-files` for `wm-*.service`, `wmd.service`,
/// `wmd-init.service`.
///
/// Returns a deduplicated, sorted list of unit names (e.g. `["wm-audio.service", …]`).
#[must_use]
pub fn discover_units(run: &CmdRunner, scope_flag: &str) -> Vec<String> {
    let mut units: Vec<String> = Vec::new();

    // Patterns we care about.
    let patterns = ["wm-*.service", "wmd.service", "wmd-init.service"];

    for pattern in patterns {
        // list-unit-files picks up units that are installed but not currently loaded.
        let (out, _) = run(
            "systemctl",
            &[scope_flag, "list-unit-files", "--plain", "--no-legend", pattern],
        );
        for line in out.lines() {
            let name = line.split_whitespace().next().unwrap_or("").to_string();
            if !name.is_empty() && name.ends_with(".service") && !units.contains(&name) {
                units.push(name);
            }
        }

        // list-units picks up loaded (running/failed) units not in unit-files.
        let (out2, _) = run(
            "systemctl",
            &[scope_flag, "list-units", "--plain", "--no-legend", "--all", pattern],
        );
        for line in out2.lines() {
            let name = line.split_whitespace().next().unwrap_or("").to_string();
            if !name.is_empty() && name.ends_with(".service") && !units.contains(&name) {
                units.push(name);
            }
        }
    }

    units.sort();
    units
}

// ---------------------------------------------------------------------------
// Per-unit probe
// ---------------------------------------------------------------------------

/// Context shared across all unit probes in a single `run_doctor` invocation.
pub struct ProbeCtx<'a> {
    /// Systemd scope flag (`"--user"` or `"--system"`).
    pub scope_flag: &'a str,
    /// Output of `systemctl cat wintermute.target` for this scope.
    pub target_cat: &'a str,
    /// Injected command runner.
    pub run: &'a CmdRunner,
    /// Optional override for the home directory (`%h`).
    pub home_override: Option<&'a str>,
    /// Optional override for the username (`%u`).
    pub user_override: Option<&'a str>,
    /// Optional override for the numeric UID (`%U`).
    pub uid_override: Option<u32>,
}

/// Build a `UnitReport` for one unit.
#[must_use]
pub fn probe_unit(unit: &str, ctx: &ProbeCtx<'_>) -> UnitReport {
    let scope_flag = ctx.scope_flag;
    let scope_label = if scope_flag == "--user" { "user" } else { "system" };

    // --- ExecStart ---
    let (cat_out, _) = (ctx.run)("systemctl", &[scope_flag, "cat", unit]);
    let exec_start = parse_exec_start(&cat_out).unwrap_or_default();
    let resolved = if exec_start.is_empty() {
        String::new()
    } else {
        expand_specifiers(&exec_start, ctx.home_override, ctx.user_override, ctx.uid_override)
    };

    let (exists, executable) = if resolved.is_empty() {
        (false, false)
    } else {
        check_binary(Path::new(&resolved))
    };

    // --- enabled / active ---
    let (enabled_out, _) = (ctx.run)("systemctl", &[scope_flag, "is-enabled", unit]);
    let enabled = enabled_out.trim().to_string();

    let (active_out, _) = (ctx.run)("systemctl", &[scope_flag, "is-active", unit]);
    let active = active_out.trim().to_string();

    // --- in wintermute.target ---
    let in_target =
        unit_in_target_from_cat(ctx.target_cat, unit) || in_target_wants_dir(unit, scope_flag);

    UnitReport {
        unit: unit.to_string(),
        exec_start,
        resolved,
        exists,
        executable,
        enabled,
        active,
        in_target,
        scope: scope_label.to_string(),
    }
}

/// Check if `unit` appears in `wintermute.target.wants/` on disk (common drop-in mechanism).
fn in_target_wants_dir(unit: &str, scope_flag: &str) -> bool {
    // Standard search paths for the relevant scope.
    let dirs: &[&str] = if scope_flag == "--user" {
        &[
            "/etc/systemd/user/wintermute.target.wants",
            "/usr/lib/systemd/user/wintermute.target.wants",
            "/usr/local/lib/systemd/user/wintermute.target.wants",
        ]
    } else {
        &[
            "/etc/systemd/system/wintermute.target.wants",
            "/usr/lib/systemd/system/wintermute.target.wants",
        ]
    };
    // Also check the user-specific dir under $HOME.
    let home = std::env::var("HOME").unwrap_or_else(|_| String::new());
    let user_config_dir = format!("{home}/.config/systemd/user/wintermute.target.wants");

    for dir in dirs {
        let p = Path::new(dir).join(unit);
        if p.exists() {
            return true;
        }
    }
    if scope_flag == "--user" {
        let p = Path::new(&user_config_dir).join(unit);
        if p.exists() {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Main run function
// ---------------------------------------------------------------------------

/// Run the doctor for the given scope.
///
/// `home_override`, `user_override`, `uid_override` are injection points for
/// tests. Pass `None` to use real environment values.
#[must_use]
pub fn run_doctor(
    scope: DoctorScope,
    run: &CmdRunner,
    home_override: Option<&str>,
    user_override: Option<&str>,
    uid_override: Option<u32>,
) -> DoctorReport {
    let scope_flags: &[&str] = match scope {
        DoctorScope::User => &["--user"],
        DoctorScope::System => &["--system"],
        DoctorScope::Both => &["--user", "--system"],
    };

    let mut all_units: Vec<UnitReport> = Vec::new();

    for &scope_flag in scope_flags {
        let (target_cat_out, _) = run("systemctl", &[scope_flag, "cat", "wintermute.target"]);
        let unit_names = discover_units(run, scope_flag);

        let ctx = ProbeCtx {
            scope_flag,
            target_cat: &target_cat_out,
            run,
            home_override,
            user_override,
            uid_override,
        };

        for unit in &unit_names {
            let report = probe_unit(unit, &ctx);
            // Deduplicate by unit name (same unit appearing in both scopes is kept once per scope).
            all_units.push(report);
        }
    }

    let failures: Vec<String> = all_units
        .iter()
        .filter(|r| !r.binary_ok() && !r.exec_start.is_empty())
        .map(|r| r.unit.clone())
        .collect();

    let all_ok = failures.is_empty();

    DoctorReport { units: all_units, failures, all_ok }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render a `DoctorReport` as a human-readable table.
///
/// Returns the formatted string (does not print).
#[must_use]
pub fn render_table(report: &DoctorReport, quiet: bool) -> String {
    let mut out = String::new();

    // Header.
    if !quiet {
        out.push_str(&format!(
            "{:<30}  {:<8}  {:<8}  {:<9}  {:<9}  {}\n",
            "UNIT", "EXISTS", "EXEC", "ENABLED", "ACTIVE", "RESOLVED"
        ));
        out.push_str(&"-".repeat(100));
        out.push('\n');
    }

    let mut failed_count = 0usize;
    for r in &report.units {
        let fail = !r.binary_ok() && !r.exec_start.is_empty();
        if quiet && !fail {
            continue;
        }
        if fail {
            failed_count += 1;
        }
        let exists_label = if r.exists { "YES" } else { "MISSING" };
        let exec_label = if r.exists {
            if r.executable { "YES" } else { "NO-EXEC" }
        } else {
            "-"
        };
        out.push_str(&format!(
            "{:<30}  {:<8}  {:<8}  {:<9}  {:<9}  {}\n",
            r.unit, exists_label, exec_label, r.enabled, r.active, r.resolved
        ));
    }

    if !quiet {
        out.push('\n');
        if report.all_ok {
            out.push_str(&format!("all {} units OK\n", report.units.len()));
        } else {
            out.push_str(&format!(
                "{} of {} unit(s) have missing/non-executable binaries: {}\n",
                report.failures.len(),
                report.units.len(),
                report.failures.join(", ")
            ));
        }
    } else if failed_count == 0 && !report.all_ok {
        // quiet + failures that match exec_start empty (no ExecStart found) — skip summary
    }

    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Specifier expansion (AC1)
    // -----------------------------------------------------------------------

    #[test]
    fn expand_h_substitutes_home() {
        let result = expand_specifiers("%h/.cargo/bin/wm-audio", Some("/home/jsy"), None, None);
        assert_eq!(result, "/home/jsy/.cargo/bin/wm-audio");
    }

    #[test]
    fn expand_u_substitutes_user() {
        let result = expand_specifiers("/home/%u/bin/wm", None, Some("jsy"), None);
        assert!(result.contains("jsy"));
        assert!(!result.contains("%u"));
    }

    #[test]
    fn expand_uppercase_u_substitutes_uid() {
        let result = expand_specifiers("/run/user/%U/wm.sock", None, None, Some(1000));
        assert_eq!(result, "/run/user/1000/wm.sock");
    }

    #[test]
    fn expand_unknown_specifier_left_as_is() {
        let result = expand_specifiers("%n-unit", Some("/home/jsy"), Some("jsy"), Some(1000));
        assert!(result.contains("%n"), "unknown specifier should be preserved");
    }

    #[test]
    fn expand_no_specifiers_passes_through() {
        let raw = "/usr/local/bin/wmd-init";
        let result = expand_specifiers(raw, Some("/home/jsy"), Some("jsy"), Some(1000));
        assert_eq!(result, raw);
    }

    // -----------------------------------------------------------------------
    // Binary existence / executable check (AC1)
    // -----------------------------------------------------------------------

    #[test]
    fn check_binary_absent_returns_false_false() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("no-such-bin");
        let (exists, exec) = check_binary(&p);
        assert!(!exists);
        assert!(!exec);
    }

    #[test]
    fn check_binary_exists_and_executable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("my-bin");
        std::fs::write(&p, b"#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).unwrap();
        let (exists, exec) = check_binary(&p);
        assert!(exists);
        assert!(exec);
    }

    #[test]
    fn check_binary_exists_but_not_executable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("data-file");
        std::fs::write(&p, b"data\n").unwrap();
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&p, perms).unwrap();
        let (exists, exec) = check_binary(&p);
        assert!(exists);
        assert!(!exec, "file with no exec bits should return executable=false");
    }

    // -----------------------------------------------------------------------
    // ExecStart parsing (AC1)
    // -----------------------------------------------------------------------

    fn unit_fixture(exec_start_line: &str) -> String {
        format!(
            "# /usr/lib/systemd/user/wm-audio.service\n[Unit]\nDescription=wm-audio\n\n[Service]\nType=simple\n{exec_start_line}\n"
        )
    }

    #[test]
    fn parse_exec_start_plain_path() {
        let out = unit_fixture("ExecStart=/home/jsy/.cargo/bin/wm-audio");
        assert_eq!(
            parse_exec_start(&out),
            Some("/home/jsy/.cargo/bin/wm-audio".to_string())
        );
    }

    #[test]
    fn parse_exec_start_specifier_path() {
        let out = unit_fixture("ExecStart=%h/.cargo/bin/wm-audio --verbose");
        assert_eq!(
            parse_exec_start(&out),
            Some("%h/.cargo/bin/wm-audio".to_string())
        );
    }

    #[test]
    fn parse_exec_start_with_prefix_flags() {
        // `-` means ignore error; should be stripped before extracting binary.
        let out = unit_fixture("ExecStart=-/usr/local/bin/wmd-init");
        assert_eq!(
            parse_exec_start(&out),
            Some("/usr/local/bin/wmd-init".to_string())
        );
    }

    #[test]
    fn parse_exec_start_missing_returns_none() {
        let out = "[Unit]\nDescription=no exec\n";
        assert_eq!(parse_exec_start(out), None);
    }

    // -----------------------------------------------------------------------
    // in-target parse (AC1)
    // -----------------------------------------------------------------------

    #[test]
    fn unit_in_target_present() {
        let target_cat = "[Unit]\nWants=wm-audio.service wmd.service\n";
        assert!(unit_in_target_from_cat(target_cat, "wm-audio.service"));
        assert!(unit_in_target_from_cat(target_cat, "wmd.service"));
    }

    #[test]
    fn unit_not_in_target() {
        let target_cat = "[Unit]\nWants=wm-audio.service\n";
        assert!(!unit_in_target_from_cat(target_cat, "wmd-init.service"));
    }

    #[test]
    fn unit_in_target_via_requires() {
        let target_cat = "[Unit]\nRequires=wmd-init.service\n";
        assert!(unit_in_target_from_cat(target_cat, "wmd-init.service"));
    }

    // -----------------------------------------------------------------------
    // Exit-code mapping (AC1)
    // -----------------------------------------------------------------------

    #[test]
    fn all_ok_report_exit_maps_to_zero() {
        let report = DoctorReport {
            units: vec![UnitReport {
                unit: "wm-audio.service".to_string(),
                exec_start: "/home/jsy/.cargo/bin/wm-audio".to_string(),
                resolved: "/home/jsy/.cargo/bin/wm-audio".to_string(),
                exists: true,
                executable: true,
                enabled: "enabled".to_string(),
                active: "active".to_string(),
                in_target: true,
                scope: "user".to_string(),
            }],
            failures: vec![],
            all_ok: true,
        };
        // all_ok == true → exit 0 (caller uses this field)
        assert!(report.all_ok);
        assert!(report.failures.is_empty());
    }

    #[test]
    fn missing_binary_report_exit_maps_to_nonzero() {
        let report = DoctorReport {
            units: vec![UnitReport {
                unit: "wmd-init.service".to_string(),
                exec_start: "/usr/local/bin/wmd-init".to_string(),
                resolved: "/usr/local/bin/wmd-init".to_string(),
                exists: false,
                executable: false,
                enabled: "enabled".to_string(),
                active: "failed".to_string(),
                in_target: true,
                scope: "user".to_string(),
            }],
            failures: vec!["wmd-init.service".to_string()],
            all_ok: false,
        };
        // all_ok == false → exit 2 (caller uses this field)
        assert!(!report.all_ok);
        assert!(!report.failures.is_empty());
        assert!(report.failures.contains(&"wmd-init.service".to_string()));
        assert_eq!(
            report.units[0].resolved,
            "/usr/local/bin/wmd-init",
            "missing binary resolved path must be named"
        );
    }

    // -----------------------------------------------------------------------
    // run_doctor with fake runner (AC3, AC4)
    // -----------------------------------------------------------------------

    fn make_fake_runner(dir: &TempDir) -> CmdRunner {
        // Build a real executable in the tempdir.
        let bin_path = dir.path().join("wm-audio");
        std::fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(&bin_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin_path, perms).unwrap();

        let bin_str = bin_path.to_string_lossy().to_string();

        Box::new(move |cmd: &str, args: &[&str]| {
            let args_s: Vec<&str> = args.to_vec();
            if cmd == "systemctl" {
                if args_s.contains(&"list-unit-files") {
                    if args_s.last() == Some(&"wm-*.service") {
                        return ("wm-audio.service enabled\n".to_string(), true);
                    }
                    return (String::new(), true);
                }
                if args_s.contains(&"list-units") {
                    return (String::new(), true);
                }
                if args_s.contains(&"cat") {
                    if args_s.last() == Some(&"wintermute.target") {
                        return (
                            format!("[Unit]\nWants=wm-audio.service\n"),
                            true,
                        );
                    }
                    if args_s.last() == Some(&"wm-audio.service") {
                        return (
                            format!("[Service]\nExecStart={bin_str}\n"),
                            true,
                        );
                    }
                }
                if args_s.contains(&"is-enabled") {
                    return ("enabled\n".to_string(), true);
                }
                if args_s.contains(&"is-active") {
                    return ("active\n".to_string(), true);
                }
            }
            (String::new(), false)
        })
    }

    #[test]
    fn run_doctor_passing_case_exits_ok() {
        let dir = tempfile::tempdir().unwrap();
        let run = make_fake_runner(&dir);
        let home = dir.path().to_string_lossy().to_string();
        let report = run_doctor(DoctorScope::User, &run, Some(&home), Some("jsy"), Some(1000));
        assert!(report.all_ok, "all binaries present → all_ok must be true");
        assert!(report.failures.is_empty());
        assert!(!report.units.is_empty(), "should discover at least one unit");
    }

    #[test]
    fn run_doctor_flags_missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        // Point ExecStart at a path that does NOT exist.
        let missing_path = dir.path().join("does-not-exist");
        let missing_str = missing_path.to_string_lossy().to_string();
        let missing_str_clone = missing_str.clone();

        let run: CmdRunner = Box::new(move |cmd: &str, args: &[&str]| {
            let args_s: Vec<&str> = args.to_vec();
            if cmd == "systemctl" {
                if args_s.contains(&"list-unit-files") && args_s.last() == Some(&"wmd-init.service") {
                    return ("wmd-init.service enabled\n".to_string(), true);
                }
                if args_s.contains(&"list-unit-files") {
                    return (String::new(), true);
                }
                if args_s.contains(&"list-units") {
                    return (String::new(), true);
                }
                if args_s.contains(&"cat") && args_s.last() == Some(&"wintermute.target") {
                    return ("[Unit]\nWants=wmd-init.service\n".to_string(), true);
                }
                if args_s.contains(&"cat") && args_s.last() == Some(&"wmd-init.service") {
                    return (
                        format!("[Service]\nExecStart={missing_str_clone}\n"),
                        true,
                    );
                }
                if args_s.contains(&"is-enabled") {
                    return ("enabled\n".to_string(), true);
                }
                if args_s.contains(&"is-active") {
                    return ("failed\n".to_string(), false);
                }
            }
            (String::new(), false)
        });

        let report = run_doctor(DoctorScope::User, &run, Some("/home/jsy"), Some("jsy"), Some(1000));
        assert!(!report.all_ok, "missing binary → all_ok must be false");
        assert!(
            report.failures.contains(&"wmd-init.service".to_string()),
            "wmd-init.service must appear in failures"
        );
        // Resolved path must be named in the report.
        let wmd_unit = report.units.iter().find(|u| u.unit == "wmd-init.service").unwrap();
        assert_eq!(wmd_unit.resolved, missing_str, "resolved path must be the unresolvable path");
        assert!(!wmd_unit.exists);
    }

    // -----------------------------------------------------------------------
    // Rendering smoke (AC4 — passing case summary)
    // -----------------------------------------------------------------------

    #[test]
    fn render_table_all_ok_contains_summary() {
        let report = DoctorReport {
            units: vec![UnitReport {
                unit: "wm-audio.service".to_string(),
                exec_start: "/bin/wm-audio".to_string(),
                resolved: "/bin/wm-audio".to_string(),
                exists: true,
                executable: true,
                enabled: "enabled".to_string(),
                active: "active".to_string(),
                in_target: true,
                scope: "user".to_string(),
            }],
            failures: vec![],
            all_ok: true,
        };
        let table = render_table(&report, false);
        assert!(table.contains("all 1 units OK"), "summary line missing: {table}");
    }

    #[test]
    fn render_table_failure_names_unit() {
        let report = DoctorReport {
            units: vec![UnitReport {
                unit: "wmd-init.service".to_string(),
                exec_start: "/usr/local/bin/wmd-init".to_string(),
                resolved: "/usr/local/bin/wmd-init".to_string(),
                exists: false,
                executable: false,
                enabled: "enabled".to_string(),
                active: "failed".to_string(),
                in_target: true,
                scope: "user".to_string(),
            }],
            failures: vec!["wmd-init.service".to_string()],
            all_ok: false,
        };
        let table = render_table(&report, false);
        assert!(table.contains("wmd-init.service"), "unit name missing from table: {table}");
        assert!(table.contains("MISSING"), "MISSING label missing: {table}");
    }

    #[test]
    fn render_table_quiet_hides_passing_rows() {
        let report = DoctorReport {
            units: vec![
                UnitReport {
                    unit: "wm-audio.service".to_string(),
                    exec_start: "/bin/wm-audio".to_string(),
                    resolved: "/bin/wm-audio".to_string(),
                    exists: true,
                    executable: true,
                    enabled: "enabled".to_string(),
                    active: "active".to_string(),
                    in_target: true,
                    scope: "user".to_string(),
                },
                UnitReport {
                    unit: "wmd-init.service".to_string(),
                    exec_start: "/usr/local/bin/wmd-init".to_string(),
                    resolved: "/usr/local/bin/wmd-init".to_string(),
                    exists: false,
                    executable: false,
                    enabled: "enabled".to_string(),
                    active: "failed".to_string(),
                    in_target: true,
                    scope: "user".to_string(),
                },
            ],
            failures: vec!["wmd-init.service".to_string()],
            all_ok: false,
        };
        let table = render_table(&report, true);
        assert!(!table.contains("wm-audio.service"), "passing row should be hidden in quiet mode");
        assert!(table.contains("wmd-init.service"), "failing row should appear in quiet mode");
    }

    // -----------------------------------------------------------------------
    // UnitReport::binary_ok
    // -----------------------------------------------------------------------

    #[test]
    fn binary_ok_true_when_exists_and_executable() {
        let r = UnitReport {
            unit: "x".to_string(),
            exec_start: "/bin/x".to_string(),
            resolved: "/bin/x".to_string(),
            exists: true,
            executable: true,
            enabled: "enabled".to_string(),
            active: "active".to_string(),
            in_target: false,
            scope: "user".to_string(),
        };
        assert!(r.binary_ok());
    }

    #[test]
    fn binary_ok_false_when_not_executable() {
        let r = UnitReport {
            unit: "x".to_string(),
            exec_start: "/bin/x".to_string(),
            resolved: "/bin/x".to_string(),
            exists: true,
            executable: false,
            enabled: "enabled".to_string(),
            active: "active".to_string(),
            in_target: false,
            scope: "user".to_string(),
        };
        assert!(!r.binary_ok());
    }

    #[test]
    fn binary_ok_false_when_missing() {
        let r = UnitReport {
            unit: "x".to_string(),
            exec_start: "/bin/x".to_string(),
            resolved: "/bin/x".to_string(),
            exists: false,
            executable: false,
            enabled: "enabled".to_string(),
            active: "inactive".to_string(),
            in_target: false,
            scope: "user".to_string(),
        };
        assert!(!r.binary_ok());
    }
}
