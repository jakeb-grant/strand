//! (proc-permit) Which program a command runs: the item after a leading
//! `stdbuf` with only buffering options or an `env` with only harmless
//! `NAME=value` items (spec.md 5.8). One rule for the two places that
//! ask: `strand check`, which checks that program against `permit exec`,
//! and the runtime, which names it when the command fails (``"`pacman`
//! failed (exit status: 1)"``, not `stdbuf`).

/// (proc-permit) Where the program a command runs is: past a leading
/// `stdbuf` with only `-i`/`-o`/`-e` options and an `env` with only
/// `NAME=value` items setting a variable of [`ENV_SEEN`], in either order
/// (spec.md 5.8). `items` are the command's items, `None` for one that is
/// not a text literal (a parameter), which stops the seeing through. The
/// index is always of a written-out item: 0 when nothing is seen through.
pub fn program_at(items: &[Option<&str>]) -> usize {
    let mut at = 0;
    loop {
        let Some(Some(word)) = items.get(at) else {
            return at;
        };
        match past(word, items, at) {
            Ok(next) => at = next,
            Err(_) => return at,
        }
    }
}

/// Where the seeing through stopped at a wrapper.
enum Stop {
    /// At this item: one the wrapper is not seen through with, a
    /// parameter, or nothing (past the end).
    At(usize),
    /// `stdbuf` with no option: it runs nothing.
    NoOption,
}

/// The program past the wrapper `word` at `at`: its index, or where the
/// seeing through stopped. `Err` with `At(at)` when `word` is no wrapper.
fn past(word: &str, items: &[Option<&str>], at: usize) -> Result<usize, Stop> {
    let mut next = at + 1;
    match wrapper(word) {
        "stdbuf" => {
            let mut options = 0;
            loop {
                match items.get(next) {
                    Some(Some(o)) if buffer_option(o).is_some() => {
                        if buffer_option(o) == Some(true) {
                            next += 1;
                        } else {
                            // `-o L`: the mode is the next item.
                            match items.get(next + 1) {
                                Some(Some(m)) if buffer_mode(m) => next += 2,
                                _ => return Err(Stop::At(next + 1)),
                            }
                        }
                        options += 1;
                    }
                    _ => break,
                }
            }
            // stdbuf runs nothing without an option.
            if options == 0 {
                return Err(match items.get(next) {
                    Some(Some(p)) if program_word(p) => Stop::NoOption,
                    _ => Stop::At(next),
                });
            }
        }
        "env" => {
            while matches!(items.get(next), Some(Some(a)) if env_assignment(a)) {
                next += 1;
            }
        }
        _ => return Err(Stop::At(at)),
    }
    // The program: written out, no option, and no assignment, which
    // `env` would take as one (`env PATH=/x pacman` runs `env`'s
    // choice, so `env` is the program checked).
    match items.get(next) {
        Some(Some(p)) if program_word(p) => Ok(next),
        _ => Err(Stop::At(next)),
    }
}

/// An item that can be the program a wrapper runs.
fn program_word(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('-') && !p.contains('=')
}

/// (proc-permit) Why the program [`program_at`] finds is `stdbuf` or
/// `env` itself, not the program after it: the item that stopped the
/// seeing through, for `check::no_permit`'s help. `None` when the program
/// is no wrapper. The help never suggests permitting the wrapper: that
/// permit would allow any program after it (spec.md 5.8).
pub fn not_seen_through(items: &[Option<&str>]) -> Option<String> {
    let at = program_at(items);
    let word = (*items.get(at)?)?;
    let name = wrapper(word);
    let stop = match past(word, items, at) {
        Err(_) if name.is_empty() => return None,
        Err(stop) => stop,
        Ok(_) => return None,
    };
    let rule = if name == "env" {
        format!(
            "`{word}` is seen through only when it sets {}",
            seen_names()
        )
    } else {
        format!(
            "`{word}` is seen through only with `-i`, `-o` and `-e` options, each with its \
             mode (`-oL`, `-o L`, `-e0`)"
        )
    };
    let why = match stop {
        Stop::NoOption => format!("`{word}` with no option is not seen through"),
        Stop::At(i) => match items.get(i) {
            None => format!("nothing follows `{word}` to run, so `{word}` itself is checked"),
            Some(None) => format!(
                "{rule}, before a program written out; a parameter stops it, so `{word}` \
                 itself is checked"
            ),
            Some(Some("")) => format!("{rule}; an empty item stops it"),
            Some(Some(item)) => format!("{rule}; `{item}` stops it"),
        },
    };
    Some(format!(
        "{why}. Without that, the program after `{word}` is the one `permit exec` checks \
         (`permit exec \"{word}\"` would allow any program after it)"
    ))
}

/// [`ENV_SEEN`] as a sentence: `LANG, LANGUAGE, …, or PYTHONUNBUFFERED`.
fn seen_names() -> String {
    let (last, rest) = ENV_SEEN.split_last().unwrap_or((&"", &[]));
    format!("{} or {last}", rest.join(", "))
}

/// A `stdbuf` option: `Some(true)` with its mode (`-oL`, `--output=L`),
/// `Some(false)` when the mode is the next item (`-o`), `None` for
/// anything else.
fn buffer_option(o: &str) -> Option<bool> {
    for (short, long) in [("-i", "--input="), ("-o", "--output="), ("-e", "--error=")] {
        if o == short {
            return Some(false);
        }
        if let Some(m) = o.strip_prefix(short).or_else(|| o.strip_prefix(long))
            && buffer_mode(m)
        {
            return Some(true);
        }
    }
    None
}

/// A `stdbuf` mode: `L` (lines) or a size (`0`, `4096`, `1K`, `1MiB`).
fn buffer_mode(m: &str) -> bool {
    m == "L"
        || m.starts_with(|c: char| c.is_ascii_digit())
            && m.chars().all(|c| c.is_ascii_alphanumeric())
}

/// (proc-permit) Which wrapper `word` names, when it is `stdbuf` or
/// `env` itself: the bare word (found on `PATH`, as a bare program is) or
/// the system's own copy (`/usr/bin/…`, `/bin/…`). Any other path is a
/// program of its own that happens to share the name, so the permit must
/// name it (spec.md 5.6: a program run by its path needs its own permit).
fn wrapper(word: &str) -> &str {
    let name = word
        .strip_prefix("/usr/bin/")
        .or_else(|| word.strip_prefix("/bin/"))
        .unwrap_or(word);
    match name {
        "stdbuf" | "env" => name,
        _ => "",
    }
}

/// (proc-permit) The variables an `env` seen through may set: the
/// locale, the time zone, the terminal's name and width, colour and
/// Python's buffering. None of them changes which program runs or what
/// code it loads (`PATH`, `LD_PRELOAD`, `PYTHONPATH`, `BASH_ENV` and the
/// like would), so the program after them is still the one permitted.
/// A name ending in `*` is a prefix (`LC_ALL`, `LC_MESSAGES`).
const ENV_SEEN: &[&str] = &[
    "LANG",
    "LANGUAGE",
    "LC_*",
    "TZ",
    "TERM",
    "NO_COLOR",
    "COLUMNS",
    "PYTHONUNBUFFERED",
];

/// An `env` item that sets a variable of [`ENV_SEEN`]: `NAME=value`.
fn env_assignment(a: &str) -> bool {
    let Some((name, _)) = a.split_once('=') else {
        return false;
    };
    let well_formed = {
        let mut cs = name.chars();
        cs.next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    well_formed
        && ENV_SEEN.iter().any(|seen| match seen.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix) && name.len() > prefix.len(),
            None => name == *seen,
        })
}

/// The program `argv` runs, as [`program_at`] finds it: its name for a
/// failure's headline. Empty for an empty command.
pub fn program(argv: &[String]) -> &str {
    let items: Vec<Option<&str>> = argv.iter().map(|a| Some(a.as_str())).collect();
    argv.get(program_at(&items)).map_or("", String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(argv: &[&str]) -> String {
        let owned: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
        program(&owned).to_string()
    }

    #[test]
    fn the_program_is_past_stdbuf_and_env() {
        assert_eq!(at(&["pacman", "-Syu"]), "pacman");
        assert_eq!(at(&["stdbuf", "-oL", "-eL", "rsync", "-a"]), "rsync");
        assert_eq!(
            at(&["/usr/bin/env", "LC_ALL=C", "stdbuf", "-o", "L", "sh"]),
            "sh"
        );
        assert_eq!(at(&["env", "PATH=/x", "pacman"]), "env");
        assert_eq!(
            at(&["env", "LC_ALL=C", "LD_PRELOAD=/x.so", "pacman"]),
            "env"
        );
        assert_eq!(at(&["/tmp/env", "pacman"]), "/tmp/env");
        assert_eq!(at(&["stdbuf", "--help"]), "stdbuf");
        assert_eq!(at(&["env", "LC_ALL=C"]), "env");
        assert_eq!(at(&[]), "");
    }

    fn why(argv: &[&str]) -> Option<String> {
        let items: Vec<Option<&str>> = argv.iter().map(|a| Some(*a)).collect();
        not_seen_through(&items)
    }

    /// (proc-permit) When `env` or `stdbuf` is the program checked, the
    /// help names what stopped the seeing through and never suggests
    /// permitting the wrapper, which would allow any program after it.
    #[test]
    fn what_stopped_the_see_through_is_named() {
        assert_eq!(why(&["pacman"]), None);
        assert_eq!(why(&["env", "LC_ALL=C", "pacman"]), None);
        assert_eq!(why(&["/tmp/env", "pacman"]), None);
        let path = why(&["env", "PATH=/x", "pacman"]).unwrap();
        assert!(
            path.starts_with(
                "`env` is seen through only when it sets LANG, LANGUAGE, LC_*, TZ, TERM, \
                 NO_COLOR, COLUMNS or PYTHONUNBUFFERED; `PATH=/x` stops it. Without that, \
                 the program after `env` is the one `permit exec` checks"
            ),
            "{path}"
        );
        assert!(path.ends_with("(`permit exec \"env\"` would allow any program after it)"));
        let deep = why(&["env", "LC_ALL=C", "/usr/bin/stdbuf", "--help", "pacman"]).unwrap();
        assert!(
            deep.starts_with("`/usr/bin/stdbuf` is seen through only with `-i`, `-o` and `-e`")
                && deep.contains("; `--help` stops it."),
            "{deep}"
        );
        let mode = why(&["stdbuf", "-o", "pacman"]).unwrap();
        assert!(mode.contains("`pacman` stops it"), "{mode}");
        let bare = why(&["stdbuf", "pacman"]).unwrap();
        assert!(
            bare.starts_with("`stdbuf` with no option is not seen through."),
            "{bare}"
        );
        let end = why(&["env", "LC_ALL=C"]).unwrap();
        assert!(end.starts_with("nothing follows `env` to run"), "{end}");
        let items = [Some("env"), Some("LC_ALL=C"), None, Some("pacman")];
        let param = not_seen_through(&items).unwrap();
        assert!(param.contains("a parameter stops it"), "{param}");
        let flag = why(&["env", "-i", "pacman"]).unwrap();
        assert!(flag.contains("; `-i` stops it."), "{flag}");
        for w in [path, deep, mode, bare, end, param, flag] {
            assert!(!w.contains("add `permit exec"), "{w}");
        }
    }
}
