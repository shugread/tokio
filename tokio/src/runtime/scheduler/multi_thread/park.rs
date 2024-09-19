//! Parks the runtime.
//!
//! A combination of the various resource driver park handles.

use crate::loom::sync::atomic::AtomicUsize;
use crate::loom::sync::{Arc, Condvar, Mutex};
use crate::runtime::driver::{self, Driver};
use crate::util::TryLock;

use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

#[cfg(loom)]
use crate::runtime::park::CURRENT_THREAD_PARK_COUNT;

// 负责阻塞当前线程,直到有任务需要处理或某个超时时间到达
pub(crate) struct Parker {
    inner: Arc<Inner>,
}

// 在任务到达时唤醒被阻塞的线程
pub(crate) struct Unparker {
    inner: Arc<Inner>,
}

/// Parker和Unparker的共享数据
struct Inner {
    /// Avoids entering the park if possible
    /// 状态
    state: AtomicUsize,

    /// Used to coordinate access to the driver / `condvar`
    /// 用于协调对 driver 和 condvar 的访问
    mutex: Mutex<()>,

    /// `Condvar` to block on if the driver is unavailable.
    /// 如果 driver 不可用时,使用 condvar 来阻塞线程
    condvar: Condvar,

    /// Resource (I/O, time, ...) driver
    /// 资源驱动器,例如 I/O 和计时器
    shared: Arc<Shared>,
}

const EMPTY: usize = 0; // 线程处于未阻塞状态
const PARKED_CONDVAR: usize = 1; // 线程正在使用条件变量进行阻塞
const PARKED_DRIVER: usize = 2; // 线程正在使用驱动器阻塞
const NOTIFIED: usize = 3; // 线程已经被通知唤醒

/// Shared across multiple Parker handles
/// 多个 Parker 句柄共享
struct Shared {
    /// Shared driver. Only one thread at a time can use this
    /// 共享驱动程序,每次只有一个线程可以使用它
    driver: TryLock<Driver>,
}

impl Parker {
    pub(crate) fn new(driver: Driver) -> Parker {
        Parker {
            inner: Arc::new(Inner {
                state: AtomicUsize::new(EMPTY),
                mutex: Mutex::new(()),
                condvar: Condvar::new(),
                shared: Arc::new(Shared {
                    driver: TryLock::new(driver),
                }),
            }),
        }
    }

    pub(crate) fn unpark(&self) -> Unparker {
        Unparker {
            inner: self.inner.clone(),
        }
    }

    // 线程挂起
    pub(crate) fn park(&mut self, handle: &driver::Handle) {
        self.inner.park(handle);
    }

    // 带超时的挂起
    pub(crate) fn park_timeout(&mut self, handle: &driver::Handle, duration: Duration) {
        // Only parking with zero is supported...
        assert_eq!(duration, Duration::from_millis(0));

        if let Some(mut driver) = self.inner.shared.driver.try_lock() {
            driver.park_timeout(handle, duration);
        } else {
            // https://github.com/tokio-rs/tokio/issues/6536
            // Hacky, but it's just for loom tests. The counter gets incremented during
            // `park_timeout`, but we still have to increment the counter if we can't acquire the
            // lock.
            #[cfg(loom)]
            CURRENT_THREAD_PARK_COUNT.with(|count| count.fetch_add(1, SeqCst));
        }
    }

    pub(crate) fn shutdown(&mut self, handle: &driver::Handle) {
        self.inner.shutdown(handle);
    }
}

// Parker克隆时, 只有shared的数据共享
impl Clone for Parker {
    fn clone(&self) -> Parker {
        Parker {
            inner: Arc::new(Inner {
                state: AtomicUsize::new(EMPTY),
                mutex: Mutex::new(()),
                condvar: Condvar::new(),
                shared: self.inner.shared.clone(),
            }),
        }
    }
}

impl Unparker {
    // 唤醒线程
    pub(crate) fn unpark(&self, driver: &driver::Handle) {
        self.inner.unpark(driver);
    }
}

impl Inner {
    /// Parks the current thread for at most `dur`.
    fn park(&self, handle: &driver::Handle) {
        // If we were previously notified then we consume this notification and
        // return quickly.
        // 如果我们之前收到过通知,那么我们就会使用此通知并快速返回.
        if self
            .state
            .compare_exchange(NOTIFIED, EMPTY, SeqCst, SeqCst)
            .is_ok()
        {
            return;
        }

        if let Some(mut driver) = self.shared.driver.try_lock() {
            // 使用driver挂起
            self.park_driver(&mut driver, handle);
        } else {
            // 使用condvar挂起
            self.park_condvar();
        }
    }

    fn park_condvar(&self) {
        // Otherwise we need to coordinate going to sleep
        let mut m = self.mutex.lock();

        match self
            .state
            .compare_exchange(EMPTY, PARKED_CONDVAR, SeqCst, SeqCst)
        {
            Ok(_) => {}
            Err(NOTIFIED) => {
                // We must read here, even though we know it will be `NOTIFIED`.
                // This is because `unpark` may have been called again since we read
                // `NOTIFIED` in the `compare_exchange` above. We must perform an
                // acquire operation that synchronizes with that `unpark` to observe
                // any writes it made before the call to unpark. To do that we must
                // read from the write it made to `state`.
                let old = self.state.swap(EMPTY, SeqCst);
                debug_assert_eq!(old, NOTIFIED, "park state changed unexpectedly");

                return;
            }
            Err(actual) => panic!("inconsistent park state; actual = {}", actual),
        }

        loop {
            // condvar挂起
            m = self.condvar.wait(m).unwrap();

            if self
                .state
                .compare_exchange(NOTIFIED, EMPTY, SeqCst, SeqCst)
                .is_ok()
            {
                // got a notification
                return;
            }

            // spurious wakeup, go back to sleep
        }
    }

    // 使用driver挂起
    fn park_driver(&self, driver: &mut Driver, handle: &driver::Handle) {
        match self
            .state
            .compare_exchange(EMPTY, PARKED_DRIVER, SeqCst, SeqCst)
        {
            Ok(_) => {}
            Err(NOTIFIED) => {
                // We must read here, even though we know it will be `NOTIFIED`.
                // This is because `unpark` may have been called again since we read
                // `NOTIFIED` in the `compare_exchange` above. We must perform an
                // acquire operation that synchronizes with that `unpark` to observe
                // any writes it made before the call to unpark. To do that we must
                // read from the write it made to `state`.
                let old = self.state.swap(EMPTY, SeqCst);
                debug_assert_eq!(old, NOTIFIED, "park state changed unexpectedly");

                return;
            }
            Err(actual) => panic!("inconsistent park state; actual = {}", actual),
        }

        driver.park(handle);

        match self.state.swap(EMPTY, SeqCst) {
            NOTIFIED => {}      // got a notification, hurray!
            PARKED_DRIVER => {} // no notification, alas
            n => panic!("inconsistent park_timeout state: {}", n),
        }
    }

    // 唤醒
    fn unpark(&self, driver: &driver::Handle) {
        // To ensure the unparked thread will observe any writes we made before
        // this call, we must perform a release operation that `park` can
        // synchronize with. To do that we must write `NOTIFIED` even if `state`
        // is already `NOTIFIED`. That is why this must be a swap rather than a
        // compare-and-swap that returns if it reads `NOTIFIED` on failure.
        match self.state.swap(NOTIFIED, SeqCst) {
            EMPTY => {}    // no one was waiting
            NOTIFIED => {} // already unparked
            PARKED_CONDVAR => self.unpark_condvar(),
            PARKED_DRIVER => driver.unpark(),
            actual => panic!("inconsistent state in unpark; actual = {}", actual),
        }
    }

    // 使用condvar唤醒
    fn unpark_condvar(&self) {
        // There is a period between when the parked thread sets `state` to
        // `PARKED` (or last checked `state` in the case of a spurious wake
        // up) and when it actually waits on `cvar`. If we were to notify
        // during this period it would be ignored and then when the parked
        // thread went to sleep it would never wake up. Fortunately, it has
        // `lock` locked at this stage so we can acquire `lock` to wait until
        // it is ready to receive the notification.
        //
        // Releasing `lock` before the call to `notify_one` means that when the
        // parked thread wakes it doesn't get woken only to have to wait for us
        // to release `lock`.
        drop(self.mutex.lock());

        self.condvar.notify_one();
    }

    fn shutdown(&self, handle: &driver::Handle) {
        if let Some(mut driver) = self.shared.driver.try_lock() {
            driver.shutdown(handle);
        }

        self.condvar.notify_all();
    }
}
