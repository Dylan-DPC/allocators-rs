// Copyright 2017 the authors. See the 'Copyright and license' section of the
// README.md file at the top-level directory of this repository.
//
// Licensed under the Apache License, Version 2.0 (the LICENSE-APACHE file) or
// the MIT license (the LICENSE-MIT file) at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Implementation of traditional `malloc`-style allocator routines based off of the `Slag`
//! allocator design.
//!
//! The primary use of this module is to provide the rudaments of a `malloc`-compatible global
//! allocator that can be used from C/C++ and Rust programs alike. The `elfc` crate that wraps
//! this one exposes such an interface. It is currently possible to use this module as a Rust
//! library, though we do not recommend it.
//!
//! # Using this Allocator from Rust
//!
//! We currently rely on some global allocator (bsalloc) to be running to service normal heap
//! allocations. As a result, this allocator cannot be used as a global allocator via the
//! `#[global_allocator]` attribute. Currently the only way around this is to use the `System`
//! allocator along with `libelfc` from the `elfc` crate loaded with `LD_PRELOAD`.
//!
//! It is also possible to use this allocator using a `Clone`-based API. As alluded to elsewhere,
//! the allocator is thread-safe and any handle on the allocator can be used to free a pointer from
//! any other handle in any other thread. If you `free` a pointer `alloc`-ed by another
//! `DynamicAllocator`, bad things will happen.
//!
//! ```rust,ignore
//! // all calls to `alloc` and `free` are unsafe
//! let mut elf = DynamicAllocator::new();
//! let ptr = elf.alloc(16) as *mut [u8; 16];
//! let mut elf_clone = elf.clone();
//! let res = thread::spawn(move || {
//!     elf_clone.alloc(24) as *mut [u8; 24]
//! }).join().unwrap();
//! elf.free(res);
//! elf.free(ptr);
//! ```
//!
//! This is probably a more limited use-case until custom allocators have better support in the
//! Rust ecosystem. Even then, we suspect most programmers using a non-global allocator will
//! instead want something more specialized, such as the `LocalAllocator` and `MagazineAllocator`
//! object-specific allocators.

use std::cmp;
use std::ptr;
use std::mem;

// One of MagazineCache and LocalCache is unused, depending on whether the 'local_cache' feature is
// enabled.
use super::sources::{MemorySource, MmapSource};
use super::slag::{compute_metadata, CoarseAllocator, DirtyFn, Metadata, PageAlloc, RevocablePipe,
                  Slag, PageCleanup};
#[allow(unused_imports)]
use super::frontends::{MagazineCache, LocalCache, DepotCache, Depot, Frontend};
use super::utils::{mmap, Lazy, TypedArray, likely};
use super::alloc_type::AllocType;

type Source = MmapSource;

pub(crate) mod global {
    //! A global malloc-style interface to interact with a `DynamicAllocator`. All of these
    //! structures are lazily initailized.
    //!
    //! One could be forgiven for thinking that this could work by simply using a global
    //! `lazy_static`-managed instance of a `DynamicAllocator` and then using thread-local storage
    //! (TLS) to store handles to this global instance. While this is essentially the architecture
    //! we use, a number of hacks have been added to ensure correctness.
    //!
    //! ## TLS Destructors
    //!
    //! Thread-local handles are stored in TLS. In their destructors, they potentially call into
    //! crossbeam code. This code too requires the use of TLS. We are not guaranteed any order in
    //! which these destructors can be run, and we have observed that crossbeam's can be run before
    //! ours, resulting in a panic.
    //!
    //! To avoid this we spawn a background thread that services `free` operations sent from
    //! threads in circumstances like this. While this is undoubtedly a code smell, it may be used
    //! in the future to collect statistics regarding the running allocator.
    //!
    //! ## Recursive `malloc` calls
    //!
    //! When used as a standard `malloc` implementation through the `elfc` crate via `LD_PRELOAD`,
    //! all calls to `malloc` and related functions will be routed through this module. The only
    //! problem is that the code that enqueues destructors for pthread TSD calls `calloc`; this
    //! causes all such calls to stack overflow.
    //!
    //! The fix for this is to use the thread-local attribute to create a thread-local boolean that
    //! indicates if the current thread's value has been initialized. If this value is false, a
    //! slower fallback algorithm is used.
    #[allow(unused_imports)]
    use super::{CoarseAllocator, DynamicAllocator, DirtyFn, ElfMalloc, MemorySource, ObjectAlloc,
                PageAlloc, TieredSizeClasses, TypedArray, AllocType, get_type, Source, AllocMap};
    use std::ptr;
    use std::cell::UnsafeCell;
    use std::mem::{ManuallyDrop, self};
    #[allow(unused_imports)]
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{channel, Sender};
    use std::sync::Mutex;
    use std::thread;

    type PA = PageAlloc<Source, ()>;
    // For debugging purposes: run a callback to eagerly dirty several pages. This is generally bad
    // for performance.
    //
    // type PA = PageAlloc<Source, BackgroundDirty>;

    unsafe fn dirty_slag(mem: *mut u8) {
        trace!("dirtying {:?}", mem);
        let usable_size = 32 << 10;
        let base_page = 4096;
        let mut cur_addr = mem.offset(base_page);
        while cur_addr < mem.offset(usable_size) {
            cur_addr = cur_addr.offset(base_page);
            (*(cur_addr as *mut AtomicUsize)).compare_and_swap(0, 1, Ordering::Relaxed);
        }
    }

    #[derive(Clone)]
    struct BackgroundDirty;
    impl DirtyFn for BackgroundDirty {
        fn dirty(_mem: *mut u8) {
            let _ = unsafe { LOCAL_DESTRUCTOR_CHAN.with(|h| h.send(Husk::Slag(_mem))).unwrap() };
        }
    }

    #[derive(Clone)]
    /// A wrapper like `DynamicAllocator` in the parent module.
    ///
    /// The reason we have a wrapper is for this module's custom `Drop` implementation, mentioned
    /// in the module documentation.
    struct GlobalAllocator {
        // GlobalAllocator's Drop implementation reads this field (using ptr::read) and sends it
        // over a channel. This invalidates the underlying memory, but of course Rust doesn't know
        // that, so if this field were of the type ElfMalloc<...>, the field's drop method would be
        // run after GlobalAllocator's drop method returned. We use ManuallyDrop to prevent that
        // from happening.
        alloc: ManuallyDrop<ElfMalloc<PA, TieredSizeClasses<ObjectAlloc<PA>>>>,
        // In some rare cases, we've observed that a thread-local GlobalAllocator is spuriously
        // dropped twice. Until we figure out why and fix it, we just detect when it's happening
        // and make the second drop call a no-op.
        dropped: bool,
    }
    unsafe impl Send for GlobalAllocator {}

    /// The type of the global instance of the allocator.
    ///
    /// This is used to create handles for TLS-stored `GlobalAllocator`s.
    struct GlobalAllocProvider {
        inner: Option<ElfMalloc<PA, TieredSizeClasses<ObjectAlloc<PA>>>>,
    }

    // We need sync to have the global allocator reference live for new threads to clone. This is
    // safe only because ElfMalloc (and PageAlloc, and TieredSizeClasses) have thread-safe clone
    // methods.
    unsafe impl Sync for GlobalAllocProvider {}
    impl GlobalAllocProvider {
        fn new() -> GlobalAllocProvider {
            GlobalAllocProvider { inner: Some(ElfMalloc::new()) }
        }
    }

    /// The type for messages sent to the background thread. These can either be arrays of size
    /// classes to be cleaned up (in the case of thread destruction) or pointers to be freed (in
    /// the case of a recursive call to `free`).
    enum Husk {
        Array(ElfMalloc<PA, TieredSizeClasses<ObjectAlloc<PA>>>),
        #[allow(dead_code)]
        Ptr(*mut u8),
        #[allow(dead_code)]
        Slag(*mut u8),
    }

    unsafe impl Send for Husk {}

    impl Drop for GlobalAllocator {
        fn drop(&mut self) {
            unsafe fn with_chan<F: FnMut(&Sender<Husk>)>(mut f: F) {
                LOCAL_DESTRUCTOR_CHAN
                    .with(|chan| f(chan))
                    .unwrap_or_else(|| {
                        let chan = DESTRUCTOR_CHAN.lock().unwrap().clone();
                        f(&chan);
                    })
            }

            // XXX: Why this check?
            //
            // We have found that for some reason, this destructor can be called more than once on
            // the same value. This could be a peculiarity of the TLS implementation, or it could
            // be a bug in the code here. Regardless; without this check there are some cases in
            // which this benchmark drops Arc-backed data-structures multiple times, leading to
            // segfaults either here or in the background thread.
            if self.dropped {
                alloc_eprintln!("{:?} dropped twice!", self as *const _);
                return;
            }
            unsafe {
                with_chan(|chan| {
                    // After we read the alloc field with ptr::read, the underlying memory should
                    // be treated as uninitialized, but Rust doesn't know this. We use ManuallyDrop
                    // to ensure that Rust doesn't try to drop the field after this method returns.
                    let dyn = ManuallyDrop::into_inner(ptr::read(&self.alloc));
                    let _ = chan.send(Husk::Array(dyn));
                });
                self.dropped = true;
            };
        }
    }

    pub unsafe fn get_layout(item: *mut u8) -> (usize /* size */, usize /* alignment */) {
        let m_block = match get_type(item) {
            // TODO(ezrosent): this duplicates some work..
            AllocType::SmallSlag | AllocType::Large => {
                with_local_or_clone(|h| {
                    (*h.get())
                        .alloc
                        .small_pages
                        .backing_memory()
                })
            }
            AllocType::BigSlag => {
                with_local_or_clone(|h| {
                    (*h.get())
                        .alloc
                        .large_pages
                        .backing_memory()
                })
            }
        };
        super::elfmalloc_get_layout(m_block, item)
    }

    fn new_handle() -> GlobalAllocator {
        GlobalAllocator {
            alloc: ManuallyDrop::new(ELF_HEAP.inner.as_ref().expect("heap uninitialized").clone()),
            dropped: false,
        }
    }

    impl Drop for GlobalAllocProvider {
        fn drop(&mut self) {
            mem::forget(self.inner.take());
        }
    }

    lazy_static! {
        static ref ELF_HEAP: GlobalAllocProvider = GlobalAllocProvider::new();
        static ref DESTRUCTOR_CHAN: Mutex<Sender<Husk>> = {
            // Background thread code: block on a channel waiting for memory reclamation messages
            // (Husks).
            let (sender, receiver) = channel();
            thread::spawn(move || unsafe {
                let mut local_alloc = new_handle();
                loop {
                    if let Ok(msg) = receiver.recv() {
                        let msg: Husk = msg;
                        match msg {
                            Husk::Array(alloc) => mem::drop(DynamicAllocator(alloc)),
                            Husk::Ptr(p) => local_alloc.alloc.free(p),
                            Husk::Slag(s) => dirty_slag(s),
                        }
                        continue
                    }
                    mem::forget(local_alloc);
                    return;
                }
            });
            Mutex::new(sender)
        };
    }

    alloc_thread_local!{ static LOCAL_DESTRUCTOR_CHAN: Sender<Husk> = DESTRUCTOR_CHAN.lock().unwrap().clone(); }
    alloc_thread_local!{ static LOCAL_ELF_HEAP: UnsafeCell<GlobalAllocator> = UnsafeCell::new(new_handle()); }

    fn with_local_or_clone<F, R>(f: F) -> R
        where F: Fn(&UnsafeCell<GlobalAllocator>) -> R
    {
        unsafe {
            alloc_tls_fast_with!(LOCAL_ELF_HEAP, h, { f(h) })
                .unwrap_or_else(|| f(&UnsafeCell::new(new_handle())))
        }
    }

    pub unsafe fn alloc(size: usize) -> *mut u8 {
        alloc_tls_fast_with!(LOCAL_ELF_HEAP, h, { (*h.get()).alloc.alloc(size) })
            .unwrap_or_else(|| super::large_alloc::alloc(size))
    }

    pub unsafe fn realloc(item: *mut u8, new_size: usize) -> *mut u8 {
        aligned_realloc(item, new_size, mem::size_of::<usize>())
    }

    pub unsafe fn aligned_realloc(item: *mut u8, new_size: usize, new_alignment: usize) -> *mut u8 {
        with_local_or_clone(|h| (*h.get()).alloc.realloc(item, new_size, new_alignment))
    }

    pub unsafe fn free(item: *mut u8) {
        alloc_tls_fast_with!(LOCAL_ELF_HEAP, h, { (*h.get()).alloc.free(item) })
            .unwrap_or_else(|| match get_type(item) {
                AllocType::Large => {
                    super::large_alloc::free(item);
                }
                AllocType::SmallSlag | AllocType::BigSlag => {
                    let chan = DESTRUCTOR_CHAN.lock().unwrap().clone();
                    let _ = chan.send(Husk::Ptr(item));
                }
            });
    }
}

/// A trait encapsulating the notion of an array of size classes for an allocator.
pub(crate) trait AllocMap<T>
where
    Self: Sized,
{
    /// The type used to index size classes.
    type Key;

    /// Create and initialize the map.
    fn init<F: FnMut(Self::Key) -> T>(start: Self::Key, n_classes: usize, f: F) -> Self {
        Self::init_conserve(start, n_classes, f).1
    }

    /// Create and initialize the map, handing back ownership of the constructor.
    fn init_conserve<F: FnMut(Self::Key) -> T>(
        start: Self::Key,
        n_classes: usize,
        f: F,
    ) -> (F, Self);

    /// Get an unchecked raw pointer to the class corresponding to `k`.
    unsafe fn get_raw(&self, k: Self::Key) -> *mut T;

    /// Get an unchecked reference to the class corresponding to `k`.
    #[cfg_attr(feature = "cargo-clippy", allow(inline_always))]
    #[inline(always)]
    unsafe fn get(&self, k: Self::Key) -> &T {
        &*self.get_raw(k)
    }

    /// Get an unchecked mutable reference to the class corresponding to `k`.
    #[cfg_attr(feature = "cargo-clippy", allow(inline_always))]
    #[inline(always)]
    unsafe fn get_mut(&mut self, k: Self::Key) -> &mut T {
        &mut *self.get_raw(k)
    }

    /// Iterate over the map's contents.
    ///
    /// This is used to clean up the contents of the map.
    fn foreach<F: Fn(*mut T)>(&self, f: F);

    /// Get the `Key` with a "maximum" value.
    ///
    /// This method is most useful when the `Key` type is a numeric type representing a "size
    /// class".
    fn max_key(&self) -> Self::Key;
}

// Note on the C API:
//
// The C allocation API guarantees a minimum alignment for all allocations. On some systems, this
// 8, while on others, 16. By default, our minimum size class is 8 bytes in size and all
// allocations are 8 byte aligned. On systems where the minimum alignment is 8, this means that we
// don't need to explicitly round up allocation size - the returned objects will always be properly
// aligned. However, on systems where the minimum alignment is 16, more work needs to be done.
// Thus, on these systems, when the "c-api" feature is enabled, we eliminate the 8-byte size class,
// making the smallest size class 16, and thus retaining this "aligned for free" property.

/// Size classes from the `scalloc` and `tcmalloc` allocators.
///
/// This includes two runs of size classes: the first (smaller) size classes are multiples of 16.
/// The larger classes are powers of two.
struct TieredSizeClasses<T> {
    // When compiling for the C API, the minimum alignment is 16 on Mac and 64-bit Windows.
    #[cfg(any(not(feature = "c-api"),
                not(any(target_os = "macos", all(windows, target_pointer_width = "64")))))]
    word_objs: Option<T>,
    small_objs: Multiples<T>,
    medium_objs: PowersOfTwo<T>,
}

impl<T> AllocMap<T> for TieredSizeClasses<T> {
    type Key = usize;
    fn init_conserve<F: FnMut(usize) -> T>(start: usize, n_classes: usize, f: F) -> (F, Self) {
        let n_small_classes = cmp::min((ELFMALLOC_SMALL_CUTOFF / MULTIPLE) - (start / MULTIPLE), n_classes / 2);
        let n_medium_classes = n_classes - n_small_classes;
        let (f2, small_classes) = Multiples::init_conserve(start, n_small_classes, f);
        // mutability is unnecessary when we don't execute the 'let word_objs = f3(8)' line
        #[allow(unused_mut)]
        let (mut f3, medium_classes) =
            PowersOfTwo::init_conserve(small_classes.max_key() + 1, n_medium_classes, f2);
        #[cfg(any(not(feature = "c-api"),
                    not(any(target_os = "macos",
                                all(windows, target_pointer_width = "64")))))]
        let word_objs = f3(8);
        (
            f3,
            TieredSizeClasses {
                // When compiling for the C API, the minimum alignment is 16 on Mac and 64-bit Windows.
                #[cfg(any(not(feature = "c-api"),
                            not(any(target_os = "macos",
                                        all(windows, target_pointer_width = "64")))))]
                word_objs: Some(word_objs),
                small_objs: small_classes,
                medium_objs: medium_classes,
            },
        )
    }

    unsafe fn get_raw(&self, n: usize) -> *mut T {
        // When compiling for the C API, the minimum alignment is 16 on Mac and 64-bit Windows.
        #[cfg(any(not(feature = "c-api"),
                    not(any(target_os = "macos",
                                all(windows, target_pointer_width = "64")))))]
        {
            if n <= 8 {
                self.word_objs.as_ref().unwrap() as *const _ as *mut T
            } else if n <= self.small_objs.max_key() {
                self.small_objs.get_raw(n)
            } else {
                self.medium_objs.get_raw(n)
            }
        }

        #[cfg(all(feature = "c-api",
                    any(target_os = "macos", all(windows, target_pointer_width = "64"))))]
        {
            if n <= self.small_objs.max_key() {
                self.small_objs.get_raw(n)
            } else {
                self.medium_objs.get_raw(n)
            }
        }
    }

    #[inline]
    fn max_key(&self) -> usize {
        self.medium_objs.max_key()
    }

    fn foreach<F: Fn(*mut T)>(&self, f: F) {
        #[cfg(any(not(feature = "c-api"),
                    not(any(target_os = "macos",
                                all(windows, target_pointer_width = "64")))))]
        {
            if let Some(r) = self.word_objs.as_ref() {
                f(r as *const _ as *mut T);
            }
        }
        self.small_objs.foreach(&f);
        self.medium_objs.foreach(f);
    }
}

// Once this can be a type parameter, it should be.
pub(crate) const MULTIPLE: usize = 16;

/// An array of size classes where sizes are multiples of 16.
pub(crate) struct Multiples<T> {
    starting_size: usize,
    max_size: usize,
    pub classes: TypedArray<T>,
}

impl<T: Clone> Clone for Multiples<T> {
    fn clone(&self) -> Self {
        Multiples::init(self.starting_size, self.classes.len(), |size| unsafe {
            self.get(size).clone()
        })
    }
}

/// Round up to the closest multiple of 16 greater than or equal to `n`.
#[inline]
fn round_up(n: usize) -> usize {
    (n + (MULTIPLE - 1)) & !(MULTIPLE - 1)
}

impl<T> AllocMap<T> for Multiples<T> {
    type Key = usize;
    fn init_conserve<F: FnMut(usize) -> T>(start: usize, n_classes: usize, mut f: F) -> (F, Self) {
        alloc_debug_assert!(n_classes >= 1);
        let starting_size = round_up(start);
        let res = Multiples {
            starting_size: starting_size,
            max_size: n_classes * MULTIPLE + starting_size - MULTIPLE,
            classes: TypedArray::new(n_classes),
        };
        let mut cur_size = res.starting_size;
        for p in res.classes.iter() {
            unsafe {
                ptr::write(p, f(cur_size));
            }
            cur_size += MULTIPLE;
        }
        alloc_debug_assert_eq!(res.max_size, cur_size - MULTIPLE);
        (f, res)
    }

    #[cfg_attr(feature = "cargo-clippy", allow(inline_always))]
    #[inline(always)]
    unsafe fn get_raw(&self, n: usize) -> *mut T {
        let class = round_up(n);
        alloc_debug_assert!(class <= self.max_size);
        self.classes.get(
            (round_up(n) - self.starting_size) / MULTIPLE,
        )
    }

    #[inline]
    fn max_key(&self) -> usize {
        self.max_size
    }

    fn foreach<F: Fn(*mut T)>(&self, f: F) {
        for class in self.classes.iter() {
            f(class)
        }
    }
}

/// Size classes that are just the powers of two.
///
/// This is useful mostly for testing purposes: it is a very simple implementation, but it can also
/// be rather wasteful.
pub(crate) struct PowersOfTwo<T> {
    starting_size: usize,
    max_size: usize,
    pub classes: TypedArray<T>,
}

impl<T: Clone> Clone for PowersOfTwo<T> {
    fn clone(&self) -> Self {
        PowersOfTwo::init(self.starting_size, self.classes.len(), |size| unsafe {
            self.get(size).clone()
        })
    }
}

impl Drop for DynamicAllocator {
    fn drop(&mut self) {
        self.0.allocs.foreach(|x| unsafe { ptr::drop_in_place(x) });
        unsafe {
            self.0.allocs.medium_objs.classes.destroy();
            self.0.allocs.small_objs.classes.destroy();
            #[cfg(any(not(feature = "c-api"),
                        not(any(target_os = "macos",
                                    all(windows, target_pointer_width = "64")))))]
            ptr::write(&mut self.0.allocs.word_objs, None);
        }
    }
}

impl<T> PowersOfTwo<T> {
    fn new(start_from: usize, n_classes: usize) -> PowersOfTwo<T> {
        PowersOfTwo {
            starting_size: start_from.next_power_of_two(),
            max_size: 0, // currently uninitialized
            classes: TypedArray::new(n_classes),
        }
    }
}

impl<T> AllocMap<T> for PowersOfTwo<T> {
    type Key = usize;
    fn init_conserve<F: FnMut(Self::Key) -> T>(
        start: usize,
        n_classes: usize,
        mut f: F,
    ) -> (F, Self) {
        let mut res = Self::new(start, n_classes);
        let mut cur_size = res.starting_size;
        unsafe {
            for item in res.classes.iter() {
                ptr::write(item, f(cur_size));
                cur_size *= 2;
            }
        }
        res.max_size = cur_size / 2;
        (f, res)
    }

    #[cfg_attr(feature = "cargo-clippy", allow(inline_always))]
    #[inline(always)]
    unsafe fn get_raw(&self, k: usize) -> *mut T {
        alloc_debug_assert!(k <= self.max_size);
        let log = (k.next_power_of_two().trailing_zeros() -
            self.starting_size.trailing_zeros()) as usize;
        alloc_debug_assert!(
            log < self.classes.len(),
            "log={} len={}",
            log,
            self.classes.len()
        );
        self.classes.get(log)
    }

    #[inline]
    fn max_key(&self) -> usize {
        self.max_size
    }

    fn foreach<F: Fn(*mut T)>(&self, f: F) {
        for class in self.classes.iter() {
            f(class)
        }
    }
}
/// A Dynamic memory allocator, instantiated with sane defaults for various `ElfMalloc` type
/// parameters.
#[derive(Clone)]
pub struct DynamicAllocator(ElfMalloc<PageAlloc<Source>, TieredSizeClasses<ObjectAlloc<PageAlloc<Source>>>>);

unsafe impl Send for DynamicAllocator {}

impl DynamicAllocator {
    pub fn new() -> Self {
        DynamicAllocator(ElfMalloc::new())
    }
    pub unsafe fn alloc(&mut self, size: usize) -> *mut u8 {
        self.0.alloc(size)
    }
    pub unsafe fn free(&mut self, item: *mut u8) {
        self.0.free(item)
    }

    pub unsafe fn realloc(&mut self, item: *mut u8, new_size: usize) -> *mut u8 {
        self.0.realloc(item, new_size, mem::size_of::<usize>())
    }

    pub unsafe fn aligned_realloc(
        &mut self,
        item: *mut u8,
        new_size: usize,
        new_alignment: usize,
    ) -> *mut u8 {
        self.0.realloc(item, new_size, new_alignment)
    }
}


// Frontends are currently feature-gated in the following fashion:

#[cfg(not(feature = "local_cache"))]
type Inner<CA> = MagazineCache<CA>;
#[cfg(feature = "local_cache")]
type Inner<CA> = LocalCache<CA>;

#[cfg(not(feature = "magazine_layer"))]
pub(crate) type ObjectAlloc<CA> = Lazy<Inner<CA>>;
#[cfg(feature = "magazine_layer")]
pub(crate) type ObjectAlloc<CA> = Lazy<DepotCache<Inner<CA>>>;


/// A Dynamic memory allocator, parmetrized on a particular `ObjectAlloc`, `CourseAllocator` and
/// `AllocMap`.
///
/// `ElfMalloc` encapsulates the logic of constructing and selecting object classes, as well as
/// delgating to the `large_alloc` module for large allocations. Most of the logic occurs in its
/// type parameters.
struct ElfMalloc<CA: CoarseAllocator, AM: AllocMap<ObjectAlloc<CA>>> {
    /// A cache of pages for all small allocations.
    small_pages: CA,
    /// A cache of pages for all medium allocations.
    large_pages: CA,
    /// An `AllocMap` of size classes of individual fixed-size object allocator.
    allocs: AM,
    /// The maximum size of a "non-large" object. Objects larger than `max_size` are allocated
    /// directly with mmap.
    max_size: usize,

    start_from: usize,
    n_classes: usize,
}

impl Default for DynamicAllocator {
    fn default() -> Self {
        Self::new()
    }
}

// TODO(ezrosent): move this to a type parameter when const generics are in.
const ELFMALLOC_PAGE_SIZE: usize = 2 << 20;
const ELFMALLOC_SMALL_PAGE_SIZE: usize = 256 << 10;
const ELFMALLOC_SMALL_CUTOFF: usize = ELFMALLOC_SMALL_PAGE_SIZE / 4;

impl<M: MemorySource, D: DirtyFn>
    ElfMalloc<PageAlloc<M, D>, TieredSizeClasses<ObjectAlloc<PageAlloc<M, D>>>> {
    fn new() -> Self {
        let pa_large = PageAlloc::new(ELFMALLOC_PAGE_SIZE, 1 << 20, 8, AllocType::BigSlag);
        // The small pages are allocated in groups where the first page is aligned to
        // ELFMALLOC_PAGE_SIZE; this page will be stamped with AllocType::SmallSlag, allowing type
        // lookups to work as expected.
        let pa_small = PageAlloc::new_aligned(
            ELFMALLOC_SMALL_PAGE_SIZE,
            1 << 20,
            8,
            ELFMALLOC_PAGE_SIZE,
            AllocType::SmallSlag,
        );
        Self::new_internal(0.6, pa_small, pa_large, 8, 25)
    }
}

#[inline(always)]
unsafe fn round_to_page<T>(item: *mut T) -> *mut T {
    ((item as usize) & !(ELFMALLOC_PAGE_SIZE - 1)) as *mut T
}

/// We ensure that for every pointer returned from a call to `alloc`, rounding that pointer down to
/// a 2MiB boundary yields the location of an `AllocType`. This is enforced separately in the
/// `PageAlloc` code, the `large_alloc` code, and the `Slag` code.
///
/// All of this allows us to run elfmalloc with a full malloc-style interface without resorting to
/// any sort of global ownership check on the underlying `MemorySource`. This method thus breaks
/// our dependency on the `Creek`.
#[inline(always)]
unsafe fn get_type(item: *mut u8) -> AllocType {
    *round_to_page(item.offset(-1) as *mut AllocType)
}

impl<M: MemorySource, D: DirtyFn, AM: AllocMap<ObjectAlloc<PageAlloc<M, D>>, Key = usize>> Clone
    for ElfMalloc<PageAlloc<M, D>, AM> {
    fn clone(&self) -> Self {
        let new_map = AM::init(self.start_from, self.n_classes, |size: usize| unsafe {
            self.allocs.get(size).clone()
        });
        ElfMalloc {
            small_pages: self.small_pages.clone(),
            large_pages: self.large_pages.clone(),
            allocs: new_map,
            max_size: self.max_size,
            start_from: self.start_from,
            n_classes: self.n_classes,
        }
    }
}

unsafe fn elfmalloc_get_layout<M: MemorySource>(m_block: &M, item: *mut u8) -> (usize, usize) {
    match get_type(item) {
        AllocType::SmallSlag | AllocType::BigSlag => {
            let meta = (*Slag::find(item, m_block.page_size())).get_metadata();
            (
                meta.object_size,
                if meta.object_size.is_power_of_two() {
                    meta.object_size
                } else {
                    mem::size_of::<usize>()
                },
            )
        }
        AllocType::Large => (large_alloc::get_size(item), mmap::page_size()),
    }
}

impl<M: MemorySource, D: DirtyFn, AM: AllocMap<ObjectAlloc<PageAlloc<M, D>>, Key = usize>>
    ElfMalloc<PageAlloc<M, D>, AM> {
    fn new_internal(
        // usable_size: usize,
        cutoff_factor: f64,
        pa_small: PageAlloc<M, D>,
        pa_large: PageAlloc<M, D>,
        start_from: usize,
        n_classes: usize,
    ) -> Self {
        use self::mmap::map;
        let mut meta_pointer = map(mem::size_of::<Metadata>() * n_classes) as *mut Metadata;
        let small_page_size = pa_small.backing_memory().page_size();
        let am = AM::init(start_from, n_classes, |size: usize| {
            let (u_size, pa, ty) = if size < ELFMALLOC_SMALL_CUTOFF {
                (small_page_size, pa_small.clone(), AllocType::SmallSlag)
            } else {
                (
                    pa_large.backing_memory().page_size(),
                    pa_large.clone(),
                    AllocType::BigSlag,
                )
            };
            let m_ptr = meta_pointer;
            unsafe {
                meta_pointer = meta_pointer.offset(1);
                ptr::write(
                    m_ptr,
                    compute_metadata(
                        size,
                        pa.backing_memory().page_size(),
                        0,
                        cutoff_factor,
                        u_size,
                        ty,
                    ),
                );
            }
            let clean = PageCleanup::new(pa.backing_memory().page_size());
            // TODO(ezrosent); new_size(8) is a good default, but a better one would take
            // num_cpus::get() into account when picking this size, as in principle this will run
            // into scaling limits at some point.
            let params = (
                m_ptr,
                1 << 20,
                pa,
                RevocablePipe::new_size_cleanup(16, clean),
            );
            #[cfg(not(feature = "magazine_layer"))]
            {
                ObjectAlloc::new(params)
            }
            #[cfg(feature = "magazine_layer")]
            {
                ObjectAlloc::new((params, Depot::default()))
            }
        });
        let max_size = am.max_key();
        ElfMalloc {
            small_pages: pa_small.clone(),
            large_pages: pa_large.clone(),
            allocs: am,
            max_size: max_size,
            start_from: start_from,
            n_classes: n_classes,
        }
    }

    #[inline]
    unsafe fn get_page_size(&self, item: *mut u8) -> Option<usize> {
        // We have carfeully orchestrated things so that allocation sizes above the cutoff are
        // aligned to at least that cutoff:
        // - Medium objects are powers of two, all of which are aligned to their size.
        // - Large objects are allocated using an MmapSource with page size equivalent to the
        //   cutoff.
        // As a result, we do not have to dereference an extra pointer for small objects that are
        // not aligned to the small cutoff (this is going to be most of them). This netted
        // small-but-noticeable performance gains.
        if (item as usize) % ELFMALLOC_SMALL_CUTOFF != 0 {
            return Some(ELFMALLOC_SMALL_PAGE_SIZE);
        }
        match get_type(item) {
            AllocType::SmallSlag => {
                alloc_debug_assert_eq!(self.small_pages.backing_memory().page_size(), ELFMALLOC_SMALL_PAGE_SIZE);
                Some(ELFMALLOC_SMALL_PAGE_SIZE)
            },
            AllocType::BigSlag => {
                alloc_debug_assert_eq!(self.large_pages.backing_memory().page_size(), ELFMALLOC_PAGE_SIZE);
                Some(ELFMALLOC_PAGE_SIZE)
            },
            AllocType::Large => None,
        }
    }

    unsafe fn alloc(&mut self, bytes: usize) -> *mut u8 {
        if likely(bytes <= self.max_size) {
            self.allocs.get_mut(bytes).alloc()
        } else {
            large_alloc::alloc(bytes)
        }
    }

    unsafe fn realloc(
        &mut self,
        item: *mut u8,
        mut new_size: usize,
        new_alignment: usize,
    ) -> *mut u8 {
        if item.is_null() {
            let alloc_size = if new_alignment <= mem::size_of::<usize>() {
                new_size
            } else {
                new_size.next_power_of_two()
            };
            return self.alloc(alloc_size);
        }
        if new_size == 0 {
            self.free(item);
            return ptr::null_mut();
        }
        let (old_size, old_alignment) = global::get_layout(item);
        if old_alignment >= new_alignment && old_size >= new_size {
            return item;
        }
        if new_alignment > mem::size_of::<usize>() {
            new_size = new_size.next_power_of_two();
        }
        let new_mem = self.alloc(new_size);
        ptr::copy_nonoverlapping(item, new_mem, ::std::cmp::min(old_size, new_size));
        self.free(item);
        #[cfg(debug_assertions)]
        {
            let (size, _) = global::get_layout(new_mem);
            alloc_debug_assert!(new_size <= size, "Realloc for {} got memory with size {}", new_size, size);
        }
        new_mem
    }

    unsafe fn free(&mut self, item: *mut u8) {
        match self.get_page_size(item) {
            Some(page_size) => {
                let slag = &*Slag::find(item, page_size);
                self.allocs.get_mut(slag.get_metadata().object_size).free(
                    item,
                )
            }
            None => large_alloc::free(item),
        };
    }
}

mod large_alloc {
    //! This module governs "large" allocations that are beyond the size of the largest size class
    //! of a dynamic allocator.
    //!
    //! Large allocations are implemented by mapping a region of memory of the indicated size, with
    //! an additional page of padding to store the size information.
    #[cfg(test)]
    use std::collections::HashMap;
    #[cfg(test)]
    use std::cell::RefCell;
    use std::cmp;
    use std::ptr;
    use super::super::sources::{MemorySource, MmapSource};
    use super::{ELFMALLOC_PAGE_SIZE, ELFMALLOC_SMALL_CUTOFF, round_to_page};
    use super::super::alloc_type::AllocType;

    // For debugging, we keep around a thread-local map of pointers to lengths. This helps us
    // scrutinize if various header data is getting propagated correctly.
    #[cfg(test)]
    thread_local! {
        pub static SEEN_PTRS: RefCell<HashMap<*mut u8, usize>> = RefCell::new(HashMap::new());
    }
    use super::mmap::unmap;
    #[cfg(debug_assertions)]
    use super::mmap::page_size;

    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct AllocInfo {
        pub ty: AllocType,
        base: *mut u8,
        region_size: usize,
    }

    pub unsafe fn alloc(size: usize) -> *mut u8 {
        // TODO(ezrosent) round up to page size
        let region_size = size + ELFMALLOC_PAGE_SIZE;
        // We need a pointer aligned to the SMALL_CUTOFF, so we use an `MmapSource` to map the
        // memory. See the comment in get_page_size.
        let src = MmapSource::new(ELFMALLOC_SMALL_CUTOFF);
        let n_pages = region_size / ELFMALLOC_SMALL_CUTOFF + cmp::min(1, region_size % ELFMALLOC_SMALL_CUTOFF);
        let mem = src.carve(n_pages).expect("[lage_alloc::alloc] mmap failed");
        let res = mem.offset(ELFMALLOC_PAGE_SIZE as isize);
        let addr = get_commitment_mut(res);
        ptr::write(
            addr,
            AllocInfo {
                ty: AllocType::Large,
                base: mem,
                region_size: region_size,
            },
        );

        // begin extra debugging information
        alloc_debug_assert!(!mem.is_null());
        alloc_debug_assert_eq!(mem as usize % ELFMALLOC_SMALL_CUTOFF, 0);
        let upage: usize = 4096;
        alloc_debug_assert_eq!(mem as usize % upage, 0);
        alloc_debug_assert_eq!(res as usize % upage, 0);
        alloc_debug_assert_eq!(get_commitment(res), (size + ELFMALLOC_PAGE_SIZE, mem));
        #[cfg(test)] SEEN_PTRS.with(|hs| hs.borrow_mut().insert(mem, region_size));
        // end extra debugging information
        res
    }

    pub unsafe fn free(item: *mut u8) {
        let (size, base_ptr) = get_commitment(item);
        use std::intrinsics::unlikely;
        if unlikely(size == 0 && base_ptr.is_null()) {
            return;
        }

        trace!("size={}, base_ptr={:?}", size, base_ptr);
        // begin extra debugging information:
        #[cfg(debug_assertions)]
        {
            ptr::write_volatile(item, 10);
            alloc_debug_assert_eq!(
                base_ptr as usize % page_size(),
                0,
                "base_ptr ({:?}) not a multiple of the page size ({})",
                base_ptr,
                page_size()
            );
        }
        #[cfg(test)]
        {
            SEEN_PTRS.with(|hm| {
                let mut hmap = hm.borrow_mut();
                {
                    if let Some(len) = hmap.get(&base_ptr) {
                        alloc_assert_eq!(*len, size);
                    }
                }
                hmap.remove(&base_ptr);
            });
        }
        // end extra debugging information
        unmap(base_ptr, size);
    }

    pub unsafe fn get_size(item: *mut u8) -> usize {
        let (size, _) = get_commitment(item);
        size - ELFMALLOC_PAGE_SIZE
    }

    unsafe fn get_commitment(item: *mut u8) -> (usize, *mut u8) {
        let meta_addr = get_commitment_mut(item);
        let base_ptr = (*meta_addr).base;
        let size = (*meta_addr).region_size;
        (size, base_ptr)
    }

    pub unsafe fn get_commitment_mut(item: *mut u8) -> *mut AllocInfo {
        round_to_page(item.offset(-1) as *mut AllocInfo)
    }
}

#[cfg(test)]
mod tests {
    extern crate env_logger;
    use super::*;
    use std::ptr::{write_bytes, write_volatile};


    #[test]
    fn layout_lookup() {
        fn test_and_free<F: Fn(usize, usize)>(inp: usize, tester: F) {
            unsafe {
                let obj = global::alloc(inp);
                let (size, align) = global::get_layout(obj);
                tester(size, align);
                global::free(obj);
            }
        }

        test_and_free(8, |size, align| {
            alloc_assert!(size >= 8);
            alloc_assert!(align >= 8);
        });
        test_and_free(24, |size, align| {
            alloc_assert!(size >= 24);
            alloc_assert!(align >= 8);
        });
        test_and_free(512, |size, align| {
            alloc_assert!(size >= 512);
            alloc_assert!(align >= 512);
        });
        test_and_free(4 << 20, |size, align| {
            alloc_assert_eq!((size, align), (4 << 20, mmap::page_size()))
        });
    }

    #[test]
    fn general_alloc_basic_global_single_threaded() {
        let _ = env_logger::init();
        for size in ((1 << 13) - 8)..((1 << 13) + 1) {
            unsafe {
                let item = global::alloc(size * 8);
                write_volatile(item, 10);
                global::free(item);
            }
        }
    }

    #[test]
    fn general_alloc_basic_clone_single_threaded() {
        let _ = env_logger::init();
        let da_c = DynamicAllocator::new();
        let mut da = da_c.clone();
        for size in ((1 << 13) - 8)..((1 << 13) + 1) {
            unsafe {
                let item = da.alloc(size * 8);
                write_volatile(item, 10);
                da.free(item);
            }
        }
    }

    #[test]
    fn general_alloc_basic_global_many_threads() {
        let _ = env_logger::init();
        use std::thread;

        const N_THREADS: usize = 32;
        let mut threads = Vec::with_capacity(N_THREADS);
        for t in 0..N_THREADS {
            threads.push(
                thread::Builder::new()
                    .name(t.to_string())
                    .spawn(move || {
                        for size in 1..(1 << 13) {
                            // ((1 << 9) + 1)..((1 << 18) + 1) {
                            unsafe {
                                let item = global::alloc(size * 8);
                                write_volatile(item, 10);
                                global::free(item);
                            }
                            if size * 8 >= (1 << 20) {
                                return;
                            }
                        }
                    })
                    .unwrap(),
            );
        }

        for t in threads {
            t.join().expect("threads should exit successfully")
        }
    }

    #[test]
    fn general_alloc_large_ws_global_many_threads() {
        let _ = env_logger::init();
        use std::thread;

        const N_THREADS: usize = 32;
        let mut threads = Vec::with_capacity(N_THREADS);
        for t in 0..N_THREADS {
            threads.push(
                thread::Builder::new()
                    .name(t.to_string())
                    .spawn(move || unsafe {
                        for _ in 0..2 {
                            let ptrs: Vec<*mut u8> =
                                (0..(1 << 20)).map(|_| global::alloc(8)).collect();
                            for p in ptrs {
                                global::free(p);
                            }
                        }
                    })
                    .unwrap(),
            );
        }

        for t in threads {
            t.join().expect("threads should exit successfully")
        }
    }

    #[test]
    fn realloc_basic() {
        let _ = env_logger::init();
        use std::thread;
        const N_THREADS: usize = 8;
        let alloc = DynamicAllocator::new();
        let mut threads = Vec::with_capacity(N_THREADS);
        for t in 0..N_THREADS {
            let mut da = alloc.clone();
            threads.push(
                thread::Builder::new()
                    .name(t.to_string())
                    .spawn(move || for size in 1..(1 << 13) {
                        const N_ITERS: usize = 8;
                        let mut v1 = Vec::with_capacity(N_ITERS);
                        let alloc_size = size * 8;
                        if alloc_size >= (1 << 20) {
                            return;
                        }
                        unsafe {
                            for _ in 0..N_ITERS {
                                let item = da.alloc(alloc_size);
                                write_bytes(item, 0xFF, alloc_size);
                                v1.push(item);
                            }
                            for i in 0..N_ITERS {
                                let item = v1[i];
                                let new_item = da.aligned_realloc(
                                    item, alloc_size * 2, if size % 2 == 0 {
                                        8
                                    } else {
                                        (alloc_size * 2).next_power_of_two()
                                    });
                                write_bytes(new_item.offset(alloc_size as isize), 0xFE, alloc_size);
                                da.free(new_item);
                            }
                        }
                    }).unwrap(),
            );
        }
        for t in threads {
            t.join().expect("threads should exit successfully")
        }
    }

    #[test]
    fn general_alloc_basic_clone_many_threads() {
        let _ = env_logger::init();
        use std::thread;

        const N_THREADS: usize = 32;
        let alloc = DynamicAllocator::new();
        let mut threads = Vec::with_capacity(N_THREADS);
        for t in 0..N_THREADS {
            let mut da = alloc.clone();
            threads.push(
                thread::Builder::new()
                    .name(t.to_string())
                    .spawn(move || {
                        for size in 1..(1 << 13) {
                            // ((1 << 9) + 1)..((1 << 18) + 1) {
                            unsafe {
                                let item = da.alloc(size * 8);
                                write_bytes(item, 0xFF, size * 8);
                                da.free(item);
                            }
                            if size * 8 >= (1 << 20) {
                                return;
                            }
                        }
                    })
                    .unwrap(),
            );
        }

        for t in threads {
            t.join().expect("threads should exit successfully")
        }
    }

    #[test]
    fn all_sizes_one_thread() {
        let _ = env_logger::init();
        for size in 1..((1 << 21) + 1) {
            unsafe {
                let item = global::alloc(size);
                write_volatile(item, 10);
                global::free(item);
                if size + 2 > 1 << 20 {
                    return;
                }
            }
        }
    }
}
