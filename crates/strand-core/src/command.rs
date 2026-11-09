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
                                    _ => return at,
                                }
                            }
                            options += 1;
                        }
                        _ => break,
                    }
                }
                // stdbuf runs nothing without an option.
                if options == 0 {
                    return at;
                }
            }
            "env" => {
                while matches!(items.get(next), Some(Some(a)) if env_assignment(a)) {
                    next += 1;
                }
            }
            _ => return at,
        }
        // The program: written out, no option, and no assignment, which
        // `env` would take as one (`env PATH=/x pacman` runs `env`'s
        // choice, so `env` is the program checked).
        match items.get(next) {
            Some(Some(p)) if !p.is_empty() && !p.starts_with('-') && !p.contains('=') => at = next,
            _ => return at,
        }
    }
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
}
