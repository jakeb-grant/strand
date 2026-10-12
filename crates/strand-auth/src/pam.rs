//! The helper's PAM FFI, written by hand (Linux-PAM's `pam_appl.h`):
//! the only code in the workspace that links libpam.
//!
//! One conversation is `pam_start` (with the user the helper runs as and
//! a conversation callback that talks to the client), `pam_authenticate`,
//! `pam_acct_mgmt`, `pam_end`. The callback sends each `PAM_TEXT_INFO`
//! and `PAM_ERROR_MSG` to the client as an `INFO` frame as PAM says it,
//! and each `PAM_PROMPT_ECHO_OFF` as a `PROMPT` frame, then waits for the
//! client's `SUBMIT` and answers with that password (decisions.md,
//! m6-lock). Only `PAM_SUCCESS` from both calls is [`Code::Success`];
//! PAM's refusals are [`Code::Denied`] and everything else
//! [`Code::Error`], both of which leave the session locked.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs::File;
use std::ptr;

use strand_auth::protocol::{self, Code, MAX_PASSWORD, Message};
use zeroize::Zeroize;

const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_PERM_DENIED: c_int = 6;
const PAM_AUTH_ERR: c_int = 7;
const PAM_CRED_INSUFFICIENT: c_int = 8;
const PAM_USER_UNKNOWN: c_int = 10;
const PAM_MAXTRIES: c_int = 11;
const PAM_NEW_AUTHTOK_REQD: c_int = 12;
const PAM_ACCT_EXPIRED: c_int = 13;
const PAM_CONV_ERR: c_int = 19;

const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;

/// `PAM_MAX_RESP_SIZE`: PAM takes at most this much of a response.
const MAX_RESP: usize = 512;
const _: () = assert!(MAX_PASSWORD == MAX_RESP - 1);

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type ConvFn = unsafe extern "C" fn(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int;

#[repr(C)]
struct PamConv {
    conv: Option<ConvFn>,
    appdata_ptr: *mut c_void,
}

#[repr(C)]
struct PamHandle {
    _opaque: [u8; 0],
}

#[link(name = "pam")]
unsafe extern "C" {
    fn pam_start(
        service: *const c_char,
        user: *const c_char,
        conv: *const PamConv,
        pamh: *mut *mut PamHandle,
    ) -> c_int;
    #[cfg(feature = "faults")]
    fn pam_start_confdir(
        service: *const c_char,
        user: *const c_char,
        conv: *const PamConv,
        confdir: *const c_char,
        pamh: *mut *mut PamHandle,
    ) -> c_int;
    fn pam_authenticate(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_acct_mgmt(pamh: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_end(pamh: *mut PamHandle, status: c_int) -> c_int;
    fn pam_strerror(pamh: *mut PamHandle, errnum: c_int) -> *const c_char;
}

/// The conversation's link to the client, and what became of it.
struct Conversation<'a> {
    input: &'a mut File,
    output: &'a mut File,
    /// Why an answer was refused before PAM saw it (a NUL, or longer
    /// than PAM takes): the conversation then fails, and its verdict is
    /// this denial whatever the stack made of it.
    refused: Option<String>,
    /// The client went away or broke the protocol: no verdict can reach
    /// it, and every later callback fails at once.
    gone: bool,
}

impl Conversation<'_> {
    fn send(&mut self, message: &Message) -> bool {
        if protocol::write_message(&mut *self.output, message).is_err() {
            self.gone = true;
        }
        !self.gone
    }

    /// The client's answer to a prompt just sent: the password, checked
    /// to be one PAM can take, or `None` (the conversation fails).
    fn answer(&mut self) -> Option<strand_auth::Password> {
        match protocol::read_message(&mut *self.input) {
            Ok(Message::Submit(password)) => {
                crate::fault_before_check(&mut *self.output);
                if let Some(why) = strand_auth::refusal(&password) {
                    self.refused = Some(why);
                    return None;
                }
                Some(password)
            }
            Ok(_) => {
                eprintln!("strand-auth: the client answered a prompt with something else");
                self.gone = true;
                None
            }
            Err(protocol::ProtocolError::Eof) => {
                self.gone = true;
                None
            }
            Err(e) => {
                eprintln!("strand-auth: {e}");
                self.gone = true;
                None
            }
        }
    }
}

/// Allocates a NUL-terminated copy of `bytes` with malloc (PAM frees
/// responses with `free`, after wiping them in the modules that read
/// passwords).
fn malloc_copy(bytes: &[u8]) -> *mut c_char {
    // [`check`] refused anything longer; the bound only keeps the copy
    // in PAM's size.
    let n = bytes.len().min(MAX_RESP - 1);
    // SAFETY: a fresh allocation of n + 1 bytes, written in bounds.
    unsafe {
        let p = libc::malloc(n + 1).cast::<u8>();
        if p.is_null() {
            return ptr::null_mut();
        }
        ptr::copy_nonoverlapping(bytes.as_ptr(), p, n);
        *p.add(n) = 0;
        p.cast()
    }
}

/// Frees a response array built by [`conversation`], wiping each answer.
///
/// SAFETY: `resp` is a calloc'd array of `n` responses whose `resp`
/// fields are null or malloc'd C strings.
unsafe fn free_responses(resp: *mut PamResponse, n: usize) {
    for i in 0..n {
        // SAFETY: in bounds of the array, per the contract.
        let r = unsafe { &mut *resp.add(i) };
        if !r.resp.is_null() {
            // SAFETY: a malloc'd NUL-terminated string.
            unsafe {
                let len = libc::strlen(r.resp);
                std::slice::from_raw_parts_mut(r.resp.cast::<u8>(), len).zeroize();
                libc::free(r.resp.cast());
            }
            r.resp = ptr::null_mut();
        }
    }
    // SAFETY: calloc'd, per the contract.
    unsafe { libc::free(resp.cast()) };
}

/// PAM's conversation callback. Each text goes to the client as PAM
/// says it; each hidden prompt goes to the client, which answers it with
/// a password. A visible prompt (a user name, a one-time code shown as
/// typed) cannot be answered by a password field and fails the
/// conversation, so the check fails closed; so does a client that went
/// away or refused an answer.
unsafe extern "C" fn conversation(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata.is_null() {
        return PAM_CONV_ERR;
    }
    let n = num_msg as usize;
    // SAFETY: `appdata` is the `Conversation` [`check`] passed, alive for
    // the whole `pam_*` call that calls back.
    let conv = unsafe { &mut *appdata.cast::<Conversation<'_>>() };
    if conv.gone || conv.refused.is_some() {
        return PAM_CONV_ERR;
    }
    // SAFETY: calloc of n zeroed responses (resp null, retcode 0).
    let out = unsafe { libc::calloc(n, std::mem::size_of::<PamResponse>()) }.cast::<PamResponse>();
    if out.is_null() {
        return PAM_BUF_ERR;
    }
    let fail = |code| {
        // SAFETY: `out` as built above; each caller returns at once, so
        // it is freed once and never handed to PAM.
        unsafe { free_responses(out, n) };
        code
    };
    for i in 0..n {
        // SAFETY: Linux-PAM passes an array of `num_msg` message pointers.
        let m = unsafe { *msg.add(i) };
        if m.is_null() {
            return fail(PAM_CONV_ERR);
        }
        // SAFETY: a valid message from PAM.
        let m = unsafe { &*m };
        let text = || {
            if m.msg.is_null() {
                String::new()
            } else {
                // SAFETY: PAM's NUL-terminated message text.
                unsafe { CStr::from_ptr(m.msg) }
                    .to_string_lossy()
                    .into_owned()
            }
        };
        match m.msg_style {
            PAM_PROMPT_ECHO_OFF => {
                if !conv.send(&Message::Prompt(text())) {
                    return fail(PAM_CONV_ERR);
                }
                let Some(password) = conv.answer() else {
                    return fail(PAM_CONV_ERR);
                };
                let p = malloc_copy(password.as_bytes());
                drop(password);
                if p.is_null() {
                    return fail(PAM_BUF_ERR);
                }
                // SAFETY: `i < n`, inside `out`.
                unsafe { (*out.add(i)).resp = p };
            }
            PAM_ERROR_MSG | PAM_TEXT_INFO => {
                if !conv.send(&Message::Info(text())) {
                    return fail(PAM_CONV_ERR);
                }
            }
            // PAM_PROMPT_ECHO_ON (2) and anything unknown.
            _ => return fail(PAM_CONV_ERR),
        }
    }
    // SAFETY: PAM owns the array from here and frees it.
    unsafe { *resp = out };
    PAM_SUCCESS
}

/// The text of PAM error `code`.
fn strerror(pamh: *mut PamHandle, code: c_int) -> String {
    // SAFETY: pam_strerror accepts the handle (or null) and returns a
    // static string or null.
    let p = unsafe { pam_strerror(pamh, code) };
    if p.is_null() {
        return format!("PAM error {code}");
    }
    // SAFETY: a NUL-terminated string from PAM.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// PAM's refusals: the password or the account, not the system.
fn denial(code: c_int) -> bool {
    matches!(
        code,
        PAM_AUTH_ERR
            | PAM_PERM_DENIED
            | PAM_CRED_INSUFFICIENT
            | PAM_USER_UNKNOWN
            | PAM_MAXTRIES
            | PAM_ACCT_EXPIRED
            | PAM_NEW_AUTHTOK_REQD
    )
}

/// What a conversation came to: the verdict and its message, or `None`
/// when the client went away (no verdict can reach it).
pub type Outcome = Option<(Code, String)>;

/// Runs one conversation for `user` with `service` (and, with `faults`,
/// a private config dir), talking to the client on `input` and `output`.
/// PAM's texts went to the client as they came; the verdict's message is
/// PAM's error text when it is not a success.
pub fn check(
    service: &str,
    user: &CStr,
    confdir: Option<&CStr>,
    input: &mut File,
    output: &mut File,
) -> Outcome {
    let Ok(service) = CString::new(service) else {
        return Some((Code::Error, "bad service name".into()));
    };
    let mut conv_data = Conversation {
        input,
        output,
        refused: None,
        gone: false,
    };
    let conv = PamConv {
        conv: Some(conversation),
        appdata_ptr: (&mut conv_data as *mut Conversation<'_>).cast(),
    };
    let mut pamh: *mut PamHandle = ptr::null_mut();
    // SAFETY: valid C strings, a conversation that outlives the handle,
    // and an out-pointer for the handle.
    let started = unsafe {
        match confdir {
            #[cfg(feature = "faults")]
            Some(dir) => pam_start_confdir(
                service.as_ptr(),
                user.as_ptr(),
                &conv,
                dir.as_ptr(),
                &mut pamh,
            ),
            #[cfg(not(feature = "faults"))]
            Some(_) => {
                return Some((Code::Error, "no private PAM confdir in this build".into()));
            }
            None => pam_start(service.as_ptr(), user.as_ptr(), &conv, &mut pamh),
        }
    };
    if started != PAM_SUCCESS || pamh.is_null() {
        return Some((Code::Error, strerror(ptr::null_mut(), started)));
    }
    // SAFETY: a started handle; the conversation data lives on this frame.
    let mut code = unsafe { pam_authenticate(pamh, 0) };
    if code == PAM_SUCCESS && !conv_data.gone && conv_data.refused.is_none() {
        // SAFETY: as above.
        code = unsafe { pam_acct_mgmt(pamh, 0) };
    }
    let what = strerror(pamh, code);
    // SAFETY: ends the handle started above, once.
    unsafe { pam_end(pamh, code) };
    if conv_data.gone {
        return None;
    }
    // A refused answer fails closed whatever the stack made of it.
    if let Some(why) = conv_data.refused.take() {
        return Some((Code::Denied, why));
    }
    Some(if code == PAM_SUCCESS {
        (Code::Success, String::new())
    } else if denial(code) {
        (Code::Denied, what)
    } else {
        (Code::Error, what)
    })
}
