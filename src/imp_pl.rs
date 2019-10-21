use std::{
    cell::UnsafeCell,
    panic::{RefUnwindSafe, UnwindSafe},
    sync::atomic::{AtomicUsize, Ordering},
    mem,
};

use parking_lot::{lock_api::RawMutex as _RawMutex, RawMutex};

pub(crate) struct OnceCell<T> {
    mutex: ReentrantMutex,
    state: AtomicUsize,
    pub(crate) value: UnsafeCell<Option<T>>,
}

// Why do we need `T: Send`?
// Thread A creates a `OnceCell` and shares it with
// scoped thread B, which fills the cell, which is
// then destroyed by A. That is, destructor observes
// a sent value.
unsafe impl<T: Sync + Send> Sync for OnceCell<T> {}
unsafe impl<T: Send> Send for OnceCell<T> {}

impl<T: RefUnwindSafe + UnwindSafe> RefUnwindSafe for OnceCell<T> {}
impl<T: UnwindSafe> UnwindSafe for OnceCell<T> {}

// Three states that a OnceCell can be in.
const EMPTY: usize = 0x2;
const RUNNING: usize = 0x1;
const COMPLETE: usize = 0x0;

impl<T> OnceCell<T> {
    pub(crate) const fn new() -> OnceCell<T> {
        OnceCell {
            mutex: ReentrantMutex::new(),
            state: AtomicUsize::new(EMPTY),
            value: UnsafeCell::new(None),
        }
    }

    /// Safety: synchronizes with store to value via Release/Acquire.
    #[inline]
    pub(crate) fn is_initialized(&self) -> bool {
        self.state.load(Ordering::Acquire) == COMPLETE
    }

    /// Safety: synchronizes with store to value via `state` or mutex
    /// lock/unlock, writes value only once because of the mutex.
    #[cold]
    pub(crate) fn initialize<F, E>(&self, f: F) -> Result<(), E>
    where
        F: FnOnce() -> Result<T, E>,
    {
        if self.is_initialized() {
            return Ok(());
        }
        // Lock a mutex so other threads can wait until we are ready.
        let _guard = self.mutex.lock();
        let old = self.state.compare_and_swap(EMPTY, RUNNING, Ordering::Relaxed);
        match old {
            EMPTY => {
                // This thread is the only one currently trying to initialize the Cell,
                // and we just set `state` to RUNNING.
                let reset_if_failed = store_on_drop(&self.state, EMPTY, Ordering::Relaxed);
                let value = f()?;
                let slot: &mut Option<T> = unsafe { &mut *self.value.get() };
                debug_assert!(slot.is_none());
                *slot = Some(value);
                reset_if_failed.cancel();
                self.state.store(COMPLETE, Ordering::Release);
            }
            RUNNING => {
                // We were able to lock the ReentrantMutex wile an initializer is running.
                // This must be a case of reentrant initialization.
                panic!();
            }
            COMPLETE => {} // Another thread must have finished initializing the Cell just now.
        }
        Ok(())
    }
}

// Helper struct used to clean up after a closure call with a `Drop`
// implementation to also run on panic.
struct StoreOnDrop<'a> {
    atomic: &'a AtomicUsize,
    value: usize,
    ordering: Ordering,
}

fn store_on_drop<'a>(atomic: &'a AtomicUsize, value: usize, ordering: Ordering) -> StoreOnDrop<'a> {
    StoreOnDrop {
        atomic,
        value,
        ordering,
    }
}

impl StoreOnDrop<'_> {
    fn cancel(self) {
        mem::forget(self)
    }
}

impl Drop for StoreOnDrop<'_> {
    fn drop(&mut self) {
        self.atomic.store(self.value, self.ordering)
    }
}

/// Wrapper around parking_lot's `RawMutex` which has `const fn` new.
struct Mutex {
    inner: RawMutex,
}

impl Mutex {
    const fn new() -> Mutex {
        Mutex { inner: RawMutex::INIT }
    }

    fn lock(&self) -> MutexGuard<'_> {
        self.inner.lock();
        MutexGuard { inner: &self.inner }
    }
}

struct MutexGuard<'a> {
    inner: &'a RawMutex,
}

impl Drop for MutexGuard<'_> {
    fn drop(&mut self) {
        self.inner.unlock();
    }
}

#[test]
#[cfg(pointer_width = "64")]
fn test_size() {
    use std::mem::size_of;

    assert_eq!(size_of::<OnceCell<u32>>, 2 * size_of::<u32>);
}
