// There's a lot of scary concurrent code in this module, but it is copied from
// `std::sync::Once` with two changes:
//   * no poisoning
//   * init function can fail

use std::{
    cell::{Cell, UnsafeCell},
    marker::PhantomData,
    mem::ManuallyDrop,
    panic::{RefUnwindSafe, UnwindSafe},
    ptr,
    sync::atomic::{AtomicBool, AtomicIsize, Ordering},
    thread::{self, Thread},
};
use crate::maybe_uninit::MaybeUninit;

pub(crate) struct OnceCell<T> {
    // This `state` word is actually an encoded version of just a pointer to a
    // `Waiter`, so we add the `PhantomData` appropriately.
    // >= 0 INITIALIZED (offset to data)
    //       -1 EMPTY
    //       -2 RUNNING
    //     < -2 (encoded waiter pointer, -(waiterptr >> 1))
    state: AtomicIsize,
    _marker: PhantomData<*const Waiter>,
    pub(crate) value: UnsafeCell<MaybeUninit<T>>,
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

// States that a OnceCell can be in before in is initialized.
const EMPTY: isize = -1;
const RUNNING: isize = -2; // and less

// Representation of a node in the linked list of waiters, used while in the
// RUNNING state.
// Note: `Waiter` can't hold a mutable pointer to the next thread, because then
// we would both hand out a mutable reference to its `Waiter` node, and keep a
// shared reference to check `signaled`. Instead we use shared references and
// interior mutability.
#[repr(align(8))]
struct Waiter {
    thread: Cell<Option<Thread>>,
    signaled: AtomicBool,
    next: *const Waiter,
}

// Helper struct used to clean up after a closure call with a `Drop`
// implementation to also run on panic.
struct Finish<'a> {
    set_value_on_drop_to: isize,
    my_state: &'a AtomicIsize,
}

impl<T> OnceCell<T> {
    pub(crate) const fn new() -> OnceCell<T> {
        OnceCell {
            state: AtomicIsize::new(EMPTY),
            _marker: PhantomData,
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Safety: does no synchronization, only checks whether the `OnceCell` is initialized.
    #[inline]
    pub(crate) fn is_initialized(&self) -> bool {
        self.state.load(Ordering::Relaxed) >= 0
    }

    /// Safety: synchronizes with store to value via Release/Acquire.
    #[inline]
    pub(crate) fn sync_get(&self) -> Option<&T> {
        // Use the atomic Consume ordering to synchronize `value`.
        let state = self.state.load(Ordering::Relaxed);
        if state >= 0 {
            unsafe { Some(self.consume(state as usize)) }
        } else {
            None
        }
    }

    #[inline]
    unsafe fn consume(&self, offset: usize) -> &T {
        let ptr = ((self as *const _ as usize) + offset) as *const UnsafeCell<MaybeUninit<T>>;
        let slot: &MaybeUninit<T> = &*(*ptr).get();
        &*slot.as_ptr()
    }

    /// Safety: synchronizes with store to value via SeqCst read from state,
    /// writes value only once because we never get to INCOMPLETE state after a
    /// successful write.
    #[cold]
    pub(crate) fn initialize<F, E>(&self, f: F) -> Result<&T, E>
    where
        F: FnOnce() -> Result<T, E>,
    {
        let mut f = Some(f);
        let mut res: Result<(), E> = Ok(());
        let slot = &self.value;
        let offset = ((slot as *const _ as usize) - (self as *const _ as usize)) as isize;
        initialize_inner(&self.state, offset, &mut || {
            let f = f.take().unwrap();
            match f() {
                Ok(value) => {
                    unsafe {
                        let slot: &mut MaybeUninit<T> = &mut *slot.get();
                        // FIXME: replace with `slot.as_mut_ptr().write(value)`
                        slot.write(value);
                    }
                    true
                }
                Err(e) => {
                    res = Err(e);
                    false
                }
            }
        });
        res?;
        unsafe {
            let state = self.state.load(Ordering::Relaxed);
            debug_assert!(state >= 0);
            Ok(self.consume(state as usize))
        }
    }
}

// Note: this is intentionally monomorphic
fn initialize_inner(my_state: &AtomicIsize, offset: isize, init: &mut dyn FnMut() -> bool) {
    // This cold path uses SeqCst consistently because the
    // performance difference really does not matter there, and
    // SeqCst minimizes the chances of something going wrong.
    let mut state = my_state.load(Ordering::Relaxed);

    loop {
        if state >= 0 {
            // If we're complete, then there's nothing to do, we just
            // jettison out as we shouldn't run the closure.
            return;
        } else if state == EMPTY {
            // Otherwise if we see an incomplete state we will attempt to
            // move ourselves into the RUNNING state. If we succeed, then
            // the queue of waiters starts at null (all 0 bits).
            let old = my_state.compare_and_swap(state, RUNNING, Ordering::Relaxed);
            if old != state {
                state = old;
                continue;
            }

            // Run the initialization routine, letting it know if we're
            // poisoned or not. The `Finish` struct is then dropped, and
            // the `Drop` implementation here is responsible for waking
            // up other waiters both in the normal return and panicking
            // case.
            let mut complete = Finish { set_value_on_drop_to: EMPTY, my_state };
            let success = init();
            // Difference from std: abort if `init` errored.
            if success {
                complete.set_value_on_drop_to = offset;
            }
            return;
        } else {
            // Create the node for our current thread that we are going to try to slot
            // in at the head of the linked list.
            //
            // We only drop our node if we didn't end up putting it in the queue.
            // After it is enqueued, the thread that wakes us will have taken out `thread` and
            // will take care of dropping it.
            // So there is nothing to clean up after enqueueing this thread. Not dropping it then
            // means there are no reads from an modified (now empty) `thread` field, and we don't
            // have to do an `acquire` on `signaled`.
            let mut node = ManuallyDrop::new(Waiter {
                thread: Cell::new(Some(thread::current())),
                signaled: AtomicBool::new(false),
                next: ptr::null_mut(),
            });
            let me = -(((&node as *const _ as usize) >> 1) as isize);

            // Try to slide in the node at the head of the linked list.
            // Run in a loop where we make sure the status is still RUNNING, and that
            // another thread did not just replace the head of the linked list.
            loop {
                if !(state <= RUNNING) {
                    // No need anymore to enqueue ourselves.
                    ManuallyDrop::into_inner(node); // drop
                    return;
                }

                if state != RUNNING {
                    // There already is a queue of waiters. Decode the pointer and add it as our
                    // next.
                    node.next = (((-state) as usize) << 1) as *const Waiter;
                }
                let old = my_state.compare_and_swap(state, me, Ordering::Release);
                if old == state {
                    break; // Success!
                }
                state = old;
            }

            // We have enqueued ourselves, now lets wait.
            // It is important not to return before being signaled, otherwise we would
            // drop our `Waiter` node and leave a hole in the linked list (and a
            // dangling reference). Guard against spurious wakeups by reparking
            // ourselves until we are signaled.
            while !node.signaled.load(Ordering::Relaxed) {
                thread::park();
            }
            state = my_state.load(Ordering::Relaxed);
        }
    }
}

impl Drop for Finish<'_> {
    fn drop(&mut self) {
        // Swap out our state with however we finished. We should only ever see
        // an old state which was RUNNING.
        let queue = self.my_state.swap(self.set_value_on_drop_to, Ordering::Release);
        assert!(queue <= RUNNING);

        // Decode the RUNNING to a list of waiters, then walk that entire list
        // and wake them up. Note that it is crucial that after we store `true`
        // in the node it can be free'd! As a result we load the `thread` to
        // signal ahead of time and then unpark it after the store.
        unsafe {
            if queue != RUNNING {
                let mut queue = ((-queue as usize) << 1) as *const Waiter;
                while !queue.is_null() {
                    let next = (*queue).next;
                    let thread = (*queue).thread.take().unwrap();
                    (*queue).signaled.store(true, Ordering::Release);
                    // `unpark` does an atomic operation that prevents it from getting reordered.
                    thread.unpark();
                    queue = next;
                }
            }
        }
    }
}

// These test are snatched from std as well.
#[cfg(test)]
mod tests {
    use std::panic;
    #[cfg(not(miri))] // miri doesn't support threads
    use std::{sync::mpsc::channel, thread};

    use super::OnceCell;

    impl<T> OnceCell<T> {
        fn init(&self, f: impl FnOnce() -> T) {
            enum Void {}
            let _ = self.initialize(|| Ok::<T, Void>(f()));
        }
    }

    #[test]
    fn smoke_once() {
        static O: OnceCell<()> = OnceCell::new();
        let mut a = 0;
        O.init(|| a += 1);
        assert_eq!(a, 1);
        O.init(|| a += 1);
        assert_eq!(a, 1);
    }

    #[test]
    #[cfg(not(miri))] // miri doesn't support threads
    fn stampede_once() {
        static O: OnceCell<()> = OnceCell::new();
        static mut RUN: bool = false;

        let (tx, rx) = channel();
        for _ in 0..10 {
            let tx = tx.clone();
            thread::spawn(move || {
                for _ in 0..4 {
                    thread::yield_now()
                }
                unsafe {
                    O.init(|| {
                        assert!(!RUN);
                        RUN = true;
                    });
                    assert!(RUN);
                }
                tx.send(()).unwrap();
            });
        }

        unsafe {
            O.init(|| {
                assert!(!RUN);
                RUN = true;
            });
            assert!(RUN);
        }

        for _ in 0..10 {
            rx.recv().unwrap();
        }
    }

    #[test]
    #[cfg(not(miri))] // miri doesn't support panics
    fn poison_bad() {
        static O: OnceCell<()> = OnceCell::new();

        // poison the once
        let t = panic::catch_unwind(|| {
            O.init(|| panic!());
        });
        assert!(t.is_err());

        // we can subvert poisoning, however
        let mut called = false;
        O.init(|| {
            called = true;
        });
        assert!(called);

        // once any success happens, we stop propagating the poison
        O.init(|| {});
    }

    #[test]
    #[cfg(not(miri))] // miri doesn't support panics
    fn wait_for_force_to_finish() {
        static O: OnceCell<()> = OnceCell::new();

        // poison the once
        let t = panic::catch_unwind(|| {
            O.init(|| panic!());
        });
        assert!(t.is_err());

        // make sure someone's waiting inside the once via a force
        let (tx1, rx1) = channel();
        let (tx2, rx2) = channel();
        let t1 = thread::spawn(move || {
            O.init(|| {
                tx1.send(()).unwrap();
                rx2.recv().unwrap();
            });
        });

        rx1.recv().unwrap();

        // put another waiter on the once
        let t2 = thread::spawn(|| {
            let mut called = false;
            O.init(|| {
                called = true;
            });
            assert!(!called);
        });

        tx2.send(()).unwrap();

        assert!(t1.join().is_ok());
        assert!(t2.join().is_ok());
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn test_size() {
        use std::mem::size_of;

        assert_eq!(size_of::<OnceCell<u32>>(), 4 * size_of::<u32>());
    }
}
