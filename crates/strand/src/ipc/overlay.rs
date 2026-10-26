//! Runtime overlays from the command line (S-ipc): the token spelling
//! (`'$accent'`, decisions.md m5-owner Q10) and `strand clear <path>`,
//! the CLI twin of the overlay notice's `[clear]` (IPC `clear{path}`).
//!
//! `strand set '$accent' '#ff8800'` writes the token overlay layer
//! (`Instance::set_token_overlay`: kept in
//! `$XDG_STATE_HOME/strand/tokens.toml`, taking the token swap path);
//! `strand clear '$accent'` drops it, and `strand clear
//! theme.prefs.accent` drops a settings field's runtime overlay, so the
//! file's value (or the default) applies again. Clearing what has no
//! overlay is not an error: the answer says `"cleared": false`.

use super::Request;
use super::query::ask;

/// The token a path names with the token spelling (`$accent` →
/// `accent`), or `None` for any other path.
pub(crate) fn token_path(path: &str) -> Option<&str> {
    path.strip_prefix('$').filter(|p| !p.is_empty())
}

/// `strand clear`'s usage.
pub(crate) const CLEAR_USAGE: &str = "usage: strand clear <'$token' | file.settings.field>";

/// `strand clear <path>`.
pub fn clear_cli(args: &[String]) -> Result<String, String> {
    let [path] = args else {
        return Err(CLEAR_USAGE.into());
    };
    ask(&Request::Clear { path: path.clone() })?;
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_spelling_names_a_token() {
        assert_eq!(token_path("$accent"), Some("accent"));
        assert_eq!(token_path("$space.2"), Some("space.2"));
        assert_eq!(token_path("$"), None);
        assert_eq!(token_path("theme.look"), None);
        assert!(clear_cli(&[]).unwrap_err().starts_with("usage"));
    }
}
