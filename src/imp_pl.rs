use std::{
    cell::UnsafeCell,
    panic::{RefUnwindSafe, UnwindSafe},
    sync::atomic::{AtomicIsize, Ordering},
};

use parking_lot::{lock_api::RawMutex as _RawMutex, RawMutex};

pub(crate) struct OnceCell<T> {
    mutex: Mutex,
    pub(crate) state: AtomicIsize,
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

const EMPTY: isize = -1;

impl<T> OnceCell<T> {
    pub(crate) const fn new() -> OnceCell<T> {
        OnceCell {
            mutex: Mutex::new(),
            state: AtomicIsize::new(EMPTY),
            value: UnsafeCell::new(None),
        }
    }

    /// Safety: synchronizes with store to value via `state` or mutex
    /// lock/unlock, writes value only once because of the mutex.
    #[cold]
    pub(crate) fn initialize<F, E>(&self, f: F) -> Result<isize, E>
    where
        F: FnOnce() -> Result<T, E>,
    {
        let _guard = self.mutex.lock();
        let mut state = self.state.load(Ordering::Relaxed);
        if state < 0 {
            // We are calling user-supplied function and need to be careful.
            // - if it returns Err, we unlock mutex and return without touching anything
            // - if it panics, we unlock mutex and propagate panic without touching anything
            // - if it calls `set` or `get_or_try_init` re-entrantly, we get a deadlock on
            //   mutex, which is important for safety. We *could* detect this and panic,
            //   but that is more complicated
            // - finally, if it returns Ok, we store the value and store the flag with
            //   `Release`, which synchronizes with `Acquire`s.
            let value = f()?;
            let slot: &mut Option<T> = unsafe { &mut *self.value.get() };
            debug_assert!(slot.is_none());
            *slot = Some(value);
            let offset = ((slot as *const _ as usize) - (self as *const _ as usize)) as isize;
            self.state.store(offset, Ordering::Release);
            state = offset;
        }
        Ok(state)
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
#[cfg(target_pointer_width = "64")]
fn test_size() {
    use std::mem::size_of;

    assert_eq!(size_of::<OnceCell<u32>>(), 3 * size_of::<u32>());
}
