//! `strand-auth`: the lock screen's PAM helper.
//!
//! Started by [`strand_auth::Client`] with a socketpair as stdin and
//! stdout, a scrubbed environment and no other open descriptors. It says
//! hello (the protocol version and the PAM service it uses), then answers
//! each password with one verdict, until the client closes the socket.
//! It authenticates the user it runs as; the client never names one.
//!
//! The service is `strand`, or `login` when no `strand` service file
//! exists (decisions.md, m4-owner); the client turns the `login` hello
//! into a one-time warning. Every PAM error fails closed (`pam.rs`).

mod pam;

use std::ffi::{CStr, CString};
use std::fs::File;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use strand_auth::protocol::{self, Code, Message, ProtocolError, Service};

/// The admin's PAM service directory, which every Linux-PAM reads.
const ETC_PAM_D: &str = "/etc/pam.d";
/// The distribution's vendor directory, which Linux-PAM reads only when
/// built with `--enable-vendordir` (openSUSE, not Debian, Fedora or
/// Arch).
const VENDOR_PAM_D: &str = "/usr/lib/pam.d";

fn main() -> ExitCode {
    harden();
    let (Ok(input), Ok(output)) = (
        std::io::stdin().as_fd().try_clone_to_owned(),
        std::io::stdout().as_fd().try_clone_to_owned(),
    ) else {
        eprintln!("strand-auth: no socket on stdin and stdout");
        return ExitCode::from(2);
    };
    // Unbuffered: a buffered reader would keep a copy of the password
    // that nothing wipes.
    let mut input = File::from(input);
    let mut output = File::from(output);

    let confdir = test_confdir();
    // The `login` fallback is said in the hello only: the client turns
    // it into the one warning per process (design.md, "one-time
    // warning"). A line here would repeat on every helper start, since
    // the helper's stderr is strand's.
    let service = choose_service(confdir.as_deref());
    let user = current_user();
    if protocol::write_message(
        &mut output,
        &Message::Hello {
            version: protocol::VERSION,
            service,
        },
    )
    .is_err()
    {
        return ExitCode::from(2);
    }
    let confdir_c = confdir
        .as_ref()
        .and_then(|d| CString::new(d.as_os_str().as_encoded_bytes()).ok());
    loop {
        match protocol::read_message(&mut input) {
            Ok(Message::Submit(password)) => {
                fault_before_check(&mut output);
                let (code, message) = match &user {
                    Some(user) => pam::check(
                        service.name(),
                        user,
                        password.as_bytes(),
                        confdir_c.as_deref(),
                    ),
                    None => (
                        Code::Error,
                        "the helper's user has no passwd entry".to_string(),
                    ),
                };
                drop(password);
                if protocol::write_message(&mut output, &Message::Verdict { code, message })
                    .is_err()
                {
                    return ExitCode::from(2);
                }
            }
            Ok(_) => {
                eprintln!("strand-auth: the client sent something other than a password");
                return ExitCode::from(2);
            }
            Err(ProtocolError::Eof) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("strand-auth: {e}");
                return ExitCode::from(2);
            }
        }
    }
}

/// No core dumps and no ptrace by other processes of the same user: a
/// password passes through this process's memory.
fn harden() {
    // SAFETY: prctl(PR_SET_DUMPABLE, 0) takes no pointers.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
}

/// `strand` when libpam will read a `strand` service file, else `login`
/// (or, with a test confdir, whether it holds a readable `strand`).
fn choose_service(confdir: Option<&Path>) -> Service {
    let found = match confdir {
        Some(dir) => readable(&dir.join("strand")),
        None => strand_service_read(Path::new(ETC_PAM_D), Path::new(VENDOR_PAM_D)),
    };
    if found {
        Service::Strand
    } else {
        Service::Login
    }
}

/// Whether libpam will read a `strand` service file: one in `etc` this
/// process can read, or one in `vendor` when this libpam reads the vendor
/// directory. A file libpam never opens (an unreadable one, or one in a
/// vendor directory this libpam ignores) would send `pam_start` to the
/// `other` service, usually `pam_deny`: every password refused as wrong,
/// with no fallback and no warning (decisions.md, m4-audit). Whether the
/// vendor directory is read cannot be asked of libpam; it is inferred
/// from the system's own `login` service, which the fallback relies on:
/// when it lives only in `vendor`, libpam must read `vendor`. Where
/// `login` is in both or neither, `vendor` is not trusted and `login` is
/// used, which works either way.
fn strand_service_read(etc: &Path, vendor: &Path) -> bool {
    readable(&etc.join("strand"))
        || (readable(&vendor.join("strand"))
            && !etc.join("login").exists()
            && readable(&vendor.join("login")))
}

/// The file opens for reading (`exists()` is not enough: libpam opens
/// it as this user).
fn readable(path: &Path) -> bool {
    File::open(path).is_ok_and(|f| f.metadata().is_ok_and(|m| m.is_file()))
}

/// The user this process runs as, from the passwd database.
fn current_user() -> Option<CString> {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let mut buf = vec![0u8; 16 * 1024];
    // SAFETY: zeroed is a valid passwd (pointers null).
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: valid out-pointers and a buffer of the stated length.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut pwd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || pwd.pw_name.is_null() {
        return None;
    }
    // SAFETY: pw_name points into `buf`, NUL-terminated.
    Some(unsafe { CStr::from_ptr(pwd.pw_name) }.to_owned())
}

/// (`faults`) The private PAM config dir tests point the helper at.
#[cfg(feature = "faults")]
fn test_confdir() -> Option<PathBuf> {
    std::env::var_os("STRAND_AUTH_PAM_CONFDIR").map(PathBuf::from)
}

#[cfg(not(feature = "faults"))]
fn test_confdir() -> Option<PathBuf> {
    None
}

/// (`faults`) `STRAND_FAULT` (a comma-separated list) names `fault`.
#[cfg(feature = "faults")]
fn fault(fault: &str) -> bool {
    std::env::var("STRAND_FAULT").is_ok_and(|v| v.split(',').any(|f| f.trim() == fault))
}

/// (`faults`) The injection points before a check: the helper crashes
/// (`auth_crash`), hangs (`auth_hang`) or answers garbage
/// (`auth_garbage`).
#[cfg(feature = "faults")]
fn fault_before_check(output: &mut File) {
    use std::io::Write;
    if fault("auth_crash") {
        eprintln!("strand-auth: STRAND_FAULT auth_crash");
        std::process::abort();
    }
    if fault("auth_hang") {
        eprintln!("strand-auth: STRAND_FAULT auth_hang");
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
    if fault("auth_garbage") {
        eprintln!("strand-auth: STRAND_FAULT auth_garbage");
        let _ = output.write_all(b"\xff\xff\xff\x7fSTRAND_FAULT garbage");
        let _ = output.flush();
    }
}

#[cfg(not(feature = "faults"))]
fn fault_before_check(_: &mut File) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let (etc, vendor) = (root.path().join("etc"), root.path().join("vendor"));
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::create_dir_all(&vendor).unwrap();
        (root, etc, vendor)
    }

    fn put(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), "auth required pam_unix.so\n").unwrap();
    }

    /// `/etc/pam.d/strand` is always read; one in the vendor directory
    /// counts only where the system's `login` lives there alone, which
    /// shows this libpam reads it. Otherwise `login`.
    #[test]
    fn a_vendor_service_counts_only_where_libpam_reads_the_vendor_dir() {
        let (_root, etc, vendor) = dirs();
        assert!(!strand_service_read(&etc, &vendor), "no file anywhere");
        put(&vendor, "strand");
        assert!(
            !strand_service_read(&etc, &vendor),
            "no sign the vendor dir is read"
        );
        put(&etc, "login");
        put(&vendor, "login");
        assert!(
            !strand_service_read(&etc, &vendor),
            "login in both: unknown"
        );
        std::fs::remove_file(etc.join("login")).unwrap();
        assert!(
            strand_service_read(&etc, &vendor),
            "login only in the vendor dir"
        );
        put(&etc, "strand");
        assert!(strand_service_read(&etc, &vendor));
    }

    /// A file that exists but cannot be opened is one libpam cannot read.
    #[test]
    fn an_unreadable_service_file_is_not_chosen() {
        use std::os::unix::fs::PermissionsExt;
        let (_root, etc, vendor) = dirs();
        put(&etc, "strand");
        let file = etc.join("strand");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads anything: the case cannot be made there.
        if File::open(&file).is_ok() {
            return;
        }
        assert!(!strand_service_read(&etc, &vendor));
        assert_eq!(choose_service(Some(&etc)), Service::Login);
        // A directory named `strand` is no service file either.
        let (_root2, etc2, vendor2) = dirs();
        std::fs::create_dir(etc2.join("strand")).unwrap();
        assert!(!strand_service_read(&etc2, &vendor2));
    }
}
