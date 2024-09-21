//! Time error types.

use std::error;
use std::fmt;

/// Errors encountered by the timer implementation.
///
/// Currently, there are two different errors that can occur:
///
/// * `shutdown` occurs when a timer operation is attempted, but the timer
///   instance has been dropped. In this case, the operation will never be able
///   to complete and the `shutdown` error is returned. This is a permanent
///   error, i.e., once this error is observed, timer operations will never
///   succeed in the future.
///
/// * `at_capacity` occurs when a timer operation is attempted, but the timer
///   instance is currently handling its maximum number of outstanding sleep instances.
///   In this case, the operation is not able to be performed at the current
///   moment, and `at_capacity` is returned. This is a transient error, i.e., at
///   some point in the future, if the operation is attempted again, it might
///   succeed. Callers that observe this error should attempt to [shed load]. One
///   way to do this would be dropping the future that issued the timer operation.
///
/// [shed load]: https://en.wikipedia.org/wiki/Load_Shedding
/// /// 计时器实现遇到的错误。
///
/// 目前,可能发生两种不同的错误:
///
/// * 当尝试执行计时器操作但计时器实例已被删除时,会发生 `shutdown`.
///   在这种情况下,操作将永远无法完成,并返回 `shutdown` 错误.
///   这是一个永久性错误,即一旦观察到此错误,计时器操作将永远不会成功.
///
/// * 当尝试执行计时器操作但计时器实例当前正在处理其最大数量的未完成睡眠实例时,会发生 `at_capacity`.
///   在这种情况下,操作无法在当前时刻执行,并返回 `at_capacity`.
///   这是一个暂时性错误,即在未来的某个时间点,如果再次尝试操作,它可能会成功.
///
#[derive(Debug, Copy, Clone)]
pub struct Error(Kind);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Kind {
    Shutdown = 1,
    AtCapacity = 2,
    Invalid = 3,
}

impl From<Kind> for Error {
    fn from(k: Kind) -> Self {
        Error(k)
    }
}

/// Errors returned by `Timeout`.
///
/// This error is returned when a timeout expires before the function was able
/// to finish.
/// `Timeout` 返回的错误.
#[derive(Debug, PartialEq, Eq)]
pub struct Elapsed(());

#[derive(Debug)]
pub(crate) enum InsertError {
    Elapsed,
}

// ===== impl Error =====

impl Error {
    /// Creates an error representing a shutdown timer.
    /// 创建一个代表关闭计时器的错误.
    pub fn shutdown() -> Error {
        Error(Kind::Shutdown)
    }

    /// Returns `true` if the error was caused by the timer being shutdown.
    /// 如果错误是由计时器关闭引起的,则返回`true`.
    pub fn is_shutdown(&self) -> bool {
        matches!(self.0, Kind::Shutdown)
    }

    /// Creates an error representing a timer at capacity.
    /// 创建计数器容量错误
    pub fn at_capacity() -> Error {
        Error(Kind::AtCapacity)
    }

    /// Returns `true` if the error was caused by the timer being at capacity.
    /// 如果错误是由计时器关闭引起的,则返回`true`.
    pub fn is_at_capacity(&self) -> bool {
        matches!(self.0, Kind::AtCapacity)
    }

    /// Creates an error representing a misconfigured timer.
    /// 创建一个表示错误配置的计时器的错误.
    pub fn invalid() -> Error {
        Error(Kind::Invalid)
    }

    /// Returns `true` if the error was caused by the timer being misconfigured.
    /// 如果错误是由于计时器配置错误导致的,则返回`true`.
    pub fn is_invalid(&self) -> bool {
        matches!(self.0, Kind::Invalid)
    }
}

impl error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let descr = match self.0 {
            Kind::Shutdown => {
                "the timer is shutdown, must be called from the context of Tokio runtime"
            }
            Kind::AtCapacity => "timer is at capacity and cannot create a new entry",
            Kind::Invalid => "timer duration exceeds maximum duration",
        };
        write!(fmt, "{}", descr)
    }
}

// ===== impl Elapsed =====

impl Elapsed {
    pub(crate) fn new() -> Self {
        Elapsed(())
    }
}

impl fmt::Display for Elapsed {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        "deadline has elapsed".fmt(fmt)
    }
}

impl std::error::Error for Elapsed {}

impl From<Elapsed> for std::io::Error {
    fn from(_err: Elapsed) -> std::io::Error {
        std::io::ErrorKind::TimedOut.into()
    }
}
