//! The guide's steps: every ```` ```<lang> level=N file=NAME ```` block
//! in `docs/guide/level-N.md`, in page order, level 1 first (docs/guide,
//! "How to read the code blocks"): `strand` blocks, and the files beside
//! them a step needs (level 5's ```` ```css ```` template). Shared by
//! `guide.rs` and `tree_sitter.rs`.

use std::path::{Path, PathBuf};

/// One step: the whole of `file` after it.
#[derive(Debug, Clone)]
pub struct Block {
    /// `level-N.md:LINE`, where the fence opens.
    pub at: String,
    pub level: u32,
    /// The fence's language: `strand`, or another for a file that is not
    /// `.strand` (decisions.md m5-docs, "a step's file that is not
    /// `.strand`").
    pub lang: String,
    pub file: String,
    pub text: String,
}

impl Block {
    pub fn is_strand(&self) -> bool {
        self.lang == "strand"
    }
}

/// The repository's root.
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The guide's pages that hold steps, in order: `level-1.md` … as many
/// as exist.
pub fn pages() -> Vec<(u32, PathBuf)> {
    let dir = root().join("docs/guide");
    (1..)
        .map(|n| (n, dir.join(format!("level-{n}.md"))))
        .take_while(|(_, p)| p.exists())
        .collect()
}

/// Every step, in page order. A ```` ```strand ```` block on a level page
/// must say its level (that page's) and its file; a block in another
/// language is a step when it says both, and is not one (a command, a
/// keybind line, a TOML sample) when it says neither. A block nested in a
/// longer fence (an example of the tag, as the README shows) is not a step.
pub fn blocks() -> Vec<Block> {
    let mut out = Vec::new();
    for (level, page) in pages() {
        let name = page.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&page).unwrap();
        for (line, info, body) in fences(&text) {
            let mut words = info.split_whitespace();
            let lang = words.next().unwrap_or("").to_string();
            let strand = lang == "strand";
            if !strand && !info.contains('=') {
                continue;
            }
            let at = format!("{name}:{line}");
            let mut block_level = None;
            let mut file = None;
            for w in words {
                match w.split_once('=') {
                    Some(("level", v)) => block_level = v.parse::<u32>().ok(),
                    Some(("file", v)) => file = Some(v.to_string()),
                    _ => panic!("{at}: unknown word {w:?} in the fence's tag"),
                }
            }
            let block_level = block_level.unwrap_or_else(|| panic!("{at}: no level=N"));
            let file = file.unwrap_or_else(|| panic!("{at}: no file=NAME"));
            assert_eq!(block_level, level, "{at}: a level-{level} page's step");
            assert!(
                file.ends_with(".strand") == strand && !file.contains('/'),
                "{at}: file={file} is not a {} file in the config directory",
                if strand { ".strand" } else { "non-.strand" }
            );
            out.push(Block {
                at,
                level,
                lang,
                file,
                text: body,
            });
        }
    }
    out
}

/// The fenced code blocks of a Markdown page: (line the fence opens on,
/// 1-based; its info string; its contents). A fence is three or more
/// backticks, closed by a line of at least as many and nothing else.
pub fn fences(text: &str) -> Vec<(usize, String, String)> {
    let mut out = Vec::new();
    let mut open: Option<(usize, usize, String, String)> = None;
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let ticks = trimmed.chars().take_while(|&c| c == '`').count();
        match &mut open {
            None if ticks >= 3 => {
                open = Some((
                    i + 1,
                    ticks,
                    trimmed[ticks..].trim().to_string(),
                    String::new(),
                ));
            }
            None => {}
            Some((start, n, info, body)) => {
                if ticks >= *n && trimmed[ticks..].trim().is_empty() {
                    out.push((*start, std::mem::take(info), std::mem::take(body)));
                    open = None;
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
            }
        }
    }
    assert!(open.is_none(), "an unclosed code fence");
    out
}
