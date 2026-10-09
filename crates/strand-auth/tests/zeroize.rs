//! Passwords are wiped: no heap block freed while a password travels
//! through the client and the protocol still holds it. A spying global
//! allocator looks at every block as it is freed (a reallocation frees
//! the old block too, so a copy left behind by growth is caught).
//!
//! One test in this binary, so no other test's allocations interleave.

use std::alloc::{GlobalAlloc, Layout, System};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use strand_auth::protocol::{Message, encode, read_message};
use strand_auth::{Client, Password};

/// Not a real password: a pattern nothing else in the process holds.
const MARKER: &[u8] = b"Zq8-marker-for-the-wipe-test-7731";

static ARMED: AtomicBool = AtomicBool::new(false);
static SEEN: AtomicUsize = AtomicUsize::new(0);

struct Spy;

fn holds_marker(block: &[u8]) -> bool {
    block.windows(MARKER.len()).any(|w| w == MARKER)
}

// SAFETY: forwards to the system allocator; `dealloc` only reads the
// block it is handed, before freeing it.
unsafe impl GlobalAlloc for Spy {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded as is.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ARMED.load(Ordering::Relaxed) {
            // SAFETY: the block is live and `layout.size()` bytes long.
            let block = unsafe { std::slice::from_raw_parts(ptr, layout.size()) };
            if holds_marker(block) {
                SEEN.fetch_add(1, Ordering::Relaxed);
            }
        }
        // SAFETY: forwarded as is.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Spy = Spy;

/// The password as a user's `String` would hold it, with room to spare
/// so it never grew.
fn password() -> Password {
    let mut s = String::with_capacity(64);
    s.push_str(std::str::from_utf8(MARKER).unwrap());
    Password::from(s)
}

#[test]
fn freed_memory_never_holds_a_password() {
    ARMED.store(true, Ordering::SeqCst);

    // Control: a plain buffer is freed with the marker in it.
    let plain = MARKER.to_vec();
    drop(plain);
    assert_eq!(
        SEEN.swap(0, Ordering::SeqCst),
        1,
        "the spy sees a plain buffer"
    );

    // The protocol both ways: the frame, the payload read back, the
    // password decoded from it.
    let frame = encode(&Message::Submit(password()));
    let m = read_message(&mut frame.as_slice()).unwrap();
    drop(frame);
    let Message::Submit(back) = m else {
        panic!("{m:?}");
    };
    assert_eq!(back.as_bytes(), MARKER);
    drop(back);
    assert_eq!(SEEN.load(Ordering::SeqCst), 0, "a protocol buffer kept it");

    // The client: a fake helper that reads the frame and refuses it.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fake-helper");
    std::fs::write(
        &path,
        "#!/bin/sh\nprintf '\\003\\000\\000\\000\\001\\001\\000'; head -c 38 >/dev/null; \
         printf '\\002\\000\\000\\000\\003\\001'; sleep 5\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    fn no_hook() {}
    let mut c = Client::new(path, no_hook).with_timeout(Duration::from_secs(5));
    let v = c.submit(password());
    assert!(!v.is_unlocked(), "{v:?}");
    drop(c);
    assert_eq!(SEEN.load(Ordering::SeqCst), 0, "the client kept it");
    ARMED.store(false, Ordering::SeqCst);
}
