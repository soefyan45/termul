//! Prints the automation host's own log lines to stderr, so a CI failure
//! shows what the host decided. Host log lines carry codes and key names,
//! never values.

use log::{Level, LevelFilter, Log, Metadata, Record};

const HOST_TARGET: &str = "termul_manager_lib::browser_automation";

struct HostLog;

impl Log for HostLog {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= Level::Warn
            || (metadata.level() <= Level::Info && metadata.target().starts_with(HOST_TARGET))
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("host {} {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

pub fn install() {
    if log::set_logger(&HostLog).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
}
