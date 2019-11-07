use core::{
    mem,
    arch::wasm32,
    sync::atomic::{AtomicU32, Ordering},
};

use crate::thread_parker::{Futex, as_u32_pub, RESERVED_MASK};

// Documentation copied from the standard library, see also the [announcement].

/// Use the proposed wasm's [`i32.atomic.wait` instruction][instr].
///
/// This function, when called, will block the current thread if the memory
/// pointed to by `ptr` is equal to `expression` (performing this action
/// atomically).
///
/// The argument `timeout_ns` is a maxinum number of nanoseconds the calling
/// thread will be blocked for, if it blocks. If the timeout is negative then
/// the calling thread will be blocked forever.
///
/// The calling thread can only be woken up with a call to the `wake` intrinsic
/// once it has been blocked. Changing the memory behind `ptr` will not wake
/// the thread once it's blocked.
///
/// # Return value
///
/// * 0 - indicates that the thread blocked and then was woken up
/// * 1 - the loaded value from `ptr` didn't match `expression`, the thread
///   didn't block
/// * 2 - the thread blocked, but the timeout expired.
///
/// # Availability
///
/// This intrinsic is only available **when the standard library itself is
/// compiled with the `atomics` target feature**. This version of the standard
/// library is not obtainable via `rustup`, but rather will require the
/// standard library to be compiled from source.
///
/// [instr]: https://github.com/WebAssembly/threads/blob/master/proposals/threads/Overview.md#wait
/// [announcement]: https://rustwasm.github.io/2018/10/24/multithreading-rust-and-wasm.html
impl Futex for AtomicUsize {
    unsafe fn park<P>(&self, should_park: P)
    where
        P: Fn(usize) -> bool
    {
        let ptr = as_u32_pub(self) as *const i32;
        loop {
            let current = self.load(Ordering::Relaxed);
            if !should_park(current & !RESERVED_MASK) {
                break;
            }
            let compare = (current >> (8 * (mem::size_of::<usize>() - mem::size_of::<u32>()))) as u32;
            let r = wasm32::i32_atomic_wait(ptr, compare as i32, -1);
            // We ignore the return value, and decide whether to park again based on the value of
            // `self`.
            debug_assert!(r == 0 || r == 1 || r == 2);
        }
    }

    unsafe fn store_and_unpark(&self, new: usize) {
        self.store(new, Ordering::Release); // FIXME: maybe SeqCst?
        let ptr = as_u32_pub(self) as *const i32;
        wasm32::atomic_notify(ptr, u32::max_value());
    }
}
