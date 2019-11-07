use core::{
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};
use libc;

use crate::thread_parker::Futex;

impl Futex for AtomicUsize {
    type Int = usize;

    unsafe fn park<P, E, D>(&self, should_park: P, _encode_ptr: E, _decode_ptr: D)
    where
        P: Fn(Self::Int) -> bool,
        E: Fn(usize, usize) -> usize,
        D: Fn(usize) -> usize
    {
        loop {
            let current = self.load(Ordering::Relaxed);
            if !should_park(current) {
                break;
            }
            umtx_wait(&self, current);
        }
    }

    unsafe fn store_and_unpark<D>(&self, new: Self::Int, _decode_ptr: D)
    where
        D: Fn(usize) -> usize
    {
        self.store(new, Ordering::Release); // FIXME: maybe SeqCst?
        umtx_wake(&self, usize::max_value());
    }
}

const _UMTX_OP: i32 = 454; // FIXME: check
const UMTX_OP_WAIT: libc::c_int = 2;
const UMTX_OP_WAKE: libc::c_int = 3;

unsafe fn umtx_op(obj: *mut usize, // actually *mut libc::c_void
           op: libc::c_int,
           val: usize, // actually libc::c_ulong
           uaddr: *mut libc::c_void,
           uaddr2: *mut libc::c_void // *mut timespec or *mut _umtx_time
    ) -> libc::c_int
{
    libc::syscall(_UMTX_OP, obj, op, val, uaddr, uaddr2)
}

#[inline]
fn umtx_wait(atomic: &AtomicUsize, current: usize) {
    let ptr = atomic as *const AtomicUsize as *mut usize;
    let r = unsafe {
        umtx_op(
            ptr,
            UMTX_OP_WAIT,
            current,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    debug_assert!(r == 0 || r == -1);
    if r == -1 {
//        debug_assert!(errno() == libc::EINTR));
    }
}

fn umtx_wake(atomic: &AtomicUsize, max_threads_to_wake: usize) {
    let ptr = atomic as *const AtomicUsize as *mut usize;
    let r = unsafe {
        umtx_op(
            ptr,
            UMTX_OP_WAKE,
            max_threads_to_wake,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    debug_assert!(r == 0 || r == -1);
    if r == -1 {
//        debug_assert!(errno() == libc::EINTR));
    }
}
