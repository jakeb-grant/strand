//! Password buffers that are wiped when dropped.

use zeroize::Zeroizing;

/// A password on its way to PAM. Its bytes are overwritten with zeros
/// when it is dropped, and its `Debug` shows only its length. Built from
/// a `String` it takes that string's buffer as it is (no copy is left
/// behind).
pub struct Password(Zeroizing<Vec<u8>>);

impl Password {
    /// Takes `bytes` (no copy).
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for Password {
    fn from(s: String) -> Self {
        Self::from_bytes(s.into_bytes())
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Password(<{} bytes>)", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_password() {
        let p = Password::from("hunter2".to_string());
        let shown = format!("{p:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert_eq!(shown, "Password(<7 bytes>)");
    }

    #[test]
    fn a_string_buffer_is_taken_without_a_copy() {
        let s = String::from("correct horse");
        let ptr = s.as_ptr();
        let p = Password::from(s);
        assert_eq!(p.as_bytes().as_ptr(), ptr);
        assert_eq!(p.as_bytes(), b"correct horse");
    }
}
