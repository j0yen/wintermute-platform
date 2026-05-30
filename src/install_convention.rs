//! Install-path convention for the wintermute fleet.
//!
//! **One convention, always `~/.local/bin/`.**
//!
//! All user-scope fleet binaries and the systemd units that exec them must
//! agree on a single install directory.  This module records that directory
//! as [`FLEET_INSTALL_DIR`] and exposes pure functions the installer and
//! `wm doctor` both use so the rule is enforced from a single source of truth.
//!
//! # Fleet binaries owned / declared by wintermute-platform
//!
//! `wmd-init` and `wm` are compiled here; the companion binaries
//! (`wm-audio`, `wm-tts`, `wm-stt`, `wm-dialog`, `wmd`, `wm-bootstrap`)
//! live in sibling crates but are part of the same user-scope fleet and
//! subject to the same convention.
//!
//! # Path-rewrite
//!
//! [`rewrite_exec_start`] performs the idempotent unit-file line rewrite:
//! it replaces any absolute `ExecStart=<old-prefix>/<binary>` with
//! `ExecStart=~/.local/bin/<binary>`, leaving argument tails intact.  The
//! `%h` specifier is also accepted as `~` for the comparison.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Convention constant — the ONE install directory for the whole fleet.
// ---------------------------------------------------------------------------

/// Canonical install directory for all user-scope wintermute fleet binaries.
///
/// All `[Service] ExecStart=` lines in user-scope unit files must resolve to
/// this prefix (after `%h` / `~` expansion) after `install.sh` runs.
pub const FLEET_INSTALL_DIR: &str = "~/.local/bin";

/// Absolute prefix used on disk (relative to `$HOME`).
pub const FLEET_INSTALL_SUBDIR: &str = ".local/bin";

/// Binaries owned by wintermute-platform (compiled in this crate).
pub const PLATFORM_BINARIES: &[&str] = &["wmd-init", "wm"];

/// All user-scope fleet binaries (platform-owned + companion crates).
///
/// Used by `wm doctor` to assert every binary is present and by
/// `install.sh`'s doctor gate.
pub const FLEET_BINARIES: &[&str] =
    &["wmd-init", "wm", "wm-audio", "wm-tts", "wm-stt", "wm-dialog", "wmd", "wm-bootstrap"];

// ---------------------------------------------------------------------------
// Pure helpers — all operate on strings/paths, no side effects.
// ---------------------------------------------------------------------------

/// Return the absolute install dir given an explicit `$HOME` value.
#[must_use]
pub fn fleet_install_dir_with(home: &Path) -> PathBuf {
    home.join(FLEET_INSTALL_SUBDIR)
}

/// Return the install path for `binary` given an explicit `$HOME` value.
#[must_use]
pub fn binary_install_path(home: &Path, binary: &str) -> PathBuf {
    fleet_install_dir_with(home).join(binary)
}

// ---------------------------------------------------------------------------
// Unit-file ExecStart path-rewrite (AC5 / 2.2a)
// ---------------------------------------------------------------------------

/// Rewrite one `ExecStart=` line so the binary resolves under
/// `~/.local/bin/`.
///
/// Accepted existing prefixes (any of these is replaced):
/// - `/usr/local/bin/`
/// - `~/.cargo/bin/`
/// - `%h/.cargo/bin/`
/// - `%h/.local/bin/`
/// - `/home/<any-name>/.local/bin/`
/// - `/home/<any-name>/.cargo/bin/`
///
/// Lines that do **not** start with `ExecStart=` are returned unchanged.
///
/// The rewritten form uses `%h/.local/bin/` (systemd's `%h` specifier
/// expands to the unit's user's home directory at runtime), matching the
/// convention the sibling units already declare.  The canonical constant
/// [`FLEET_INSTALL_DIR`] uses `~` for human display; unit files use `%h`
/// for portability.
///
/// # Examples
///
/// ```
/// use wintermute_platform::install_convention::rewrite_exec_start;
///
/// assert_eq!(
///     rewrite_exec_start("ExecStart=/usr/local/bin/wmd-init"),
///     "ExecStart=%h/.local/bin/wmd-init"
/// );
/// assert_eq!(
///     rewrite_exec_start("ExecStart=%h/.cargo/bin/wm-audio start"),
///     "ExecStart=%h/.local/bin/wm-audio start"
/// );
/// assert_eq!(
///     rewrite_exec_start("Description=something"),
///     "Description=something"
/// );
/// ```
#[must_use]
pub fn rewrite_exec_start(line: &str) -> String {
    let Some(rest) = line.strip_prefix("ExecStart=") else {
        return line.to_string();
    };

    // Split on the first space to separate the executable from arguments.
    let (exec_path, args_tail) = rest.split_once(' ').map_or((rest, ""), |(e, a)| (e, a));

    // Attempt to extract just the binary name from any known prefix.
    let binary_name = extract_binary_name(exec_path);

    binary_name.map_or_else(
        || line.to_string(),
        |bin| {
            let new_exec = format!("%h/.local/bin/{bin}");
            if args_tail.is_empty() {
                format!("ExecStart={new_exec}")
            } else {
                format!("ExecStart={new_exec} {args_tail}")
            }
        },
    )
}

/// Attempt to extract the binary name from an `ExecStart` path.
///
/// Returns `Some(binary_name)` when the path lives under one of the known
/// prefixes, `None` otherwise.
fn extract_binary_name(path: &str) -> Option<&str> {
    // Known prefixes (in order of specificity).
    const PREFIXES: &[&str] = &[
        "/usr/local/bin/",
        "%h/.local/bin/",
        "%h/.cargo/bin/",
        "~/.local/bin/",
        "~/.cargo/bin/",
    ];

    for prefix in PREFIXES {
        if let Some(rest) = path.strip_prefix(prefix) {
            // Rest should be just the binary name (no extra '/').
            if !rest.is_empty() && !rest.contains('/') {
                return Some(rest);
            }
        }
    }

    // Also handle absolute /home/<user>/{.local,.cargo}/bin/<binary>
    if let Some(bin) = extract_from_home_prefix(path) {
        return Some(bin);
    }

    None
}

/// Extract a binary name from `/home/<user>/.{local,cargo}/bin/<binary>`.
fn extract_from_home_prefix(path: &str) -> Option<&str> {
    if !path.starts_with("/home/") {
        return None;
    }
    // Strip /home/<user>/
    let after_home = path.strip_prefix("/home/")?;
    let slash = after_home.find('/')?;
    let after_user = &after_home[slash + 1..];

    // Must be .local/bin/<bin> or .cargo/bin/<bin>
    let rest = after_user
        .strip_prefix(".local/bin/")
        .or_else(|| after_user.strip_prefix(".cargo/bin/"))?;

    if rest.is_empty() || rest.contains('/') {
        None
    } else {
        Some(rest)
    }
}

/// Rewrite all `ExecStart=` lines in a complete unit-file body.
///
/// Non-`ExecStart` lines are passed through unchanged.  Idempotent: calling
/// twice on an already-correct file returns the same string.
#[must_use]
pub fn rewrite_unit_exec_starts(unit_body: &str) -> String {
    unit_body
        .lines()
        .map(rewrite_exec_start)
        .collect::<Vec<_>>()
        .join("\n")
        + if unit_body.ends_with('\n') { "\n" } else { "" }
}

// ---------------------------------------------------------------------------
// Convention check — pure, used by `wm doctor`
// ---------------------------------------------------------------------------

/// Result of checking one unit file for convention compliance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitConventionCheck {
    /// Unit file name (basename only).
    pub unit_name: String,
    /// `true` iff all `ExecStart=` lines already use `%h/.local/bin/`.
    pub compliant: bool,
    /// The offending `ExecStart=` value, if any (first non-compliant line).
    pub offending_exec_start: Option<String>,
}

/// Check whether a unit-file body's `ExecStart` lines are convention-compliant.
///
/// A line is compliant when it starts with `ExecStart=%h/.local/bin/` or
/// does not start with `ExecStart=` (non-binary `ExecStart` like `/bin/sh -c`
/// are left alone — they are not fleet binaries).
#[must_use]
pub fn check_unit_convention(unit_name: &str, unit_body: &str) -> UnitConventionCheck {
    let offending = unit_body.lines().find_map(|line| {
        let rest = line.strip_prefix("ExecStart=")?;
        // /bin/sh -c and similar are not fleet binaries — ignore.
        if rest.starts_with("/bin/") || rest.starts_with("/usr/bin/") {
            return None;
        }
        if rest.starts_with("%h/.local/bin/") {
            None
        } else {
            Some(rest.to_string())
        }
    });

    UnitConventionCheck {
        unit_name: unit_name.to_string(),
        compliant: offending.is_none(),
        offending_exec_start: offending,
    }
}

// ---------------------------------------------------------------------------
// Idempotent-install helpers (pure — caller performs the actual fs writes)
// ---------------------------------------------------------------------------

/// Describes a single file-copy/symlink operation the installer needs to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOp {
    /// Source path (built binary in `target/release/`).
    pub src: PathBuf,
    /// Destination path under the fleet install dir.
    pub dst: PathBuf,
    /// `true` if `dst` already exists with the correct content (skip).
    pub already_correct: bool,
}

/// Compute the set of install operations needed to place `binaries` (each
/// compiled to `release_dir/<binary>`) into `install_dir`.
///
/// An operation is marked `already_correct` when `dst` already exists.
/// The caller checks content/checksum if it wants a stricter idempotency
/// guarantee; this function only checks presence.
#[must_use]
pub fn plan_install(
    release_dir: &Path,
    install_dir: &Path,
    binaries: &[&str],
) -> Vec<InstallOp> {
    binaries
        .iter()
        .map(|&bin| {
            let src = release_dir.join(bin);
            let dst = install_dir.join(bin);
            let already_correct = dst.exists();
            InstallOp { src, dst, already_correct }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // AC1 coverage: convention constant

    #[test]
    fn fleet_install_dir_constant_is_local_bin() {
        assert_eq!(FLEET_INSTALL_DIR, "~/.local/bin");
    }

    #[test]
    fn fleet_install_subdir_constant_is_correct() {
        assert_eq!(FLEET_INSTALL_SUBDIR, ".local/bin");
    }

    #[test]
    fn fleet_install_dir_with_builds_correct_path() {
        let home = Path::new("/home/jsy");
        assert_eq!(fleet_install_dir_with(home), PathBuf::from("/home/jsy/.local/bin"));
    }

    #[test]
    fn binary_install_path_appends_binary_name() {
        let home = Path::new("/home/jsy");
        assert_eq!(
            binary_install_path(home, "wmd-init"),
            PathBuf::from("/home/jsy/.local/bin/wmd-init")
        );
    }

    #[test]
    fn platform_binaries_includes_wmd_init_and_wm() {
        assert!(PLATFORM_BINARIES.contains(&"wmd-init"));
        assert!(PLATFORM_BINARIES.contains(&"wm"));
    }

    #[test]
    fn fleet_binaries_includes_all_six_companions() {
        for bin in &["wm-audio", "wm-tts", "wm-stt", "wm-dialog", "wmd", "wm-bootstrap"] {
            assert!(
                FLEET_BINARIES.contains(bin),
                "FLEET_BINARIES missing {bin}"
            );
        }
    }

    // AC1 coverage: unit-reconcile path-rewrite

    #[test]
    fn rewrite_exec_start_usr_local_bin() {
        assert_eq!(
            rewrite_exec_start("ExecStart=/usr/local/bin/wmd-init"),
            "ExecStart=%h/.local/bin/wmd-init"
        );
    }

    #[test]
    fn rewrite_exec_start_cargo_bin() {
        assert_eq!(
            rewrite_exec_start("ExecStart=%h/.cargo/bin/wm-audio start"),
            "ExecStart=%h/.local/bin/wm-audio start"
        );
    }

    #[test]
    fn rewrite_exec_start_tilde_cargo_bin() {
        assert_eq!(
            rewrite_exec_start("ExecStart=~/.cargo/bin/wm-audio start"),
            "ExecStart=%h/.local/bin/wm-audio start"
        );
    }

    #[test]
    fn rewrite_exec_start_already_correct_is_idempotent() {
        let line = "ExecStart=%h/.local/bin/wmd-init";
        assert_eq!(rewrite_exec_start(line), line);
    }

    #[test]
    fn rewrite_exec_start_non_exec_start_unchanged() {
        let line = "Description=wmd supervisor";
        assert_eq!(rewrite_exec_start(line), line);
    }

    #[test]
    fn rewrite_exec_start_bin_sh_unchanged() {
        let line = "ExecStart=/bin/sh -c 'if wm ready; then wm say hi; fi'";
        assert_eq!(rewrite_exec_start(line), line);
    }

    #[test]
    fn rewrite_exec_start_home_abs_path() {
        assert_eq!(
            rewrite_exec_start("ExecStart=/home/jsy/.local/bin/wm-tts start"),
            "ExecStart=%h/.local/bin/wm-tts start"
        );
    }

    #[test]
    fn rewrite_exec_start_home_cargo_abs_path() {
        assert_eq!(
            rewrite_exec_start("ExecStart=/home/jsy/.cargo/bin/wm-audio start"),
            "ExecStart=%h/.local/bin/wm-audio start"
        );
    }

    #[test]
    fn rewrite_unit_exec_starts_rewrites_all_exec_start_lines() {
        let input = "[Unit]\n\
                     Description=wmd\n\
                     \n\
                     [Service]\n\
                     ExecStart=/usr/local/bin/wmd-init\n\
                     Restart=always\n";
        let output = rewrite_unit_exec_starts(input);
        assert!(
            output.contains("ExecStart=%h/.local/bin/wmd-init"),
            "expected rewrite in: {output}"
        );
        assert!(output.contains("[Unit]"));
        assert!(output.contains("Restart=always"));
    }

    #[test]
    fn rewrite_unit_exec_starts_idempotent_when_already_correct() {
        let input = "[Service]\nExecStart=%h/.local/bin/wmd-init\nRestart=always\n";
        assert_eq!(rewrite_unit_exec_starts(input), input);
    }

    #[test]
    fn rewrite_unit_exec_starts_preserves_trailing_newline() {
        let input = "ExecStart=/usr/local/bin/wmd-init\n";
        let out = rewrite_unit_exec_starts(input);
        assert!(out.ends_with('\n'), "trailing newline lost");
    }

    #[test]
    fn rewrite_unit_exec_starts_no_trailing_newline_preserved() {
        let input = "ExecStart=/usr/local/bin/wmd-init";
        let out = rewrite_unit_exec_starts(input);
        assert!(!out.ends_with('\n'), "unexpected trailing newline added");
    }

    // AC1 coverage: idempotent-install logic

    #[test]
    fn plan_install_marks_missing_dst_as_not_yet_correct() {
        let release_dir = Path::new("/nowhere/target/release");
        let install_dir = Path::new("/nowhere/.local/bin");
        let ops = plan_install(release_dir, install_dir, &["wmd-init"]);
        assert_eq!(ops.len(), 1);
        let op = ops.first().expect("expected one op");
        assert_eq!(op.src, PathBuf::from("/nowhere/target/release/wmd-init"));
        assert_eq!(op.dst, PathBuf::from("/nowhere/.local/bin/wmd-init"));
        assert!(!op.already_correct, "dst should not exist");
    }

    #[test]
    fn plan_install_marks_present_dst_as_already_correct() -> std::io::Result<()> {
        let tmp = tempdir()?;
        let release_dir = tmp.path().join("target/release");
        let install_dir = tmp.path().join(".local/bin");
        std::fs::create_dir_all(&install_dir)?;
        // Pre-create the destination file.
        std::fs::write(install_dir.join("wmd-init"), b"dummy")?;

        let ops = plan_install(&release_dir, &install_dir, &["wmd-init"]);
        assert_eq!(ops.len(), 1);
        let op = ops.first().expect("expected one op");
        assert!(op.already_correct, "dst exists, should be marked correct");
        Ok(())
    }

    #[test]
    fn plan_install_empty_binaries_returns_empty_ops() {
        let ops = plan_install(Path::new("/a"), Path::new("/b"), &[]);
        assert!(ops.is_empty());
    }

    // AC5 coverage: single-convention checker

    #[test]
    fn check_unit_convention_compliant_local_bin() {
        let body = "[Service]\nExecStart=%h/.local/bin/wmd-init\n";
        let r = check_unit_convention("wmd-init.service", body);
        assert!(r.compliant);
        assert!(r.offending_exec_start.is_none());
    }

    #[test]
    fn check_unit_convention_non_compliant_usr_local_bin() {
        let body = "[Service]\nExecStart=/usr/local/bin/wmd-init\n";
        let r = check_unit_convention("wmd-init.service", body);
        assert!(!r.compliant);
        assert_eq!(r.offending_exec_start.as_deref(), Some("/usr/local/bin/wmd-init"));
    }

    #[test]
    fn check_unit_convention_non_compliant_cargo_bin() {
        let body = "[Service]\nExecStart=%h/.cargo/bin/wm-audio start\n";
        let r = check_unit_convention("wm-audio.service", body);
        assert!(!r.compliant);
    }

    #[test]
    fn check_unit_convention_bin_sh_ignored() {
        let body = "[Service]\nExecStart=/bin/sh -c 'if wm ready; then wm say hi; fi'\n";
        let r = check_unit_convention("wintermute-ready.service", body);
        assert!(r.compliant, "bin/sh ExecStart should not be flagged");
    }

    #[test]
    fn check_unit_convention_no_exec_start_compliant() {
        let body = "[Unit]\nDescription=no exec\n";
        let r = check_unit_convention("other.service", body);
        assert!(r.compliant);
    }
}
