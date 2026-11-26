//! Log output for the `reso-ingest` binary: text or JSON lines, level from `RUST_LOG`
//!
//! The library crates only emit [`tracing`] events; nothing is printed unless a subscriber is
//! installed. The binary installs one through [`cli::run_main`](crate::cli::run_main), writing to
//! stderr. Every event of a run carries the fields of its spans: `run_id` (the `run` span of `sync`
//! and `media`), `server` (one span per server task) and `resource` (one span per resource that
//! `sync` pulls). Logs never hold tokens, secrets or record contents.

use std::str::FromStr;

use tracing::Subscriber;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

/// The level filter used when `RUST_LOG` is unset or invalid
pub const DEFAULT_FILTER: &str = "info";

/// The environment variable that selects the log format when `--log-format` is not given
pub const LOG_FORMAT_ENV: &str = "RESO_LOG_FORMAT";

/// How log lines are written
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    /// One human-readable line per event, span fields as a prefix
    #[default]
    Text,
    /// One JSON object per line (`timestamp`, `level`, `fields`, `target`, `span`, `spans`), for log
    /// shippers
    Json,
}

impl FromStr for LogFormat {
    type Err = String;

    /// Parses `text` or `json` (any case)
    ///
    /// # Examples
    ///
    /// ```
    /// use reso_ingest::LogFormat;
    ///
    /// assert_eq!("JSON".parse::<LogFormat>(), Ok(LogFormat::Json));
    /// assert!("xml".parse::<LogFormat>().is_err());
    /// ```
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!(
                "unknown log format `{other}` (expected text or json)"
            )),
        }
    }
}

/// The level filter from `RUST_LOG`, or [`DEFAULT_FILTER`] when it is unset or invalid
pub fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

/// A subscriber that writes `format` lines to `writer`, keeping the events `filter` allows
///
/// Install it with [`tracing::subscriber::set_global_default`]. JSON lines list every enclosing
/// span with its fields under `spans`, so `run_id`, `server` and `resource` are on each line.
///
/// # Examples
///
/// ```
/// use reso_ingest::{log_subscriber, LogFormat};
/// use tracing_subscriber::EnvFilter;
///
/// let subscriber = log_subscriber(LogFormat::Json, EnvFilter::new("info"), std::io::stderr);
/// tracing::subscriber::with_default(subscriber, || tracing::info!("hello"));
/// ```
pub fn log_subscriber<W>(
    format: LogFormat,
    filter: EnvFilter,
    writer: W,
) -> Box<dyn Subscriber + Send + Sync>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer);
    match format {
        LogFormat::Text => Box::new(builder.finish()),
        LogFormat::Json => Box::new(
            builder
                .json()
                .with_current_span(false)
                .with_span_list(true)
                .finish(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn formats_parse_in_any_case() {
        assert_eq!("text".parse::<LogFormat>(), Ok(LogFormat::Text));
        assert_eq!("Text".parse::<LogFormat>(), Ok(LogFormat::Text));
        assert_eq!("JSON".parse::<LogFormat>(), Ok(LogFormat::Json));
        let err = "XML".parse::<LogFormat>().unwrap_err();
        assert!(
            err.contains("`xml`") && err.contains("text or json"),
            "{err}"
        );
    }

    #[test]
    fn text_lines_are_not_json_and_respect_the_filter() {
        let buf = Buffer::default();
        let writer = buf.clone();
        let subscriber = log_subscriber(LogFormat::Text, EnvFilter::new("info"), move || {
            writer.clone()
        });
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(server = "mls", "hello");
            tracing::debug!("hidden");
        });
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            text.contains("hello") && text.contains("server=\"mls\""),
            "{text}"
        );
        assert!(!text.contains("hidden"), "{text}");
        assert!(!text.trim_start().starts_with('{'), "{text}");
    }

    #[test]
    fn env_filter_always_gives_a_filter() {
        // Whatever RUST_LOG holds (unset, valid or invalid), a filter comes back
        let _ = env_filter().to_string();
    }
}
