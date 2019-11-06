use core::{
    mem,
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::thread_parker::{Futex, as_u32_pub, RESERVED_MASK};

impl Futex for AtomicUsize {
    unsafe fn park<P>(&self, should_park: P)
    where
        P: Fn(usize) -> bool
    {
        // Linux futex takes an `i32` to compare if the thread should be parked.
        // convert our reference to `AtomicUsize` to an `*const i32`, pointing to the part
        // containing the non-reserved bits.
        let ptr = as_u32_pub(self) as *const i32;
        loop {
            let current = self.load(Ordering::Relaxed);
            if !should_park(current & !RESERVED_MASK) {
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

fn errno() -> libc::c_int {
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location()
    }
    #[cfg(target_os = "android")]
    unsafe {
        *libc::__errno()
    }
}

#[inline]
fn futex_wait(futex: *const i32, current: i32, ts: Option<libc::timespec>) {
    let ts_ptr = ts
        .as_ref()
        .map(|ts_ref| ts_ref as *const _)
        .unwrap_or(ptr::null());
    let r = unsafe {
        libc::syscall(
            libc::SYS_futex,
            futex,
            libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
            current,
            ts_ptr,
        )
    };
    debug_assert!(r == 0 || r == -1);
    if r == -1 {
        debug_assert!(
            errno() == libc::EINTR
                || errno() == libc::EAGAIN
                || (ts.is_some() && errno() == libc::ETIMEDOUT)
        );
    }
}

fn futex_wake(futex: *const i32, max_threads_to_wake: i32) {
    let r = unsafe {
        libc::syscall(
            libc::SYS_futex,
            futex,
            libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
            max_threads_to_wake,
        )
    };
    debug_assert!((r >= 0 && r <= max_threads_to_wake as i64) || r == -1);
    if r == -1 {
        debug_assert_eq!(errno(), libc::EFAULT);
    }
}
