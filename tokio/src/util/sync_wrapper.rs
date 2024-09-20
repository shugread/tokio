//! This module contains a type that can make `Send + !Sync` types `Sync` by
//! disallowing all immutable access to the value.
//!
//! A similar primitive is provided in the `sync_wrapper` crate.
//! 该模块包含一种类型,可以通过禁止对值的所有不可变访问,使`Send + !Sync`类型变为`Sync`.

use std::any::Any;

pub(crate) struct SyncWrapper<T> {
    value: T,
}

// safety: The SyncWrapper being send allows you to send the inner value across
// thread boundaries.
unsafe impl<T: Send> Send for SyncWrapper<T> {}

// safety: An immutable reference to a SyncWrapper is useless, so moving such an
// immutable reference across threads is safe.
unsafe impl<T> Sync for SyncWrapper<T> {}

impl<T> SyncWrapper<T> {
    pub(crate) fn new(value: T) -> Self {
        Self { value }
    }

    pub(crate) fn into_inner(self) -> T {
        self.value
    }
}

impl SyncWrapper<Box<dyn Any + Send>> {
    /// Attempt to downcast using `Any::downcast_ref()` to a type that is known to be `Sync`.
    /// 尝试使用`Any::downcast_ref()`向下转换为已知为`Sync`的类型.
    pub(crate) fn downcast_ref_sync<T: Any + Sync>(&self) -> Option<&T> {
        // SAFETY: if the downcast fails, the inner value is not touched,
        // so no thread-safety violation can occur.
        self.value.downcast_ref()
    }
}
