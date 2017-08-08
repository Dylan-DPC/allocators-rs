#![no_std]
#![feature(alloc, allocator_api)]
// so that we can use core::intrinsics::type_name
#![feature(core_intrinsics)]

extern crate alloc;
use alloc::allocator::{Alloc, AllocErr, Layout};

/// An error indicating that no memory is available.
///
/// The `Exhausted` error indicates that an allocation request has failed due to resources being
/// unavailable. It strongly implies that *some* sequence of deallocations would allow a subsequent
/// reissuing of the original allocation request to succeed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Exhausted;

/// Allocators which allocate objects of a particular type.
///
/// `ObjectAlloc`s provide an interface which is slightly different than the interface provided by
/// a standard allocator. By definition, they are only capable of allocating objects of a
/// particular type. Additionally, memory returned from a call to `alloc` is guaranteed to already
/// be a valid, initialized instance of `T`. `ObjectAlloc`s may differ in how much flexibility they
/// provide in specifying how allocated objects are initialized.
///
/// These differences allow `ObjectAlloc`s to provide significant performance improvements over
/// general-purpose allocators. First, only having to allocate objects of a particular size and
/// alignment allows them to make optimizations that are not available to general-purpose
/// allocators. Second, since `alloc` is required to return already-constructed objects, clients
/// don't have to initialize allocated objects. This, coupled with an object-caching scheme for
/// `dealloc`'d objects, allows many calls to `allloc` to avoid initialization altogether.
///
/// # Dropping
///
/// When an `ObjectAlloc` is dropped, all cached `T` objects that have not yet been dropped are
/// dropped. The order in which they are dropped is undefined.
pub unsafe trait ObjectAlloc<T> {
    /// Allocate an object of type `T`.
    ///
    /// The memory pointed to by the returned raw pointer is guaranteed to be a valid, initialized
    /// instance of `T`. In particular, the returned object will be in one of the following two
    /// states:
    ///
    /// * The result of a call to whatever initialization function was used to configure this
    ///   `ObjectAlloc`
    /// * The same state as a `T` which was previously returned via a call to `dealloc`
    ///
    /// There is one exception to the above rule: It is valid for `ObjectAlloc`s to provide
    /// `unsafe` constructors which return an `ObjectAlloc` that returns invalid or uninitialized
    /// memory from calls to `alloc`, or which accept a constructor function (for `T` objects)
    /// which is itself `unsafe`, and thus not guaranteed to produce valid instances of `T`. Since
    /// these `ObjectAlloc` constructors must be `unsafe`, it is not possible for safe Rust code to
    /// use an `ObjectAlloc` to obtain a reference to uninitialized memory.
    ///
    /// The memory returned by `alloc` is guaranteed to be aligned according to the requirements of
    /// `T` (that is, according to `core::mem::align_of::<T>()`).
    unsafe fn alloc(&mut self) -> Result<*mut T, Exhausted>;

    /// Deallocate an object previously returned by `alloc`.
    ///
    /// If `x` was not obtained through a call to `alloc`, or if `x` has already been `dealloc`'d,
    /// the behavior of `dealloc` is undefined.
    ///
    /// It is valid for `x` to be cached and used to serve future calls to `alloc`. The only
    /// guarantee that is made is that `x` will be dropped at some point during the `ObjectAlloc`'s
    /// lifetime. This may happen during this call to `dealloc`, when the `ObjectAlloc` itself is
    /// dropped, or some time in between.
    unsafe fn dealloc(&mut self, x: *mut T);

    /// Allocator-specific method for signalling an out-of-memory condition.
    ///
    /// `oom` aborts the thread or process, optionally performing cleanup or logging diagnostic
    /// information before panicking or aborting.
    ///
    /// `oom` is meant to be used by clients unable to cope with an unsatisfied allocation request,
    /// and wish to abandon computation rather than attempt to recover locally. The allocator
    /// likely has more insight into why the request failed, and thus can likely print more
    /// informative diagnostic information than the client could.
    ///
    /// Implementations of the `oom` method are discouraged from infinitely regressing in nested
    /// calls to `oom`. In practice this means implementors should eschew allocating, especially
    /// from `self` (directly or indirectly).
    ///
    /// Implementions of `alloc` are discouraged from panicking (or aborting) in the event of
    /// memory exhaustion; instead they should return an error and let the client decide whether to
    /// invoke this `oom` method in response.
    fn oom(&mut self) -> ! {
        panic!()
    }
}

pub unsafe trait UntypedObjectAlloc {
    fn layout(&self) -> Layout;
    unsafe fn alloc(&mut self) -> Result<*mut u8, Exhausted>;
    unsafe fn dealloc(&mut self, x: *mut u8);
}

unsafe impl<T> UntypedObjectAlloc for ObjectAlloc<T> {
    fn layout(&self) -> Layout {
        // NOTE: This is safe because the layout method doesn't guarantee that it provides the most
        // specific layout, but rather simply that all objects returned from alloc are guaranteed
        // to abide by this layout. This particular ObjectAlloc could have been configured with a
        // more strict alignment than T's alignment, but that's OK.
        Layout::new::<T>()
    }

    unsafe fn alloc(&mut self) -> Result<*mut u8, Exhausted> {
        ObjectAlloc::alloc(self).map(|x| x as *mut u8)
    }

    unsafe fn dealloc(&mut self, x: *mut u8) {
        ObjectAlloc::dealloc(self, x as *mut T);
    }
}

unsafe impl<T, A: Alloc> ObjectAlloc<T> for A {
    unsafe fn alloc(&mut self) -> Result<*mut T, Exhausted> {
        match Alloc::alloc(self, Layout::new::<T>()) {
            Ok(ptr) => Ok(ptr as *mut T),
            Err(AllocErr::Exhausted { .. }) => Err(Exhausted),
            Err(AllocErr::Unsupported { details }) => {
                use core::intrinsics::type_name;
                panic!("Alloc does not support allocating objects of type {}: {}",
                       type_name::<T>(),
                       details)
            }
        }
    }

    unsafe fn dealloc(&mut self, x: *mut T) {
        Alloc::dealloc(self, x as *mut u8, Layout::new::<T>());
    }
}
