#![allow(non_snake_case)]

use core::{
    cell::Cell,
    mem,
    ptr,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use winapi::{
    shared::{
        basetsd::SIZE_T,
        minwindef::{BOOL, DWORD, TRUE, ULONG},
        ntdef::NTSTATUS,
        ntstatus::{STATUS_SUCCESS},
    },
    um::{
        libloaderapi::{GetModuleHandleA, GetProcAddress},
        winbase::INFINITE,
        winnt::{
            ACCESS_MASK, BOOLEAN, GENERIC_READ, GENERIC_WRITE, HANDLE, LPCSTR, PHANDLE,
            PLARGE_INTEGER, PVOID,
        },
    },
};

use crate::thread_parker::{Futex, RESERVED_MASK};

struct Backend {
    initialized: AtomicBool,
    backend: Cell<AvailableBackend>,
}
static BACKEND: Backend = Backend::new();

impl Backend {
    const fn new() -> Self {
        Backend {
            initialized: AtomicBool::new(false),
            backend: Cell::new(AvailableBackend::None),
        }
    }

    fn get(&self) -> AvailableBackend {
        if !self.initialized.load(Ordering::Acquire) {
            let backend =
                if let Some(res) = ProbeWaitAddress() { AvailableBackend::Wait(res) }
                else if let Some(res) = ProbeKeyedEvent() { AvailableBackend::Keyed(res) }
                else {
                    panic!("failed to load both NT Keyed Events (WinXP+) and \
                           WaitOnAddress/WakeByAddress (Win8+)");
                };
            self.backend.set(backend);
            self.initialized.store(true, Ordering::Release);
        }
        self.backend.get()
    }
}

unsafe impl Sync for Backend {}

#[derive(Clone, Copy)]
enum AvailableBackend {
    None,
    Wait(WaitAddress),
    Keyed(KeyedEvent),
}

#[derive(Clone, Copy)]
struct WaitAddress {
    WaitOnAddress: extern "system" fn(
        Address: PVOID,
        CompareAddress: PVOID,
        AddressSize: SIZE_T,
        dwMilliseconds: DWORD,
    ) -> BOOL,
    WakeByAddressAll: extern "system" fn(Address: PVOID),
}

#[derive(Clone, Copy)]
struct KeyedEvent {
    handle: HANDLE, // The global keyed event handle.
    NtReleaseKeyedEvent: extern "system" fn(
        EventHandle: HANDLE,
        Key: PVOID,
        Alertable: BOOLEAN,
        Timeout: PLARGE_INTEGER,
    ) -> NTSTATUS,
    NtWaitForKeyedEvent: extern "system" fn(
        EventHandle: HANDLE,
        Key: PVOID,
        Alertable: BOOLEAN,
        Timeout: PLARGE_INTEGER,
    ) -> NTSTATUS,
}

fn ProbeWaitAddress() -> Option<WaitAddress> {
    unsafe {
        // MSDN claims that that WaitOnAddress and WakeByAddressAll are
        // located in kernel32.dll, but they aren't...
        let synch_dll =
            GetModuleHandleA(b"api-ms-win-core-synch-l1-2-0.dll\0".as_ptr() as LPCSTR);
        if synch_dll.is_null() {
            return None;
        }

        let WaitOnAddress = GetProcAddress(synch_dll, b"WaitOnAddress\0".as_ptr() as LPCSTR);
        if WaitOnAddress.is_null() {
            return None;
        }
        let WakeByAddressAll =
            GetProcAddress(synch_dll, b"WakeByAddressAll\0".as_ptr() as LPCSTR);
        if WakeByAddressAll.is_null() {
            return None;
        }

        Some(WaitAddress {
            WaitOnAddress: mem::transmute(WaitOnAddress),
            WakeByAddressAll: mem::transmute(WakeByAddressAll),
        })
    }
}

fn ProbeKeyedEvent() -> Option<KeyedEvent> {
    unsafe {
        let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr() as LPCSTR);
        if ntdll.is_null() {
            return None;
        }

        let NtCreateKeyedEvent =
            GetProcAddress(ntdll, b"NtCreateKeyedEvent\0".as_ptr() as LPCSTR);
        if NtCreateKeyedEvent.is_null() {
            return None;
        }
        let NtWaitForKeyedEvent =
            GetProcAddress(ntdll, b"NtWaitForKeyedEvent\0".as_ptr() as LPCSTR);
        if NtWaitForKeyedEvent.is_null() {
            return None;
        }
        let NtReleaseKeyedEvent =
            GetProcAddress(ntdll, b"NtReleaseKeyedEvent\0".as_ptr() as LPCSTR);
        if NtReleaseKeyedEvent.is_null() {
            return None;
        }

        let NtCreateKeyedEvent: extern "system" fn(
            KeyedEventHandle: PHANDLE,
            DesiredAccess: ACCESS_MASK,
            ObjectAttributes: PVOID,
            Flags: ULONG,
        ) -> NTSTATUS = mem::transmute(NtCreateKeyedEvent);
        let mut handle: HANDLE = ptr::null_mut();
        let status = NtCreateKeyedEvent(
            &mut handle,
            GENERIC_READ | GENERIC_WRITE,
            ptr::null_mut(),
            0,
        );
        if status != STATUS_SUCCESS {
            return None;
        }

        Some(KeyedEvent {
            handle: handle,
            NtReleaseKeyedEvent: mem::transmute(NtReleaseKeyedEvent),
            NtWaitForKeyedEvent: mem::transmute(NtWaitForKeyedEvent),
        })
    }
}

impl Futex for AtomicUsize {
    unsafe fn park<P>(&self, should_park: P)
    where
        P: Fn(usize) -> bool
    {
        match BACKEND.get() {
            AvailableBackend::Wait(f) => {
                loop {
                    let current = self.load(Ordering::Relaxed);
                    if !should_park(current & !RESERVED_MASK) {
                        break;
                    }
                    let address = self as *const _ as PVOID;
                    let compare_address = &current as *const _ as PVOID;
                    let r = (f.WaitOnAddress)(address, compare_address, mem::size_of::<AtomicUsize>(), INFINITE);
                    debug_assert!(r == TRUE);
                }
            }
            AvailableBackend::Keyed(f) => {
                let mut current = self.load(Ordering::Relaxed);
                loop {
                    if !should_park(current & !RESERVED_MASK) {
                        break;
                    }
                    // We need to store that there is at least one waiter. If another thread calls
                    // `NtReleaseKeyedEvent` while there are no waiters it blocks indefinitely.
                    let waiter_count = current & RESERVED_MASK;
                    let old = self.compare_and_swap(current,
                                                    (current & !RESERVED_MASK) | waiter_count,
                                                    Ordering::Relaxed);
                    if old != current {
                        current = old;
                        continue;
                    }
                    // We need some unique key to wait on. The address of `self` seems like a good
                    // option.
                    let key = self as *const AtomicUsize as PVOID;
                    (f.NtWaitForKeyedEvent)(f.handle, key, 0, ptr::null_mut());
                }
            }
            AvailableBackend::None => unreachable!(),
        }
    }

    unsafe fn store_and_unpark(&self, new: usize) {
        match BACKEND.get() {
            AvailableBackend::Wait(f) => {
                self.store(new, Ordering::Release); // FIXME: maybe SeqCst?
                (f.WakeByAddressAll)(self as *const _ as PVOID);
            }
            AvailableBackend::Keyed(f) => {
                let current = self.swap(new, Ordering::Release); // FIXME: maybe SeqCst?
                let waiter_count = current & RESERVED_MASK;
                // Recreate the key; the address of self.
                let key = self as *const AtomicUsize as PVOID;
                for _ in 0..waiter_count {
                    (f.NtReleaseKeyedEvent)(f.handle, key, 0, ptr::null_mut());
                }
            }
            AvailableBackend::None => unreachable!(),
        }
    }
}

// `NtWaitForKeyedEvent` allows a thread to go to sleep, waiting on the event signaled by
// `NtReleaseKeyedEvent`. The major different between this API and the Futex API is that there is no
// comparison value that is checked as the thread goes to sleep. Instead the `NtReleaseKeyedEvent`
// function blocks, waiting for a thread to wake if there is none. (Compared to the Futex wake
// function which will immediately return.)
//
// Thus to use this API we need to keep track of how many waiters there are to prevent the release
// function from hanging.
//
// http://joeduffyblog.com/2006/11/28/windows-keyed-events-critical-sections-and-new-vista-synchronization-features/
// http://locklessinc.com/articles/keyed_events/
