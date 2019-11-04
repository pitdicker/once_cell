// There's a lot of scary concurrent code in this module, but it is copied from
// `std::sync::Once` with two changes:
//   * no poisoning
//   * init function can fail

use std::{
    cell::{Cell, UnsafeCell},
    marker::PhantomData,
    panic::{RefUnwindSafe, UnwindSafe},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread::{self, Thread},
};

#[derive(Debug)]
pub(crate) struct OnceCell<T> {
    // This `state` word is actually an encoded version of just a pointer to a
    // `Waiter`, so we add the `PhantomData` appropriately.
    state: AtomicUsize,
    _marker: PhantomData<*const Waiter>,
    // FIXME: switch to `std::mem::MaybeUninit` once we are ready to bump MSRV
    // that far. It was stabilized in 1.36.0, so, if you are reading this and
    // it's higher than 1.46.0 outside, please send a PR! ;) (and to the same
    // for `Lazy`, while we are at it).
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

// Three states that a OnceCell can be in, encoded into the lower bits of `state` in
// the OnceCell structure.
const INCOMPLETE: usize = 0x0;
const RUNNING: usize = 0x1;
const COMPLETE: usize = 0x2;

// Mask to learn about the state. All other bits are the queue of waiters if
// this is in the RUNNING state.
const STATE_MASK: usize = 0x3;

// Representation of a node in the linked list of waiters, used while in the
// RUNNING state.
// Note: `Waiter` can't hold a mutable pointer to the next thread, because then
// we would both hand out a mutable reference to its `Waiter` node, and keep a
// shared reference to check `signaled`. Instead we use shared references and
// interior mutability.
struct Waiter {
    thread: Cell<Option<Thread>>,
    signaled: AtomicBool,
    next: *const Waiter,
}

// Helper struct used to clean up after a closure call with a `Drop`
// implementation to also run on panic.
struct Finish<'a> {
    failed: bool,
    my_state: &'a AtomicUsize,
}

impl<T> OnceCell<T> {
    pub(crate) const fn new() -> OnceCell<T> {
        OnceCell {
            state: AtomicUsize::new(INCOMPLETE),
            _marker: PhantomData,
            value: UnsafeCell::new(None),
        }
    }

    /// Safety: synchronizes with store to value via Release/(Acquire|SeqCst).
    #[inline]
    pub(crate) fn is_initialized(&self) -> bool {
        // An `Acquire` load is enough because that makes all the initialization
        // operations visible to us, and, this being a fast path, weaker
        // ordering helps with performance. This `Acquire` synchronizes with
        // `SeqCst` operations on the slow path.
        self.state.load(Ordering::Acquire) == COMPLETE
    }

    /// Safety: synchronizes with store to value via SeqCst read from state,
    /// writes value only once because we never get to INCOMPLETE state after a
    /// successful write.
    #[cold]
    pub(crate) fn initialize<F, E>(&self, f: F) -> Result<(), E>
    where
        F: FnOnce() -> Result<T, E>,
    {
        let mut f = Some(f);
        let mut res: Result<(), E> = Ok(());
        let slot = &self.value;
        initialize_inner(&self.state, &mut || {
            let f = f.take().unwrap();
            match f() {
                Ok(value) => {
                    unsafe { *slot.get() = Some(value) };
                    true
                }
                Err(e) => {
                    res = Err(e);
                    false
                }
            }
        });
        res
    }
}

// Note: this is intentionally monomorphic
fn initialize_inner(my_state: &AtomicUsize, init: &mut dyn FnMut() -> bool) -> bool {
    // This cold path uses SeqCst consistently because the
    // performance difference really does not matter there, and
    // SeqCst minimizes the chances of something going wrong.
    let mut state = my_state.load(Ordering::SeqCst);

    'outer: loop {
        match state {
            // If we're complete, then there's nothing to do, we just
            // jettison out as we shouldn't run the closure.
            COMPLETE => return true,

            // Otherwise if we see an incomplete state we will attempt to
            // move ourselves into the RUNNING state. If we succeed, then
            // the queue of waiters starts at null (all 0 bits).
            INCOMPLETE => {
                let old = my_state.compare_and_swap(state, RUNNING, Ordering::SeqCst);
                if old != state {
                    state = old;
                    continue;
                }

                // Run the initialization routine, letting it know if we're
                // poisoned or not. The `Finish` struct is then dropped, and
                // the `Drop` implementation here is responsible for waking
                // up other waiters both in the normal return and panicking
                // case.
                let mut complete = Finish { failed: true, my_state };
                let success = init();
                // Difference from std: abort if `init` errored.
                complete.failed = !success;
                return success;
            }

            // All other values we find should correspond to the RUNNING
            // state with an encoded waiter list in the more significant
            // bits. We attempt to enqueue ourselves by moving us to the
            // head of the list and bail out if we ever see a state that's
            // not RUNNING.
            _ => {
                // Try to slide in the node at the head of the linked list.
                // Run in a loop where we make sure the status is still RUNNING, and that
                // another thread did not just replace the head of the linked list.
                loop {
                    if state & STATE_MASK != RUNNING {
                        break; // No need anymore to enqueue ourselves.
                    }

                    // Create the node for our current thread that we are going to try to
                    // slot in at the head of the linked list.
                    let node = Waiter {
                        thread: Cell::new(Some(thread::current())),
                        signaled: AtomicBool::new(false),
                        next: (state & !STATE_MASK) as *const Waiter,
                    };
                    let me = &node as *const Waiter as usize;

                    let old = my_state.compare_and_swap(state, me | RUNNING, Ordering::Release);
                    if old != state {
                        state = old;
                        continue;
                    }

                    // We have enqueued ourselves, now lets wait.
                    // It is important not to return before being signaled, otherwise we
                    // would drop our `Waiter` node and leave a hole in the linked list
                    // (and a dangling reference). Guard against spurious wakeups by
                    // reparking ourselves until we are signaled.
                    while !node.signaled.load(Ordering::Acquire) {
                        // If the managing thread happens to signal and unpark us before we
                        // can park ourselves, the result could be this thread never gets
                        // unparked. Luckily `park` comes with the guarantee that if it got
                        // an `unpark` just before on an unparked thread is does not park.
                        thread::park();
                    }
                    break;
                }
                state = my_state.load(Ordering::Acquire);
            }
        }
    }
}

impl Drop for Finish<'_> {
    fn drop(&mut self) {
        // Swap out our state with however we finished. We should only ever see
        // an old state which was RUNNING.
        let queue = if self.failed {
            // Difference from std: flip back to INCOMPLETE rather than POISONED.
            self.my_state.swap(INCOMPLETE, Ordering::SeqCst)
        } else {
            self.my_state.swap(COMPLETE, Ordering::SeqCst)
        };
        assert_eq!(queue & STATE_MASK, RUNNING);

        // Decode the RUNNING to a list of waiters, then walk that entire list
        // and wake them up. Note that it is crucial that after we store `true`
        // in the node it can be free'd! As a result we load the `thread` to
        // signal ahead of time and then unpark it after the store.
        unsafe {
            let mut queue = (queue & !STATE_MASK) as *const Waiter;
            while !queue.is_null() {
                let next = (*queue).next;
                let thread = (*queue).thread.take().unwrap();
                (*queue).signaled.store(true, Ordering::SeqCst);
                thread.unpark();
                queue = next;
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
