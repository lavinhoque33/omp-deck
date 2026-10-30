//! The relay's logger: `env_logger` (`RUST_LOG`, default `info`) with the wire crates clamped to
//! `info`, whatever `RUST_LOG` says. Their debug/trace output dumps frame payloads, hello bytes
//! (room secrets, admission tokens) included.

use log::{Level, Log, Metadata, Record};

/// Targets clamped to `info`, with their submodules (`tungstenite::protocol::frame`, …).
const WIRE_TARGETS: [&str; 6] = [
    "tungstenite",
    "tokio_tungstenite",
    "rustls",
    "tokio_rustls",
    "hyper",
    "hyper_util",
];

fn is_wire(target: &str) -> bool {
    WIRE_TARGETS.iter().any(|w| {
        target
            .strip_prefix(w)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

/// Applies the clamp before the inner logger's own directives, so a more specific directive
/// (`tungstenite::protocol=trace`) cannot lift it.
struct Clamped<L>(L);

impl<L: Log> Log for Clamped<L> {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        (metadata.level() <= Level::Info || !is_wire(metadata.target())) && self.0.enabled(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            self.0.log(record);
        }
    }

    fn flush(&self) {
        self.0.flush();
    }
}

/// Installs the logger (once per process; later calls are ignored).
pub fn init_logging() {
    let logger =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).build();
    let max = logger.filter();
    if log::set_boxed_logger(Box::new(Clamped(logger))).is_ok() {
        log::set_max_level(max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled(logger: &impl Log, target: &str, level: Level) -> bool {
        logger.enabled(&Metadata::builder().target(target).level(level).build())
    }

    #[test]
    fn wire_targets_are_clamped_to_info_even_by_specific_directives() {
        let logger = Clamped(
            env_logger::Builder::new()
                .parse_filters("trace,tungstenite::protocol=trace,rustls=debug")
                .build(),
        );
        for target in [
            "tungstenite",
            "tungstenite::protocol::frame",
            "tokio_tungstenite::compat",
            "rustls::server::hs",
            "tokio_rustls",
            "hyper::proto::h1::io",
            "hyper_util::server",
        ] {
            assert!(!enabled(&logger, target, Level::Trace), "{target}");
            assert!(!enabled(&logger, target, Level::Debug), "{target}");
            assert!(enabled(&logger, target, Level::Info), "{target}");
            assert!(enabled(&logger, target, Level::Warn), "{target}");
        }
        // Only whole path segments are clamped (env_logger's own `rustls=debug` directive also
        // matches `rustlsish`, so debug); the relay's own targets keep their level.
        assert!(enabled(&logger, "rustlsish", Level::Debug));
        assert!(enabled(&logger, "deck_relay::server", Level::Trace));
        // The inner filter still applies below the clamp.
        let quiet = Clamped(env_logger::Builder::new().parse_filters("warn").build());
        assert!(!enabled(&quiet, "tungstenite", Level::Info));
        assert!(enabled(&quiet, "tungstenite", Level::Warn));
    }
}
