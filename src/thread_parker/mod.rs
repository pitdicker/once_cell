// TODO:
// Fuchsia: https://fuchsia.dev/fuchsia-src/concepts/objects/futex
//          https://docs.rs/fuchsia-zircon-sys/0.3.3/fuchsia_zircon_sys/fn.zx_futex_wait.html
// OpenBSD: futex
// SGX: ...

use std::{
    mem,
    sync::atomic::{AtomicUsize},
};

#[cfg(windows)]
mod windows;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;

#[cfg(target_os = "freebsd")]
mod freebsd;

#[cfg(all(unix,
          not(any(target_os = "linux", target_os = "android", target_os = "freebsd"))))]
mod posix;

#[cfg(any(target_os = "redox"))]
mod redox;

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
mod wasm_atomic;

#[cfg(not(any(windows,
              unix,
              target_os = "linux",
              target_os = "freebsd",
              target_os = "android",
              target_os = "redox",
              all(target_arch = "wasm32", target_feature = "atomics")
    )))]
mod spin_loop;

pub trait Futex {
    // Park the current thread if `should_park` returns `true`. Reparks after a spurious wakeup.
    //
    // `should_park` is provided the current value of `self & !RESERVED_MASK`, and should decide if
    // this thread needs to be parked. This avoids a race condition while trying to park, and allows
    // reparking on spurious wakeups.
    //
    // # Safety
    // * After a spurious wakeup `should_park` MUST detect `store_and_unpark` was not called, and
    //   return `false`.
    unsafe fn park<P>(&self, should_park: P)
    where
        P: Fn(usize) -> bool;

    // Unpark all waiting threads. `new` must be provided set `self` to some value that is not
    // matched by the `should_park` function passed to `park`.
    //
    // # Safety
    // * Don't change any of the reserved bits while there can be threads parked. can cause
    unsafe fn store_and_unpark(&self, new: usize);
}

pub trait ParkerWithState {
    fn new() -> Self;

    fn park(&self);

    fn unpark(&self);
}

pub const FREE_BITS: usize = 5;
pub const RESERVED_BITS: usize = mem::size_of::<usize>() * 8 - FREE_BITS;
pub const RESERVED_MASK: usize = (1 << RESERVED_BITS) - 1;

// Convert this pointer to an `AtomicUsize` to a pointer to an `*const u32`, pointing to the part
// containing the non-reserved bits.
#[allow(unused)]
#[cfg(all(target_pointer_width="64", target_endian="little"))]
pub(crate) fn as_u32_pub(ptr: *const AtomicUsize) -> *const u32 {
    unsafe { (ptr as *const _ as *const u32).offset(1) }
}

// Convert this pointer to an `AtomicUsize` to a pointer to an `*const u32`, pointing to the part
// containing the non-reserved bits.
#[allow(unused)]
#[cfg(any(target_pointer_width="32",
          all(target_pointer_width="64", target_endian="big")))]
pub(crate) fn as_u32_pub(ptr: *const AtomicUsize) -> *const u32 {
    ptr as *const _ as *const u32
}

// Convert this pointer to an `AtomicUsize` to a pointer to an `*const u32`, pointing to the part
// containing only reserved bits.
#[allow(unused)]
#[cfg(all(target_pointer_width="64", target_endian="little"))]
pub(crate) fn as_u32_priv(ptr: *const AtomicUsize) -> *const u32 {
    ptr as *const _ as *const u32
}

// Convert this pointer to an `AtomicUsize` to a pointer to an `*const u32`, pointing to the part
// containing only reserved bits.
#[allow(unused)]
#[cfg(any(target_pointer_width="32",
          all(target_pointer_width="64", target_endian="big")))]
pub(crate) fn as_u32_priv(ptr: *const AtomicUsize) -> *const u32 {
    unsafe { (ptr as *const _ as *const u32).offset(1) }
}