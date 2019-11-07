use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crate::thread_parker::{Futex, ParkerWithState, FREE_BITS, RESERVED_MASK};

#[repr(align(8))] // Leave the three lower bits are free to use for other purposes.
struct Waiter {
    parker: PosixParker,
    next: usize,
}

impl Futex for AtomicUsize {
    unsafe fn park<P>(&self, should_park: P)
    where
        P: Fn(usize) -> bool
    {
        let mut current = self.load(Ordering::Relaxed);
        loop {
            if !should_park(current & !RESERVED_MASK) {
                break;
            }
            // Create a node for our current thread.
            let node = Waiter {
                parker: PosixParker::new(),
                next: current,
            };
            let me = (current & !RESERVED_MASK) | ((&node as *const Waiter as usize) >> FREE_BITS);

            // Try to slide in the node at the head of the linked list, making sure
            // that another thread didn't just replace the head of the linked list.
            let old = self.compare_and_swap(current, me, Ordering::Release);
            if old != current {
                current = old;
                continue;
            }

            // We have enqueued ourselves, now lets wait.
            // The parker will not park our thread if we got unparked just now.
            node.parker.park();
        }
    }

    unsafe fn store_and_unpark(&self, new: usize) {
        let queue = self.swap(new, Ordering::AcqRel); // FIXME: maybe SeqCst?

        // Walk the entire linked list of waiters and wake them up (in lifo
        // order, last to register is first to wake up).
        let mut next = ((queue & RESERVED_MASK) << FREE_BITS) as *const Waiter;
        while !next.is_null() {
            let current = next;
            next = (((*current).next & RESERVED_MASK) << FREE_BITS) as *const Waiter;
            (*current).parker.unpark();
        }
    }
}


// `UnsafeCell` because Posix needs mutable references to these types.
pub struct PosixParker {
    mutex: UnsafeCell<libc::pthread_mutex_t>,
    condvar: UnsafeCell<libc::pthread_cond_t>,
    should_park: AtomicBool,
}

impl ParkerWithState for PosixParker {
    fn new() -> Self {
        Self {
            mutex: UnsafeCell::new(libc::PTHREAD_MUTEX_INITIALIZER),
            condvar: UnsafeCell::new(libc::PTHREAD_COND_INITIALIZER),
            should_park: AtomicBool::new(true),
        }
    }

    #[inline]
    fn park(&self) {
        unsafe {
            let r = libc::pthread_mutex_lock(self.mutex.get());
            debug_assert_eq!(r, 0);
            while self.should_park.load(Ordering::Relaxed) {
                let r = libc::pthread_cond_wait(self.condvar.get(), self.mutex.get());
                debug_assert_eq!(r, 0);
            }
            let r = libc::pthread_mutex_unlock(self.mutex.get());
            debug_assert_eq!(r, 0);
        }
    }

    #[inline]
    fn unpark(&self) {
        unsafe {
            let r = libc::pthread_mutex_lock(self.mutex.get());
            debug_assert_eq!(r, 0);
            self.should_park.store(false, Ordering::Relaxed);
            let r = libc::pthread_cond_signal(self.condvar.get());
            debug_assert_eq!(r, 0);
            let r = libc::pthread_mutex_unlock(self.mutex.get());
            debug_assert_eq!(r, 0);
        }
    }
}


impl Drop for PosixParker {
    #[inline]
    fn drop(&mut self) {
        // On DragonFly pthread_mutex_destroy() returns EINVAL if called on a
        // mutex that was just initialized with libc::PTHREAD_MUTEX_INITIALIZER.
        // Once it is used (locked/unlocked) or pthread_mutex_init() is called,
        // this behaviour no longer occurs. The same applies to condvars.
        unsafe {
            let r = libc::pthread_mutex_destroy(self.mutex.get());
            if cfg!(target_os = "dragonfly") {
                debug_assert!(r == 0 || r == libc::EINVAL);
            } else {
                debug_assert_eq!(r, 0);
            }
            let r = libc::pthread_cond_destroy(self.condvar.get());
            if cfg!(target_os = "dragonfly") {
                debug_assert!(r == 0 || r == libc::EINVAL);
            } else {
                debug_assert_eq!(r, 0);
            }
        }
    }
}
