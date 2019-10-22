// There are two pieces of tricky concurrent code in this module that both operate on the same
// atomic `OnceCell::state_and_queue`:
// - one to make sure only one thread is initializing the `OnceCell`, the `state` part.
// - another to manage a queue of waiting threads while the state is RUNNING.
//
// The concept of a queue of waiting threads using a linked list, where every node is a struct on
// the stack of a waiting thread, is taken from `std::sync::Once`.
//
// Differences with `std::sync::Once`:
//   * no poisoning
//   * init function can fail
//   * thread parking is factored out of `initialize`
//
// Atomic orderings:
// When initializing `OnceCell` we deal with multiple atomics:
// `OnceCell.state_and_queue` and an unknown number of `Waiter.signaled`.
// * `state_and_queue` is used (1) as a state flag, (2) for synchronizing the
//   data in `OnceCell.value`, and (3) for synchronizing `Waiter` nodes.
//     - At the end of the `initialize_inner` function we have to make sure
//      `OnceCell.value` is acquired. So every load which can be the only one to
//       load COMPLETED must have at least Acquire ordering, which means all
//       three of them.
//     - `WaiterQueue::Drop` is the only place that stores COMPLETED, and must
//       do so with Release ordering to make `OnceCell.value` available.
//     - `wait` inserts `Waiter` nodes as a pointer in `state_and_queue`, and
//       needs to make the nodes available with Release ordering. The load in
//       its `compare_and_swap` can be Relaxed because it only has to compare
//       the atomic, not to read other data.
//     - `WaiterQueue::Drop` must see the `Waiter` nodes, so it must load
//       `state_and_queue` with Acquire ordering.
//     - There is just one store where `state_and_queue` is used only as a
//       state flag, without having to synchronize data: switching the state
//       from EMPTY to RUNNING in `initialize_inner`. This store can be Relaxed,
//       but the read has to be Acquire because of the requirements mentioned
//       above.
// * `Waiter.signaled` is only used as a flag, and does not have to synchronize
//    data. It can be stored and loaded with Relaxed ordering.
// * There is one place where the two atomics `OnceCell.state_and_queue` and
//   `Waiter.signaled` come together, and may be reordered by the compiler or
//   processor. When `wait` loads `signaled` and returns when true,
//   `initialize_inner` continues with a load on `state_and_queue`. If they are
//   reordered `state_and_queue` might still return the old state from before
//   the thread got parked. Both have to use SeqCst ordering. It is possible for
//   `signaled` to be set and for `wait` to return before parking the thread, so
//   even if `park`/`unpark` would form some sort of barrier, that would not be
//   enough.

use std::{
    cell::UnsafeCell,
    marker::PhantomData,
    panic::{RefUnwindSafe, UnwindSafe},
    ptr,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread::{self, Thread},
};

#[derive(Debug)]
pub(crate) struct OnceCell<T> {
    // `state_and_queue` is actually an a pointer to a `Waiter` with extra state
    // bits, so we add the `PhantomData` appropriately.
    state_and_queue: AtomicUsize,
    _marker: PhantomData<*mut Waiter>,
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

// Three states that a OnceCell can be in, encoded into the lower bits of `state_and_queue` in
// the OnceCell structure.
const EMPTY: usize = 0x0;
const RUNNING: usize = 0x1;
const COMPLETE: usize = 0x2;

// Mask to learn about the state. All other bits are the queue of waiters if
// this is in the RUNNING state.
const STATE_MASK: usize = 0x3;

// Representation of a node in the linked list of waiters, used while in the
// RUNNING state.
struct Waiter {
    thread: Thread,
    signaled: AtomicBool,
    next: *const Waiter,
    // Note: we have to use a raw pointer for `next`. After setting `signaled`
    // to `true` the next thread may wake up and free its `Waiter`, while we
    // would still hold a live reference.
}

// Head of a linked list of waiters.
// Will wake up the waiters when it gets dropped, i.e. also on panic.
struct WaiterQueue<'a> {
    state_and_queue: &'a AtomicUsize,
    set_state_on_drop_to: usize,
}

impl<T> OnceCell<T> {
    pub(crate) const fn new() -> OnceCell<T> {
        OnceCell {
            state_and_queue: AtomicUsize::new(EMPTY),
            _marker: PhantomData,
            value: UnsafeCell::new(None),
        }
    }

    /// Safety: synchronizes with store to value via Release/Acquire.
    #[inline]
    pub(crate) fn is_initialized(&self) -> bool {
        // An `Acquire` load is enough because that makes all the initialization
        // operations visible to us, and, this being a fast path, weaker
        // ordering helps with performance.
        self.state_and_queue.load(Ordering::Acquire) == COMPLETE
    }

    /// Safety: synchronizes with store to value via Release/Acquire.
    /// Writes value only once because we never get to INCOMPLETE state after a
    /// successful write.
    #[cold]
    pub(crate) fn initialize<F, E>(&self, f: F) -> Result<(), E>
    where
        F: FnOnce() -> Result<T, E>,
    {
        // Create a new closure that executes `f`, and that can write its results directly into
        // `self.value` and `res`. This way `initialize_inner` can be monomorphic.
        // It should return a bool to indicate success or failure.
        let mut f = Some(f);
        let mut res: Result<(), E> = Ok(());
        let slot = &self.value;
        initialize_inner(&self.state_and_queue, &mut || {
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

// This is a non-generic function to reduce the monomorphization cost of
// using `call_once` (this isn't exactly a trivial or small implementation).
//
// Finally, this takes an `FnMut` instead of a `FnOnce` because there's
// currently no way to take an `FnOnce` and call it via virtual dispatch
// without some allocation overhead.
fn initialize_inner(state: &AtomicUsize, init: &mut dyn FnMut() -> bool) {
    let mut state_and_queue = state.load(Ordering::Acquire);
    loop {
        match state_and_queue {
            COMPLETE => break,
            EMPTY => {
                // Try to register this thread as the one doing initialization (RUNNING).
                let old = state.compare_and_swap(EMPTY, RUNNING, Ordering::Acquire);
                if old != EMPTY {
                    state_and_queue = old;
                    continue
                }
                // `waiter_queue` will manage other waiting threads, and
                // wake them up on drop.
                let mut waiter_queue = WaiterQueue {
                    state_and_queue: state,
                    set_state_on_drop_to: EMPTY,
                };
                // Run the initialization function.
                init();
                waiter_queue.set_state_on_drop_to = COMPLETE;
                break
            }
            _ => {
                // All other values must be RUNNING with possibly a
                // pointer to the waiter queue in the more significant bits.
                assert!(state_and_queue & STATE_MASK == RUNNING);
                wait(&state, state_and_queue);
                // Load with SeqCst instead of Acquire to prevent reordering
                // with the previous load on `Waiter.signaled` in `wait()`.
                state_and_queue = state.load(Ordering::SeqCst);
            }
        }
    }
}

fn wait(state_and_queue: &AtomicUsize, current_state: usize) {
    // Create the node for our current thread that we are going to try to slot
    // in at the head of the linked list.
    let mut node = Waiter {
        thread: thread::current(),
        signaled: AtomicBool::new(false),
        next: ptr::null(),
    };
    let me = &node as *const Waiter as usize;
    assert!(me & STATE_MASK == 0); // We assume pointers have 2 free bits that
                                   // we can use for state.

    // Try to slide in the node at the head of the linked list.
    // Run in a loop where we make sure the status is still RUNNING, and that
    // another thread did not just replace the head of the linked list.
    let mut old_head_and_status = current_state;
    loop {
        if old_head_and_status & STATE_MASK != RUNNING {
            return; // No need anymore to enqueue ourselves.
        }

        node.next = (old_head_and_status & !STATE_MASK) as *mut Waiter;
        let old = state_and_queue.compare_and_swap(old_head_and_status,
                                                   me | RUNNING,
                                                   Ordering::Release);
        if old == old_head_and_status {
            break; // Success!
        }
        old_head_and_status = old;
    }

    // We have enqueued ourselves, now lets wait.
    // It is important not to return before being signaled, otherwise we would
    // drop our `Waiter` node and leave a hole in the linked list (and a
    // dangling reference). Guard against spurious wakeups by reparking
    // ourselves until we are signaled.
    //
    // Load with SeqCst instead of Relaxed to prevent reordering with the
    // following load on `OnceCell.state_and_queue` in `wait()`.
    while !node.signaled.load(Ordering::SeqCst) {
        thread::park();
    }
}

impl Drop for WaiterQueue<'_> {
    fn drop(&mut self) {
        // Swap out our state with however we finished.
        let state_and_queue = self.state_and_queue.swap(self.set_state_on_drop_to,
                                                        Ordering::AcqRel);

        // We should only ever see an old state which was RUNNING.
        assert_eq!(state_and_queue & STATE_MASK, RUNNING);

        // Walk the entire linked list of waiters and wake them up (in lifo
        // order, last to register is first to wake up).
        // Note that storing `true` in `signaled` must be the last read we do
        // because right after that the node can be freed if there happens to be
        // a spurious wakeup.
        unsafe {
            let mut queue = (state_and_queue & !STATE_MASK) as *const Waiter;
            while !queue.is_null() {
                let next = (*queue).next;
                let thread = (*queue).thread.clone();
                (*queue).signaled.store(true, Ordering::Relaxed);
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
