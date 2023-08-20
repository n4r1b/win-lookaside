//! This crate provides an experimental Rust allocator for the Windows Kernel based on [Lookaside
//! Lists](https://learn.microsoft.com/en-us/windows-hardware/drivers/kernel/using-lookaside-lists).
//!
//! Given the nature of Lookaside Lists (fixed-size buffers) this Allocator is not meant to be used
//! as the Global Allocator for this reason this crate does not implement the GlobalAlloc trait,
//! on the other hand it does implement the Allocator trait (no grow nor shrink) so it can
//! be used as the allocator in `xxx_in` methods.
//! > The default implementation of grow/grow_zeroed & shrink of the Allocator API has been
//! overridden to throw a panic. This is done just to make the user aware that calling these
//! methods on this allocator is a misuse of the Lookaside API.
//!
//! Obviously, this crate requires the `allocator_api`feature is enabled (hence this being an
//! experimental/unstable crate).
//!
//! Alternatively, this can be used directly by initializing the allocator and using allocate &
//! deallocate methods to get an entry from the list as a  `*mut u8` and return an entry to the
//! list respectively.
//!
//! Since this is meant to be used in the kernel, this Allocator can return `NULL` and doesn't
//! trigger the `alloc_error_handler` if an OOM condition happens. This requires using
//! fallible APIs such as [Box::try_new_in](https://doc.rust-lang.org/nightly/alloc/boxed/struct.Box.html#method.try_new_in)
//! or crates such as [fallible_vec](https://docs.rs/fallible_vec/latest/fallible_vec/index.html).
//!
//! # Usage
//!
//! If working on a Driver fully written in Rust, the following example shows how we can make use
//! of the Allocator.
//!
//! All in one function just for the sake of the example, usually we would store the Lookaside
//! Allocator in some structure or global variable initialized in the DriverEntry and destroy it
//! in the DriverUnload.
//! ```
//! #![no_std]
//! #![feature(allocator_api)]
//! #[macro_use] extern crate win_lookaside;
//!
//! extern crate alloc;
//!
//! use alloc::boxed::Box;
//! use win_lookaside::LookasideAlloc;
//! use windows_sys::Wdk::Foundation::NonPagedPool;
//!
//! fn example() {
//!     // Init Lookaside List allocator with default values//!
//!     let mut allocator = LookasideAlloc::default();
//!
//!     // Init Lookaside List with fixed-size to hold a u32
//!     // Properly handle possible InitError;
//!     allocator.init(core::mem::size_of::<u32>(), NonPagedPool as i32, None, None, None).unwrap();
//!
//!     // Allocate from Lookaside & Free to it on Drop
//!     {
//!         let Ok(ctx) = Box::try_new_in(10, &allocator) else {
//!             return; // AllocError
//!         };
//!     }
//!
//!     // Destroy Lookaside List Allocator
//!     allocator.destroy();
//! }
//! ```
//!
//!
//! Another option is if we are working with a Driver written in C++ and we want to work on a
//! extensions/component in Rust. We can write a thin FFI layer on top of this crate to expose the
//! functionality to the Driver.
//!
//! A very simple implementation of how this FFI layer could look like is the following:
//! ```
//! #![no_std]
//! #![feature(allocator_api)]
//! #[macro_use] extern crate win_lookaside;
//!
//! extern crate alloc;
//!
//! use alloc::boxed::Box;
//! use windows_sys::Wdk::Foundation::PagedPool;
//! use windows_sys::Win32::Foundation::{NTSTATUS, STATUS_INSUFFICIENT_RESOURCES, STATUS_SUCCESS};
//! use win_lookaside::LookasideAlloc;
//!
//! // Interior mutability due to the way the Lookaside API works
//! static mut LOOKASIDE: LookasideAlloc = LookasideAlloc::default();
//!
//! struct Context{};
//!
//! #[no_mangle]
//! pub unsafe extern "C" fn init_lookaside(tag: u32) -> NTSTATUS {
//!     LOOKASIDE.init(core::mem::size_of::<Context>(), PagedPool, Some(tag), None, None )?;
//!     STATUS_SUCCESS
//! }
//!
//! #[no_mangle]
//! pub extern "C" fn create_context(context: *mut *mut Context) -> FfiResult<()> {
//!     let Ok(ctx) = unsafe { Box::try_new_in(Context {}, &LOOKASIDE) } else {
//!         return STATUS_INSUFFICIENT_RESOURCES;
//!     };
//!
//!     unsafe {
//!         *context = Box::into_raw(ctx);
//!     }
//!
//!     STATUS_SUCCESS
//! }
//!
//! #[no_mangle]
//! pub extern "C" fn remove_context(context: *mut Context) {
//!     let _ctx = unsafe { Box::from_raw_in(context, &LOOKASIDE) };
//! }
//!
//! #[no_mangle]
//! pub unsafe extern "C" fn free_lookaside() {
//!     LOOKASIDE.destroy();
//! }
//! ```
//! > Here the Context is just an empty struct, but it could be something more complex that could
//! offer more functionality and the C++ driver would just need to store those as an opaque pointer.
//!
//! # Remarks
//! This crate has been developed under the 22H2 WDK meaning certain Lookaside API methods are
//! exported instead of inlined. The crate is yet to be tested in an older WDK, behavior when
//! trying to build might be different.
//!
//! At the moment the crate uses [spin](https://crates.io/crates/spin) as the synchronization
//! mechanism. Even thou this does the job, ideally at some point it should use synchronization
//! primitives native to the OS.
//!
//!
#![no_std]
#![feature(allocator_api)]
use core::{
    alloc::{AllocError, Allocator, Layout},
    cell::RefCell,
    ffi::c_void,
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use windows_sys::{
    Wdk::Foundation::POOL_TYPE,
    Win32::Foundation::{NTSTATUS, STATUS_SUCCESS},
};

/*
TODO: Review WDK metadata https://github.com/microsoft/wdkmetadata#overview
TODO: Use bindings from windows-rs when available
use windows_sys::Wdk::System::SystemServices::{
    ExAllocateFromLookasideListEx,
    ExDeleteLookasideListEx,
    ExFlushLookasideListEx,
    ExFreeToLookasideListEx,
    ExInitializeLookasideListEx,
};
*/

/// Default PoolTag used by the Lookaside Allocator if none passed when initializing it
pub const DEFAULT_POOL_TAG: u32 = u32::from_ne_bytes(*b"srLL");

/// Possible Errors returned by the Lookaside List Allocator
pub enum LookasideError {
    InitError(NtStatus),
}

/// Lookaside List Allocator Result
pub type LookasideResult<T> = Result<T, LookasideError>;

/// The LookasideListAllocateEx routine allocates the storage for a new lookaside-list entry when
/// a client requests an entry from a lookaside list that is empty.
///
/// More info: <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nc-wdm-allocate_function_ex>
pub type AllocateFunctionEx = Option<
    unsafe extern "system" fn(
        pooltype: POOL_TYPE,
        numberofbytes: usize,
        tag: u32,
        lookaside: *mut LookasideList,
    ) -> *mut c_void,
>;
/// The LookasideListFreeEx routine frees the storage for a lookaside-list entry when a client
/// tries to insert the entry into a lookaside list that is full.
///
/// More info: <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nc-wdm-free_function_ex>
pub type FreeFunctionEx =
    Option<unsafe extern "system" fn(buffer: *const c_void, *mut LookasideList)>;

/// Newtype over windows-sys [NTSTATUS (i32)](https://docs.rs/windows-sys/0.48.0/windows_sys/Win32/Foundation/type.NTSTATUS.html)
#[repr(transparent)]
#[derive(Copy, Clone)]
pub struct NtStatus(NTSTATUS);

impl NtStatus {
    /// True if NTSTATUS == STATUS_SUCCESS
    pub fn is_ok(&self) -> bool {
        self.0 == STATUS_SUCCESS
    }

    /// True if NTSTATUS != STATUS_SUCCESS
    pub fn is_err(&self) -> bool {
        self.0 != STATUS_SUCCESS
    }
}

/// Wrapper over the [_LOOKASIDE_LIST_EX](https://learn.microsoft.com/en-us/windows-hardware/drivers/kernel/eprocess#lookaside_list_ex)
/// type
///
/// See: <https://www.vergiliusproject.com/kernels/x64/Windows%2011/22H2%20(2022%20Update)/_LOOKASIDE_LIST_EX>
#[repr(C)]
pub struct LookasideList {
    general_lookaside_pool: [u8; 0x60],
}

impl LookasideList {
    const fn init() -> Self {
        LookasideList {
            general_lookaside_pool: [0; 0x60],
        }
    }

    fn as_ptr(&self) -> *const Self {
        self as *const _
    }

    fn as_mut_ptr(&mut self) -> *mut Self {
        self as *mut _
    }
}

#[link(name = "ntoskrnl")]
extern "system" {
    /// <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-exinitializelookasidelistex>
    pub fn ExInitializeLookasideListEx(
        lookaside: *mut LookasideList,
        allocate: AllocateFunctionEx,
        free: FreeFunctionEx,
        pool_type: POOL_TYPE,
        flags: u32,
        size: usize,
        tag: u32,
        depth: u16,
    ) -> NtStatus;

    /// <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-exdeletelookasidelistex>
    pub fn ExDeleteLookasideListEx(lookaside: *mut LookasideList);
    /// <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-exallocatefromlookasidelistex>
    pub fn ExAllocateFromLookasideListEx(lookaside: *mut LookasideList) -> *mut u64;
    /// <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-exfreetolookasidelistex>
    pub fn ExFreeToLookasideListEx(lookaside: *mut LookasideList, entry: u64);
}

/// Lookaside List Allocator
pub struct LookasideAlloc {
    init: AtomicBool,
    lookaside: spin::Mutex<RefCell<LookasideList>>,
}

impl Default for LookasideAlloc {
    fn default() -> Self {
        Self::default()
    }
}

impl Drop for LookasideAlloc {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl LookasideAlloc {
    #[inline(always)]
    fn borrow_mut_list(list: &RefCell<LookasideList>) -> *mut LookasideList {
        // Should be called with the lock so it's safe to assume the value is not currently
        // borrowed hence the unwrap.
        list.try_borrow_mut().unwrap().as_mut_ptr()
    }

    /// const default initializer for the Lookaside List allocator.
    pub const fn default() -> Self {
        LookasideAlloc {
            init: AtomicBool::new(false),
            lookaside: spin::Mutex::new(RefCell::new(LookasideList::init())),
        }
    }

    /// Initialize the Lookaside list
    pub fn init(
        &mut self,
        size: usize,
        pool_type: POOL_TYPE,
        tag: Option<u32>,
        flags: Option<u32>,
        alloc_fn: AllocateFunctionEx,
        free_fn: FreeFunctionEx,
    ) -> LookasideResult<()> {
        let mut lock = self.lookaside.lock();
        let status = unsafe {
            ExInitializeLookasideListEx(
                lock.get_mut(),
                alloc_fn,
                free_fn,
                pool_type,
                flags.unwrap_or(0),
                size,
                tag.unwrap_or(DEFAULT_POOL_TAG),
                0,
            )
        };

        if status.is_err() {
            return Err(LookasideError::InitError(status));
        }

        self.init.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Delete the Lookaside List
    pub fn destroy(&mut self) {
        let mut lock = self.lookaside.lock();
        if self.init.load(Ordering::Relaxed) {
            unsafe {
                ExDeleteLookasideListEx(lock.get_mut());
            }
        }
    }

    // Lookaside API guarantees thread-safety when calling Alloc & Free
    /// Allocate from Lookaside List
    pub fn alloc(&self) -> *mut u8 {
        let lock = self.lookaside.lock();
        unsafe { ExAllocateFromLookasideListEx(Self::borrow_mut_list(&lock)) as _ }
    }

    /// Free to Lookaside List
    pub fn free(&self, ptr: *mut u8) {
        let lock = self.lookaside.lock();
        unsafe { ExFreeToLookasideListEx(Self::borrow_mut_list(&lock), ptr as _) }
    }
}

unsafe impl Allocator for LookasideAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        let pool = self.alloc();

        if pool.is_null() {
            return Err(AllocError);
        }

        let slice = unsafe { core::slice::from_raw_parts_mut(pool, layout.size()) };
        Ok(unsafe { NonNull::new_unchecked(slice) })
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, _layout: Layout) {
        self.free(ptr.as_ptr())
    }

    /// Lookaside List does not support Grow
    unsafe fn grow(
        &self,
        _ptr: NonNull<u8>,
        _old_layout: Layout,
        _new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        panic!("Not supported");
    }

    /// Lookaside List does not support Grow
    unsafe fn grow_zeroed(
        &self,
        _ptr: NonNull<u8>,
        _old_layout: Layout,
        _new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        panic!("Not supported");
    }

    /// Lookaside List does not support Shrink
    unsafe fn shrink(
        &self,
        _ptr: NonNull<u8>,
        _old_layout: Layout,
        _new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        panic!("Not supported");
    }
}
