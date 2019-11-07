use core::{
    sync::atomic::{spin_loop_hint, AtomicUsize, Ordering},
};

use crate::thread_parker::{Futex, RESERVED_MASK};

impl Futex for AtomicUsize {
    unsafe fn park<P>(&self, should_park: P)
    where
        P: Fn(usize) -> bool
    {
        while should_park(self.load(Ordering::Relaxed) & !RESERVED_MASK) {
            spin_loop_hint();
        }
    }

    unsafe fn store_and_unpark(&self, new: usize) {
        self.store(new, Ordering::Release); // FIXME: maybe SeqCst?
    }
}