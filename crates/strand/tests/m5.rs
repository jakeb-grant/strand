//! M5's integration tests, one binary (each test binary links the whole
//! graph): a module per stream under `tests/m5/` (docs/m5-plan.md,
//! "Ground rules": test binaries). The module list is the integrator's.

mod m5 {
    mod cli;
    mod dbus_ipc;
    mod export;
    mod inspect;
    mod new;
    mod newcomer;
    mod plugin;
    mod process_setup;
    mod report;
}
