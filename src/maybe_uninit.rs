//! This module contains a re-export or vendored version of `core::mem::MaybeUninit` depending
//! on which Rust version it's compiled for.
//!
//! Remove this module and use `core::mem::MaybeUninit` directly when dropping support for <1.36

/// This is a terrible imitation of `core::mem::MaybeUninit` to support Rust older than 1.36.
/// Differences from the real deal:
/// - We drop the values contained in `MaybeUninit`, while that has to be done manually otherwise;
/// - We use more memory;
/// - `as_mut_ptr()` can't be used to initialize the `MaybeUninit`.

#[cfg(maybe_uninit)]
pub use core::mem::MaybeUninit;

#[cfg(not(maybe_uninit))]
use core::hint::unreachable_unchecked;

#[cfg(not(maybe_uninit))]
pub struct MaybeUninit<T>(Option<T>);

impl<T> MaybeUninit<T> {
    #[inline]
    pub const fn uninit() -> MaybeUninit<T> {
        MaybeUninit(None)
    }

    #[inline]
    pub fn as_ptr(&self) -> *const T {
        match self.0.as_ref() {
            Some(value) => value,
            None => {
                // This unsafe does improve performance, see `examples/bench`.
                debug_assert!(false);
                unsafe { unreachable_unchecked() }
            }
        }
    }

    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.0.as_mut().unwrap()
    }

    // It would be better to use `as_mut_ptr().write()`, but that can't be emulated with `Option`.
    #[inline]
    pub fn write(&mut self, val: T) -> &mut T {
        self.0 = Some(val);
        self.0.as_mut().unwrap()
    }

    #[inline]
    pub unsafe fn assume_init(self) -> T {
        self.0.unwrap()
    }
}
