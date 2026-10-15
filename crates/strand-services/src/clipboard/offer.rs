//! What a selection is, from the MIME types it offers (pure, tested
//! here): a text, an image kept as PNG or JPEG, another image (flagged,
//! not kept), or nothing read; and whether a password manager marked it
//! secret.

/// The type a password manager (KeePassXC, KWallet, Bitwarden's desktop
/// app) offers beside a secret it copies, holding `secret`.
pub const PASSWORD_HINT: &str = "x-kde-passwordManagerHint";

/// The most of the hint read: it holds one short word.
pub const HINT_BYTES: usize = 64;

/// The text types, in the order taken (a copy from `clipboard.copy`
/// offers each).
pub const TEXT: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

/// The image types kept in the history, in the order taken, with the
/// extension of their files.
pub const IMAGES: [(&str, &str); 2] = [("image/png", "png"), ("image/jpeg", "jpg")];

/// What a selection is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A text, read as this type.
    Text(String),
    /// An image kept in the history, read as this type.
    Image(String),
    /// An image of another type: `clipboard.holds_image`, never kept.
    OtherImage,
    /// Nothing Strand reads (or no selection).
    Empty,
}

/// Whether the offer carries a password manager's hint, which is read
/// (and judged by [`hint_is_secret`]) before anything else of it.
pub fn has_hint(mimes: &[String]) -> bool {
    mimes.iter().any(|m| m == PASSWORD_HINT)
}

/// Whether a hint read as `data` marks the offer secret: it says
/// `secret`, or it could not be read (`None`: fail closed).
pub fn hint_is_secret(data: Option<&[u8]>) -> bool {
    match data {
        None => true,
        Some(d) => String::from_utf8_lossy(d).trim() == "secret",
    }
}

/// What an offer of `mimes` is: a text whenever a text type is offered
/// (an image offered beside it, as browsers do for a copied image's
/// address, does not make it an image); else a PNG, then a JPEG; else
/// any other `image/*`.
pub fn kind(mimes: &[String]) -> Kind {
    let offered = |t: &str| mimes.iter().any(|m| m == t);
    if let Some(t) = TEXT.iter().find(|t| offered(t)) {
        return Kind::Text((*t).to_string());
    }
    if let Some((t, _)) = IMAGES.iter().find(|(t, _)| offered(t)) {
        return Kind::Image((*t).to_string());
    }
    if mimes.iter().any(|m| m.starts_with("image/")) {
        return Kind::OtherImage;
    }
    Kind::Empty
}

/// The extension of an image file of type `mime` (`png` when unknown).
pub fn extension(mime: &str) -> &'static str {
    IMAGES
        .iter()
        .find(|(t, _)| *t == mime)
        .map_or("png", |(_, e)| e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn offers_are_classified_text_first_then_png_then_jpeg() {
        assert_eq!(
            kind(&m(&["image/png", "text/plain", "UTF8_STRING"])),
            Kind::Text("UTF8_STRING".into()),
            "text before an image, UTF8_STRING before text/plain"
        );
        assert_eq!(
            kind(&m(&["STRING", "text/plain;charset=utf-8"])),
            Kind::Text("text/plain;charset=utf-8".into())
        );
        assert_eq!(
            kind(&m(&["image/jpeg", "image/png"])),
            Kind::Image("image/png".into())
        );
        assert_eq!(
            kind(&m(&["image/jpeg", "image/bmp"])),
            Kind::Image("image/jpeg".into())
        );
        assert_eq!(kind(&m(&["image/webp"])), Kind::OtherImage, "flagged only");
        assert_eq!(kind(&m(&["application/x-thing"])), Kind::Empty);
        assert_eq!(kind(&[]), Kind::Empty);
        assert_eq!(extension("image/jpeg"), "jpg");
        assert_eq!(extension("image/png"), "png");
    }

    #[test]
    fn a_password_managers_hint_marks_a_secret_and_fails_closed() {
        assert!(has_hint(&m(&["text/plain", PASSWORD_HINT])));
        assert!(!has_hint(&m(&["text/plain"])));
        assert!(hint_is_secret(Some(b"secret")));
        assert!(hint_is_secret(Some(b"secret\n")), "trimmed");
        assert!(hint_is_secret(None), "an unreadable hint is a secret");
        assert!(!hint_is_secret(Some(b"")));
        assert!(!hint_is_secret(Some(b"public")));
    }
}
