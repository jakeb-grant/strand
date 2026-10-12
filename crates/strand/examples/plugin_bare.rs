//! A plugin binary without `strand::process_setup!()` and without
//! services: the case `Builder::run`'s process-setup warning is for
//! (tests/m5/plugin.rs, `the_setup_warning_shows_without_the_macro`).
//! Do not copy it; copy `plugin_hello.rs`.

fn main() -> std::process::ExitCode {
    strand::Shell::builder().run()
}
