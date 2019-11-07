use core::{
    ptr,
    sync::atomic::{AtomicU32, Ordering},
};
use syscall::{
    call,
    data::TimeSpec,
    error::{Error, EAGAIN, EFAULT, EINTR, ETIMEDOUT},
    flag::{FUTEX_WAIT, FUTEX_WAKE},
};

use crate::thread_parker::{Futex, as_u32_pub, RESERVED_MASK};

impl Futex for AtomicUsize {
    unsafe fn park<P>(&self, should_park: P)
    where
        P: Fn(usize) -> bool
    {
        // Redox futex takes an `i32` to compare if the thread should be parked.
        // convert our reference to `AtomicUsize` to an `*const i32`, pointing to the part
        // containing the non-reserved bits.
        let ptr = as_u32_pub(self) as *const i32;
        loop {
            let current = self.load(Ordering::Relaxed);
            if !should_park(current &! RESERVED_MASK) {
                break;
            }
            let compare = (current >> (8 * (mem::size_of::<usize>() - mem::size_of::<u32>()))) as u32;
            futex_wait(ptr, compare as i32, None);
        }
    }

    unsafe fn store_and_unpark(&self, new: usize) {
        self.store(new, Ordering::Release); // FIXME: maybe SeqCst?
        let ptr = as_u32_pub(self) as *const i32;
        futex_wake(ptr, i32::max_value());
    }
}

fn futex_wait(atomic: *const i32, current: i32, ts: Option<TimeSpec>) {
    let r = unsafe {
        call::futex(atomic, FUTEX_WAIT, current, ts_ptr as usize, ptr::null_mut())
    };
    match r {
        Ok(r) => debug_assert_eq!(r, 0),
        Err(Error { errno }) => {
            debug_assert!(errno == EINTR || errno == EAGAIN || errno == ETIMEDOUT);
        }
    }
}

fn futex_wake(atomic: *const i32, max_threads_to_wake: i32) {
    let r = unsafe {
        call::futex(atomic, FUTEX_WAKE, max_threads_to_wake, 0, ptr::null_mut())
    };
    match r {
        Ok(num_woken) =>  debug_assert!(num_woken <= max_threads_to_wake as usize),
        Err(Error { errno }) => debug_assert_eq!(errno, EFAULT),
    }
}
