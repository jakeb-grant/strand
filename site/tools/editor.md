# Editor setup

Strand's editor support is a language server and a tree-sitter grammar.
Both live outside the shell, so they cost `strand` nothing. The
repository ships no editor plugin or editor configuration; this page
gives the settings that start them in Helix and Neovim.

## The language server

The language server is `strand-dev lsp`. Build `strand-dev` from the
checkout and put it on your `PATH`:

```sh
cargo build --release -p strand-dev
cp target/release/strand-dev ~/.local/bin/
```

`strand-dev lsp` speaks the Language Server Protocol over standard
input and output. An editor starts it for files ending in `.strand`.

### What it does

- **Diagnostics**: what `strand check` finds in the config's files
  (all of it but a missing `strand-auth` helper), for every file of the
  config: when a file opens or is saved, and 200 ms after you stop
  typing.
- **Completion** after `$`, `.` and `<->`, and of element, component,
  keyword, prop and event names and enum values.
- **Hover**: a name's type, and its documentation from the schemas.
- **Go to definition**, across the config's files.
- **Rename** across files: a `state`, a `let`, a component, a token, and
  a component's prop at its call sites. A rename that would break the
  config, or one of a built-in name, is refused.
- **Code actions**: the "did you mean" fixes the diagnostics carry, a
  missing `key`, and "extract component", which moves the selected
  subtree into a new component.
- **Formatting**, with the formatter `strand fmt` uses.

It checks a file together with the rest of its config: the config
directory, when the file is part of it; else an editor workspace folder
that holds `.strand` files and the file; else the file's own directory.
That is the rule `strand check <file>` uses, with the workspace folder
added. Changes made to the files outside the editor are seen too.

Two initialization options change its defaults: `debounceMs` (how long
after typing it waits, default 200) and `configDir` (the config
directory, default the one `strand check` uses).

## Helix

Add this to `~/.config/helix/languages.toml`:

```toml
use-grammars = { only = ["strand"] }

[language-server.strand]
command = "strand-dev"
args = ["lsp"]

[[language]]
name = "strand"
scope = "source.strand"
file-types = ["strand"]
comment-token = "//"
indent = { tab-width = 2, unit = "  " }
language-servers = ["strand"]

[[grammar]]
name = "strand"
source = { path = "/path/to/strand/crates/strand-dev/tree-sitter-strand" }
```

`use-grammars` keeps the build step below to this one grammar; leave
it out if you build other grammars yourself. Set `path` to your
checkout. Then build the grammar and copy the highlight query:

```sh
mkdir -p ~/.config/helix/runtime/grammars ~/.config/helix/runtime/queries/strand
hx --grammar build
cp /path/to/strand/crates/strand-dev/tree-sitter-strand/queries/highlights.scm \
  ~/.config/helix/runtime/queries/strand/
hx --health strand
```

On Arch Linux the program is called `helix`, not `hx`.

The queries use nvim-treesitter's capture names. Helix reads them, but a
capture its themes name differently is not coloured: Helix themes call
numbers `constant.numeric`, for one, where the query says `number`.

## Neovim

Neovim 0.11 and later configure language servers with `vim.lsp.config`.
In `~/.config/nvim/init.lua`:

```lua
vim.filetype.add({ extension = { strand = "strand" } })

vim.lsp.config("strand", {
  cmd = { "strand-dev", "lsp" },
  filetypes = { "strand" },
})
vim.lsp.enable("strand")
```

For highlighting, compile the grammar into Neovim's parser directory
and copy its queries:

```sh
cd /path/to/strand/crates/strand-dev/tree-sitter-strand
mkdir -p ~/.local/share/nvim/site/parser ~/.local/share/nvim/site/queries/strand
cc -shared -fPIC -O2 -I src src/parser.c src/scanner.c \
  -o ~/.local/share/nvim/site/parser/strand.so
cp queries/*.scm ~/.local/share/nvim/site/queries/strand/
```

and start it for `.strand` buffers:

```lua
vim.api.nvim_create_autocmd("FileType", {
  pattern = "strand",
  callback = function() vim.treesitter.start() end,
})
```

## VS Code and other editors

There is no VS Code extension for Strand. VS Code starts a language
server only through an extension, and the repository has none, nor a
TextMate grammar for its highlighting. Any editor whose language client
can run a command over standard input and output for `*.strand` files
can use `strand-dev lsp` as it is.

## The tree-sitter grammar

The grammar is in
[`crates/strand-dev/tree-sitter-strand`](../../crates/strand-dev/tree-sitter-strand):
`grammar.js`, the parser generated from it (`src/parser.c`, beside a
hand-written `src/scanner.c`), and four queries in `queries/`:

- `highlights.scm`, the highlighting;
- `indents.scm`, one indent level inside every block and bracketed
  list;
- `injections.scm`, which injects only into comments (as the
  `comment` language), since strings hold no other language;
- `locals.scm`, the scopes, definitions and references.

All four use nvim-treesitter's capture names. The grammar follows
[`docs/grammar.md`](../../docs/grammar.md), and CI checks that the
committed generated files are what tree-sitter 0.27.0 makes of
`grammar.js`.
