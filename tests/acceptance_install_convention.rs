//! Acceptance tests for the install-path convention
//! (PRD-wintermute-install-path-convention).
//!
//! These tests are purely structural / pure-function based so they run in
//! CI without root, hardware, or a live systemd instance.
//!
//! AC coverage:
//!   AC1 — `cargo test --lib` covers convention constant, idempotent-install
//!          logic, and unit-reconcile path-rewrite (unit tests in
//!          `src/install_convention.rs`).  This integration file provides
//!          additional cross-module assertions and end-to-end scenarios.
//!   AC2 — idempotent install: running `plan_install` twice on the same state
//!          returns identical `already_correct` flags.
//!   AC5 — single convention: all platform-owned unit files in
//!          `pkg/systemd/user/` are convention-compliant after the rewrite.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::Path;

use wintermute_platform::install_convention::{
    check_unit_convention, plan_install, rewrite_unit_exec_starts, FLEET_INSTALL_DIR,
    FLEET_INSTALL_SUBDIR, PLATFORM_BINARIES,
};

// ---------------------------------------------------------------------------
// AC5: Single convention — platform unit files are compliant after rewrite
// ---------------------------------------------------------------------------

/// Discover every *.service under pkg/systemd/user/ and assert that after
/// `rewrite_unit_exec_starts` each file is convention-compliant.
#[test]
fn ac5_platform_units_compliant_after_rewrite() {
    // Resolve repo root from CARGO_MANIFEST_DIR.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let unit_dir = Path::new(manifest_dir).join("pkg/systemd/user");

    assert!(
        unit_dir.exists(),
        "pkg/systemd/user/ not found at {unit_dir:?}"
    );

    let mut checked = 0_u32;
    for entry in std::fs::read_dir(&unit_dir)
        .expect("read pkg/systemd/user/")
        .flatten()
    {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.ends_with(".service") {
            continue;
        }
        let body = std::fs::read_to_string(entry.path())
            .unwrap_or_else(|_| panic!("read {name_str}"));

        // After applying the rewrite, the file must be convention-compliant.
        let rewritten = rewrite_unit_exec_starts(&body);
        let r = check_unit_convention(name_str.as_ref(), &rewritten);
        assert!(
            r.compliant,
            "unit {name_str} is still non-compliant after rewrite; offending: {:?}",
            r.offending_exec_start
        );
        checked += 1;
    }

    assert!(checked >= 1, "no .service files found under pkg/systemd/user/");
}

/// The canonical wmd-init.service in the repo must already be compliant
/// (source-of-truth — no rewrite required).
#[test]
fn ac5_wmd_init_service_already_compliant_in_repo() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let unit_path = Path::new(manifest_dir).join("pkg/systemd/user/wmd-init.service");

    assert!(unit_path.exists(), "wmd-init.service not found");

    let body = std::fs::read_to_string(&unit_path).expect("read wmd-init.service");
    let r = check_unit_convention("wmd-init.service", &body);
    assert!(
        r.compliant,
        "wmd-init.service in repo has non-compliant ExecStart: {:?}",
        r.offending_exec_start
    );
}

// ---------------------------------------------------------------------------
// AC2: Idempotent install — plan_install converges on second run
// ---------------------------------------------------------------------------

#[test]
fn ac2_plan_install_idempotent_on_absent_targets() {
    let release_dir = Path::new("/nowhere/target/release");
    let install_dir = Path::new("/nowhere/.local/bin");

    // First call: nothing installed yet.
    let ops1 = plan_install(release_dir, install_dir, PLATFORM_BINARIES);
    // Second call on same (nonexistent) state: identical result.
    let ops2 = plan_install(release_dir, install_dir, PLATFORM_BINARIES);

    assert_eq!(ops1.len(), ops2.len());
    for (o1, o2) in ops1.iter().zip(ops2.iter()) {
        assert_eq!(o1.src, o2.src);
        assert_eq!(o1.dst, o2.dst);
        assert_eq!(
            o1.already_correct, o2.already_correct,
            "idempotency violated for {}",
            o1.dst.display()
        );
    }
}

#[test]
fn ac2_plan_install_idempotent_on_present_targets() -> std::io::Result<()> {
    let tmp = tempfile::tempdir()?;
    let release_dir = tmp.path().join("target/release");
    let install_dir = tmp.path().join(".local/bin");
    std::fs::create_dir_all(&install_dir)?;

    // Pre-place the destination files.
    for &bin in PLATFORM_BINARIES {
        std::fs::write(install_dir.join(bin), b"fake-binary")?;
    }

    // Both calls must report already_correct=true.
    let ops1 = plan_install(&release_dir, &install_dir, PLATFORM_BINARIES);
    let ops2 = plan_install(&release_dir, &install_dir, PLATFORM_BINARIES);

    for (o1, o2) in ops1.iter().zip(ops2.iter()) {
        assert!(o1.already_correct, "{} should be already_correct (1st run)", o1.dst.display());
        assert!(o2.already_correct, "{} should be already_correct (2nd run)", o2.dst.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Convention constant cross-checks
// ---------------------------------------------------------------------------

#[test]
fn fleet_install_dir_constant_matches_subdir() {
    // FLEET_INSTALL_DIR = "~/.local/bin" must end with FLEET_INSTALL_SUBDIR.
    assert!(
        FLEET_INSTALL_DIR.ends_with(FLEET_INSTALL_SUBDIR),
        "FLEET_INSTALL_DIR ({FLEET_INSTALL_DIR:?}) does not end with FLEET_INSTALL_SUBDIR ({FLEET_INSTALL_SUBDIR:?})"
    );
}

// ---------------------------------------------------------------------------
// Rewrite idempotency round-trip on a realistic wmd-init unit body
// ---------------------------------------------------------------------------

#[test]
fn rewrite_unit_exec_starts_round_trip_wmd_init() {
    let original = "[Unit]\n\
                    Description=wintermute supervisor (wmd-init)\n\
                    After=pipewire.service\n\
                    \n\
                    [Service]\n\
                    Type=notify\n\
                    ExecStart=/usr/local/bin/wmd-init\n\
                    Restart=always\n\
                    \n\
                    [Install]\n\
                    WantedBy=wintermute.target\n";

    let pass1 = rewrite_unit_exec_starts(original);
    let pass2 = rewrite_unit_exec_starts(&pass1);

    // After the first rewrite, the line must be correct.
    assert!(
        pass1.contains("ExecStart=%h/.local/bin/wmd-init"),
        "rewrite did not produce expected ExecStart: {pass1}"
    );

    // Idempotent: second pass is identical to first.
    assert_eq!(pass1, pass2, "rewrite is not idempotent");
}
