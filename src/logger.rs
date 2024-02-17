use anyhow::Result;
use log::Record;
use std::{fmt::Arguments, fs::OpenOptions, io::Write};

use crate::InputFile;

/// A trait that allows writing to a path or to an open file.
pub trait LogDest {
    fn my_write_fmt(self, fmt: Arguments) -> Result<()>;
}

impl LogDest for &InputFile {
    fn my_write_fmt(self, fmt: Arguments) -> Result<()> {
        if let Some(ref log_path) = self.log_path {
            self.create_log_directory()?;
            let mut file = OpenOptions::new()
                .append(true)
                .create(true)
                .open(log_path)?;
            LogDest::my_write_fmt(&mut file, fmt)?;
        }
        Ok(())
    }
}

impl LogDest for &mut std::fs::File {
    fn my_write_fmt(self, fmt: Arguments) -> Result<()> {
        self.write_fmt(fmt)?;
        self.write_all(b"\n")?;
        Ok(())
    }
}

impl LogDest for &mut Option<std::fs::File> {
    fn my_write_fmt(self, fmt: Arguments) -> Result<()> {
        if let Some(file) = self.as_mut() {
            return LogDest::my_write_fmt(file, fmt);
        }
        Ok(())
    }
}

/// Log to both the logger and a file (if it exists)
#[macro_export]
macro_rules! _trace {
    ($dest:expr, $($fmt:tt)*) => {{
        $crate::_log!(log::Level::Trace, $dest, $($fmt)*)
    }};
}
/// Log to both the logger and a file (if it exists)
#[macro_export]
macro_rules! _debug {
    ($dest:expr, $($fmt:tt)*) => {{
        $crate::_log!(log::Level::Debug, $dest, $($fmt)*)
    }};
}
/// Log to both the logger and a file (if it exists)
#[macro_export]
macro_rules! _info {
    ($dest:expr, $($fmt:tt)*) => {{
        $crate::_log!(log::Level::Info, $dest, $($fmt)*)
    }};
}
/// Log to both the logger and a file (if it exists)
#[macro_export]
macro_rules! _warn {
    ($dest:expr, $($fmt:tt)*) => {{
        $crate::_log!(log::Level::Warn, $dest, $($fmt)*)
    }};
}
/// Log to both the logger and a file (if it exists)
#[macro_export]
macro_rules! _error {
    ($dest:expr, $($fmt:tt)*) => {{
        $crate::_log!(log::Level::Error, $dest, $($fmt)*)
    }};
}

/// Log to both the logger and a file (if it exists)
#[macro_export]
macro_rules! _log {
    ($level: expr, $dest:expr, $($fmt:tt)*) => {{
        $crate::_log($level, $dest, format_args!($($fmt)*))
    }};
}

/// Log to both the logger and a file (if it exists)
pub(crate) fn _log<T>(level: log::Level, dest: T, message: Arguments)
where
    T: LogDest,
{
    let record = Record::builder()
        .args(message)
        .level(level)
        .file(Some(file!()))
        .line(Some(line!()))
        .module_path(Some(module_path!()))
        .build();
    log::logger().log(&record);
    if let Err(err) = dest.my_write_fmt(message) {
        log::warn!("Could not write to log file: {}", err);
    }
}
