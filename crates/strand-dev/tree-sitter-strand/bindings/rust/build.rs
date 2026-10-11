//! Compiles the generated parser and the external scanner (C) into the
//! package's static library.

fn main() {
    let src = std::path::Path::new("src");
    let mut c = cc::Build::new();
    c.std("c11")
        .include(src)
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-trigraphs");
    for file in ["parser.c", "scanner.c"] {
        let path = src.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        c.file(path);
    }
    println!("cargo:rerun-if-changed=src/tree_sitter/parser.h");
    c.compile("tree-sitter-strand");
}
