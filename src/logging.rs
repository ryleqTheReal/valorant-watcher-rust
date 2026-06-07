use std::fs;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use crate::error::Result;
use crate::paths;

// keeps the non-blocking writer threads alive; drop only at app exit
pub struct Guards {
    _debug: WorkerGuard,
    _errors: WorkerGuard,
}

// stderr + data/debug.log get INFO and up, data/errors.log gets WARN and up
pub fn init() -> Result<Guards> {
    let data_dir = paths::data_dir()?;
    fs::create_dir_all(&data_dir)?;

    let (debug_writer, debug_guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(&data_dir, "debug.log"));
    let (error_writer, error_guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(&data_dir, "errors.log"));

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(LevelFilter::INFO),
        )
        .with(
            fmt::layer()
                .with_ansi(false)
                .with_writer(debug_writer)
                .with_filter(LevelFilter::INFO),
        )
        .with(
            fmt::layer()
                .with_ansi(false)
                .with_writer(error_writer)
                .with_filter(LevelFilter::WARN),
        )
        .init();

    Ok(Guards {
        _debug: debug_guard,
        _errors: error_guard,
    })
}
