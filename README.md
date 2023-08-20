# Lookaside Lists Allocator for Rust

This crate provides an experimental Rust allocator for the Windows Kernel based on Lookaside Lists.

Given the nature of Lookaside Lists (fixed-size buffers) this Allocator is not meant to be used as the Global Allocator for this reason this crate does not implement the GlobalAlloc trait,
on the other hand it does implement the Allocator trait (no grow nor shrink) hence this being an experimental/unstable crate.

## Usage
The crate can be used by means of the `allocator_api` or directly by using the `LookasideAlloc` struct.

### Rust Driver
If working on a Driver fully written in Rust, the following example shows how we can make use of the crate by means of the Allocator API.

```rust
#![no_std]
#![feature(allocator_api)]
#[macro_use] extern crate win_lookaside;

extern crate alloc;

use alloc::boxed::Box;
use win_lookaside::LookasideAlloc;
use windows_sys::Wdk::Foundation::NonPagedPool;

fn example() {
    // Init Lookaside List allocator with default values//!
    let mut allocator = LookasideAlloc::default();

    // Init Lookaside List with fixed-size to hold a u32
    // Properly handle possible InitError;
    allocator.init(core::mem::size_of::<u32>(), NonPagedPool as i32, None, None, None).unwrap();

    // Allocate from Lookaside & Free to it on Drop
    {
        let Ok(ctx) = Box::try_new_in(10, &allocator) else {
            return; // AllocError
        };
    }

    // Destroy Lookaside List Allocator
    allocator.destroy();
}
```
> All in one function just for the sake of the example, usually we would store the Lookaside Allocator in some structure or global variable initialized in the DriverEntry and destroy it in the DriverUnload.

The above example would generate the following simplified pseudocode when looking at it with a decompiler:   
```c
void example()
{
  Status = -2;
  memset(&LookasideAlloc.unwrap, 0, sizeof(LookasideAlloc));
  while ( _InterlockedCompareExchange8(&LookasideAlloc.spin, 1, 0) )
  {
    while ( LookasideAlloc.spin )
      _mm_pause();
  }
  if ( ExInitializeLookasideListEx(&LookasideAlloc.lookaside, NULL, NULL, NonPagedPool, 0, 4, 'LLrs', 0) )
  {
    LookasideAlloc.spin = 0;
    return;
  }
  LookasideAlloc.init = 1;
  LookasideAlloc.spin = 0;
  while ( _InterlockedCompareExchange8(&LookasideAlloc.spin, 1, 0) )
  {
    while ( LookasideAlloc.spin )
      _mm_pause();
  }
  ctx = ExAllocateFromLookasideListEx(&LookasideAlloc.lookaside);
  LookasideAlloc.spin = 0;
  if ( !ctx )
    return;
  *ctx = 10;
  while ( _InterlockedCompareExchange8(&LookasideAlloc.spin, 1, 0) )
  {
      while ( LookasideAlloc.spin )
        _mm_pause();
  }
  ExFreeToLookasideListEx(&LookasideAlloc.lookaside, ctx);
  LookasideAlloc.spin = 0;
  while ( _InterlockedCompareExchange8(&LookasideAlloc.spin, 1, 0) )
  {
    while ( LookasideAlloc.spin )
      _mm_pause();
  }
  if ( LookasideAlloc.init )
    ExDeleteLookasideListEx(&LookasideAlloc.lookaside);
  LookasideAlloc.spin = 0;
}
```

### C++ Driver
Another option is if we are working with a Driver written in C++ and we want to work on a extensions/component in Rust. We can write a thin FFI layer on top of this crate to expose the functionality.

A very simple implementation of how this FFI layer could look like is the following:
```rust
#![no_std]
#![feature(allocator_api)]
#[macro_use] extern crate win_lookaside;

extern crate alloc;

use alloc::boxed::Box;
use windows_sys::Wdk::Foundation::PagedPool;
use windows_sys::Win32::Foundation::{NTSTATUS, STATUS_INSUFFICIENT_RESOURCES, STATUS_SUCCESS};
use win_lookaside::LookasideAlloc;

// Interior mutability due to the way the Lookaside API works
static mut LOOKASIDE: LookasideAlloc = LookasideAlloc::default();

struct Context{};

#[no_mangle]
pub unsafe extern "C" fn init_lookaside(tag: u32) -> NTSTATUS {
    LOOKASIDE.init(core::mem::size_of::<Context>(), PagedPool, Some(tag), None, None )?;
    STATUS_SUCCESS
}

#[no_mangle]
pub extern "C" fn create_context(context: *mut *mut Context) -> FfiResult<()> {
    let Ok(ctx) = unsafe { Box::try_new_in(Context {}, &LOOKASIDE) } else {
        return STATUS_INSUFFICIENT_RESOURCES;
    };

    unsafe {
        *context = Box::into_raw(ctx);
    }

    STATUS_SUCCESS
}

#[no_mangle]
pub extern "C" fn remove_context(context: *mut Context) {
    let _ctx = unsafe { Box::from_raw_in(context, &LOOKASIDE) };
}

#[no_mangle]
pub unsafe extern "C" fn free_lookaside() {
    LOOKASIDE.destroy();
}
```
> Here the Context is just an empty struct, but it could be something more complex that could offer more functionality and the C++ driver would just need to store those as an opaque pointer.

We could then use this FFI layer from our C++ Driver in the following fashion:
```cpp
// No error handling
#define LOOKASIDE_TAG 'aabb'

NTSTATUS
DriverEntry(
    _In_ PDRIVER_OBJECT     DriverObject,
    _In_ PUNICODE_STRING    RegistryPath
    )
{ 
    UNREFERENCED_PARAMETER(RegistryPath);

    PRUST_CTX context1, context2, context3, context4;

    DriverObject->DriverUnload = DriverUnload;

    init_lookaside(LOOKASIDE_TAG);

    create_context(&context1);
    create_context(&context2);
    create_context(&context3);
    create_context(&context4);

    remove_context(context1);
    remove_context(context3);
    remove_context(context4);
    remove_context(context2);
}

VOID
DriverUnload(
    _In_ PDRIVER_OBJECT DriverObject
    )
{
    PAGED_CODE();
    UNREFERENCED_PARAMETER(DriverObject);

    free_lookaside();
}
```
We can inspect the state of the Lookaside List just before the call to `free_lookaside` with the WinDBG extension `!lookaside` to get an output likeso:

```
4: kd> !lookaside fffff80474866030

Lookaside "" @ 0xfffff80474866030  Tag(hex): 0x61616262 "bbaa"
    Type           =       0000  NonPagedPool
    Current Depth  =          0  Max Depth  =          4
    Size           =          8  Max Alloc  =         32
    AllocateMisses =          4  FreeMisses =          0
    TotalAllocates =          4  TotalFrees =          4
    Hit Rate       =          0% Hit Rate   =        100%
```

## Remarks
- This crate has been written with love but is still experimental!! Please keep that in mind before making use of it 😄.
- This crate has been developed under the 22H2 WDK meaning certain Lookaside API methods are exported instead of inlined. The crate is yet to be tested in an older WDK.
- No benchmark nor performance test has been done, although the crate has been tested running under driver verifier with standard settings.
- Default implementation of methods grow, grow_zeroed & shrink of the Allocator API have been overridden and will panic if used since this represents a misuse of the Lookaside API. 

## TODO
- [ ] Use Lookaside API bindings from windows-sys when available (Look into WDK metadata).
- [ ] Use Native MS synchronization primitives.
- [ ] Test the crate with a WDK older than 22H2.