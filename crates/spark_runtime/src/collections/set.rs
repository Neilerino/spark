//! Set runtime storage and operations.

use std::mem::size_of;

use crate::value::{
    buffer_align, drop_elem_if_needed, eq_elem, hash_elem, retain_elem_if_needed, value_desc_clone,
    value_desc_free, SparkValueDesc,
};

use super::hash::{
    alloc_hashes, dealloc_hashes, for_occupied_slots, insertion_slot, is_occupied, lookup_slot,
    rehash_occupied_slots, InsertionSlot, LookupSlot, HASH_TOMBSTONE, MIN_HASH_CAP,
};
use super::storage::{alloc_buffer, byte_size, clone_buffer, dealloc_buffer};

// =============================================================================
// Set — type-erased open-addressing hash set with reference counting
// =============================================================================

/// Type-erased open-addressing hash set. Elements are stored as raw bytes,
/// with an FNV-1a hash per slot for O(1) amortized lookups.
///
/// The `rc` field is always at offset 0 for generic RC operations.
#[repr(C)]
pub struct SparkSet {
    /// Reference count.
    rc: usize,
    elem_desc: *mut SparkValueDesc,
    /// Hash per slot (`HASH_EMPTY` = empty, `HASH_TOMBSTONE` = deleted, `≥ 2` = occupied).
    hashes: *mut u64,
    /// Pointer to contiguous element buffer.
    data: *mut u8,
    /// Number of live elements.
    len: usize,
    /// Total number of slots (always a power of 2).
    cap: usize,
    /// Number of tombstoned slots.
    tombstones: usize,
}

impl SparkSet {
    fn elem(&self) -> &SparkValueDesc {
        unsafe { &*self.elem_desc }
    }

    fn elem_size(&self) -> usize {
        self.elem().size
    }

    fn elem_align(&self) -> usize {
        self.elem().align
    }

    fn elem_drop(&self) -> usize {
        self.elem().drop_fn
    }

    fn elem_retain(&self) -> usize {
        self.elem().retain_fn
    }

    fn elem_hash(&self) -> usize {
        self.elem().hash_fn
    }

    fn elem_eq(&self) -> usize {
        self.elem().eq_fn
    }
}

/// Creates a new empty set (rc = 1).
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub extern "C" fn __spark_set_new(elem_desc: *mut u8) -> *mut u8 {
    if elem_desc.is_null() {
        eprintln!("spark: null set element descriptor");
        std::process::abort();
    }
    let set = Box::new(SparkSet {
        rc: 1,
        elem_desc: elem_desc.cast(),
        hashes: std::ptr::null_mut(),
        data: std::ptr::null_mut(),
        len: 0,
        cap: 0,
        tombstones: 0,
    });
    Box::into_raw(set).cast::<u8>()
}

/// Inserts an element into the set if not already present.
///
/// # Safety
///
/// `set` must be a valid pointer from `__spark_set_new`.
/// `elem_ptr` must point to at least `elem_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_truncation)]
pub unsafe extern "C" fn __spark_set_insert(set: *mut u8, elem_ptr: *const u8) {
    let set = unsafe { &mut *(set.cast::<SparkSet>()) };

    // Lazily allocate or grow when load factor exceeds 75%.
    if set.cap == 0 {
        set_hash_init(set);
    } else if (set.len + set.tombstones) * 4 >= set.cap * 3 {
        set_hash_grow(set);
    }

    let h = unsafe { hash_elem(elem_ptr, set.elem_size(), set.elem_hash()) };
    let slot = unsafe {
        insertion_slot(set.hashes, set.cap, h, |idx| {
            let existing = set.data.add(idx * set.elem_size());
            eq_elem(existing, elem_ptr, set.elem_size(), set.elem_eq())
        })
    };

    match slot {
        InsertionSlot::Vacant {
            index,
            reused_tombstone,
        } => {
            if reused_tombstone {
                set.tombstones -= 1;
            }
            unsafe { *set.hashes.add(index) = h };
            let dest = unsafe { set.data.add(index * set.elem_size()) };
            unsafe { std::ptr::copy_nonoverlapping(elem_ptr, dest, set.elem_size()) };
            set.len += 1;
        }
        InsertionSlot::Occupied(_) => {
            unsafe { drop_elem_if_needed(set.elem_drop(), elem_ptr.cast_mut()) };
        }
    }
}

/// Allocates initial hash table storage for a set (capacity = [`MIN_HASH_CAP`]).
#[allow(clippy::cast_ptr_alignment)] // Buffers are 8-byte aligned by design
fn set_hash_init(set: &mut SparkSet) {
    let cap = MIN_HASH_CAP;
    let elem_align = buffer_align(set.elem_align());

    let hashes = unsafe { alloc_hashes(cap) };

    let data = unsafe {
        alloc_buffer(
            byte_size(cap, set.elem_size()),
            elem_align,
            "spark: out of memory",
        )
    };

    set.hashes = hashes;
    set.data = data;
    set.cap = cap;
}

/// Doubles the set's capacity and re-inserts all occupied entries.
#[allow(clippy::cast_ptr_alignment)] // Buffers are 8-byte aligned by design
fn set_hash_grow(set: &mut SparkSet) {
    let old_cap = set.cap;
    let old_hashes = set.hashes;
    let old_data = set.data;

    let new_cap = old_cap * 2;
    let elem_align = buffer_align(set.elem_align());

    let new_hashes = unsafe { alloc_hashes(new_cap) };

    let new_data = unsafe {
        alloc_buffer(
            byte_size(new_cap, set.elem_size()),
            elem_align,
            "spark: out of memory",
        )
    };

    unsafe {
        rehash_occupied_slots(
            old_hashes,
            old_cap,
            new_hashes,
            new_cap,
            |old_idx, new_idx| {
                let src = old_data.add(old_idx * set.elem_size());
                let dst = new_data.add(new_idx * set.elem_size());
                std::ptr::copy_nonoverlapping(src, dst, set.elem_size());
            },
        );
    }

    unsafe {
        dealloc_hashes(old_hashes, old_cap);
        dealloc_buffer(old_data, byte_size(old_cap, set.elem_size()), elem_align);
    }

    set.hashes = new_hashes;
    set.data = new_data;
    set.cap = new_cap;
    set.tombstones = 0;
}

/// Returns the number of elements in a set.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_set_new`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_wrap)]
pub unsafe extern "C" fn __spark_set_len(ptr: *mut u8) -> i64 {
    let set = unsafe { &*(ptr.cast::<SparkSet>()) };
    set.len as i64
}

/// Returns 1 if the set contains the element, 0 otherwise.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_set_new`.
/// `elem_ptr` must point to at least `elem_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_truncation)]
pub unsafe extern "C" fn __spark_set_contains(ptr: *mut u8, elem_ptr: *const u8) -> i8 {
    let set = unsafe { &*(ptr.cast::<SparkSet>()) };
    if set.cap == 0 {
        return 0;
    }

    let h = unsafe { hash_elem(elem_ptr, set.elem_size(), set.elem_hash()) };
    let slot = unsafe {
        lookup_slot(set.hashes, set.cap, h, |idx| {
            let existing = set.data.add(idx * set.elem_size());
            eq_elem(existing, elem_ptr, set.elem_size(), set.elem_eq())
        })
    };
    match slot {
        LookupSlot::Occupied(_) => 1,
        LookupSlot::Empty => 0,
    }
}

/// Removes an element from the set by tombstoning.
/// Calls the element destructor when the set descriptor has one.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_set_new`.
/// `elem_ptr` must point to at least `elem_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_truncation)]
pub unsafe extern "C" fn __spark_set_remove(ptr: *mut u8, elem_ptr: *const u8) {
    let set = unsafe { &mut *(ptr.cast::<SparkSet>()) };
    if set.cap == 0 {
        return;
    }

    let h = unsafe { hash_elem(elem_ptr, set.elem_size(), set.elem_hash()) };
    let slot = unsafe {
        lookup_slot(set.hashes, set.cap, h, |idx| {
            let existing = set.data.add(idx * set.elem_size());
            eq_elem(existing, elem_ptr, set.elem_size(), set.elem_eq())
        })
    };

    let LookupSlot::Occupied(idx) = slot else {
        return;
    };

    let existing = unsafe { set.data.add(idx * set.elem_size()) };
    if set.elem_drop() != 0 {
        let elem_drop_fn: unsafe extern "C" fn(*mut u8) =
            unsafe { std::mem::transmute(set.elem_drop()) };
        unsafe { elem_drop_fn(existing) };
    }
    unsafe { *set.hashes.add(idx) = HASH_TOMBSTONE };
    set.len -= 1;
    set.tombstones += 1;
}

/// Decrements the reference count of a `SparkSet`.
/// When the count reaches zero, calls `elem_drop` on each occupied slot
/// for recursive cleanup, then frees all buffers.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_set_new`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_set_rc_dec(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let set = unsafe { &mut *(ptr.cast::<SparkSet>()) };
    set.rc -= 1;
    if set.rc == 0 {
        // Call element destructor on each occupied slot.
        if set.elem_drop() != 0 && set.cap > 0 && !set.hashes.is_null() {
            let elem_drop_fn: unsafe extern "C" fn(*mut u8) =
                unsafe { std::mem::transmute(set.elem_drop()) };
            for i in 0..set.cap {
                if is_occupied(unsafe { *set.hashes.add(i) }) {
                    unsafe { elem_drop_fn(set.data.add(i * set.elem_size())) };
                }
            }
        }
        // Free all buffers.
        unsafe {
            dealloc_hashes(set.hashes, set.cap);
            dealloc_buffer(
                set.data,
                byte_size(set.cap, set.elem_size()),
                buffer_align(set.elem_align()),
            );
            value_desc_free(set.elem_desc);
        }
        drop(unsafe { Box::from_raw(ptr.cast::<SparkSet>()) });
    }
}

/// COW for sets. Copies both backing arrays and retains occupied elements
/// when the descriptor has retain glue.
///
/// # Safety
///
/// `ptr` must be a valid `SparkSet` pointer.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_set_cow(ptr: *mut u8) -> *mut u8 {
    if ptr.is_null() {
        return ptr;
    }
    let set = unsafe { &*(ptr.cast::<SparkSet>()) };
    if set.rc == 1 {
        return ptr;
    }
    let elem_align = buffer_align(set.elem_align());

    let hash_size = byte_size(set.cap, size_of::<u64>());
    let new_hashes = unsafe {
        clone_buffer(
            set.hashes.cast::<u8>(),
            hash_size,
            hash_size,
            std::mem::align_of::<u64>(),
            "spark: out of memory",
        )
    }
    .cast::<u64>();

    let data_buf_size = byte_size(set.cap, set.elem_size());
    let new_data = unsafe {
        clone_buffer(
            set.data,
            data_buf_size,
            data_buf_size,
            elem_align,
            "spark: out of memory",
        )
    };
    if set.elem_retain() != 0 && !new_data.is_null() {
        unsafe {
            for_occupied_slots(set.hashes, set.cap, |i| {
                let elem_slot = new_data.add(i * set.elem_size());
                retain_elem_if_needed(set.elem_retain(), elem_slot);
            });
        }
    }

    let new = Box::new(SparkSet {
        rc: 1,
        elem_desc: unsafe { value_desc_clone(set.elem_desc) },
        hashes: new_hashes,
        data: new_data,
        len: set.len,
        cap: set.cap,
        tombstones: set.tombstones,
    });
    // Decrement old rc.
    let old = unsafe { &mut *(ptr.cast::<SparkSet>()) };
    old.rc -= 1;
    Box::into_raw(new).cast::<u8>()
}
