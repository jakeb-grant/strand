//! A test build without `faults` leaves a helper with none of the test
//! hooks in cargo's output directory, where `cargo install` and a
//! package's `install` step take it from: `cargo test --release` before
//! packaging must not ship `STRAND_FAULT` or a private PAM confdir.
//! (With `--features faults`, asked for by name, `no_faults.rs` runs
//! instead.)

#![cfg(not(feature = "faults"))]

use std::path::Path;

#[test]
fn a_plain_test_build_leaves_a_helper_without_fault_code() {
    let helper = Path::new(env!("CARGO_BIN_EXE_strand-auth"));
    let bytes = std::fs::read(helper).unwrap();
    for needle in [
        &b"STRAND_FAULT"[..],
        b"STRAND_AUTH_PAM_CONFDIR",
        b"pam_start_confdir",
    ] {
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle),
            "{} holds {:?}",
            helper.display(),
            String::from_utf8_lossy(needle)
        );
    }
}
