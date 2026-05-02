//! Map runtime storage and operations.

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
// Map — type-erased open-addressing hash map with reference counting
// =============================================================================

/// Type-erased open-addressing hash map. Keys and values are stored as raw
/// bytes, with an FNV-1a hash per slot for O(1) amortized lookups.
///
/// The `rc` field is always at offset 0 for generic RC operations.
#[repr(C)]
pub struct SparkMap {
    /// Reference count.
    rc: usize,
    key_desc: *mut SparkValueDesc,
    value_desc: *mut SparkValueDesc,
    /// Hash per slot (`HASH_EMPTY` = empty, `HASH_TOMBSTONE` = deleted, `≥ 2` = occupied).
    hashes: *mut u64,
    /// Pointer to contiguous key buffer.
    keys: *mut u8,
    /// Pointer to contiguous value buffer.
    values: *mut u8,
    /// Number of live key-value pairs.
    len: usize,
    /// Total number of slots (always a power of 2).
    cap: usize,
    /// Number of tombstoned slots (used to decide when to grow).
    tombstones: usize,
}

impl SparkMap {
    fn key(&self) -> &SparkValueDesc {
        unsafe { &*self.key_desc }
    }

    fn value(&self) -> &SparkValueDesc {
        unsafe { &*self.value_desc }
    }

    fn key_size(&self) -> usize {
        self.key().size
    }

    fn value_size(&self) -> usize {
        self.value().size
    }

    fn key_align(&self) -> usize {
        self.key().align
    }

    fn value_align(&self) -> usize {
        self.value().align
    }

    fn key_drop(&self) -> usize {
        self.key().drop_fn
    }

    fn value_drop(&self) -> usize {
        self.value().drop_fn
    }

    fn key_retain(&self) -> usize {
        self.key().retain_fn
    }

    fn value_retain(&self) -> usize {
        self.value().retain_fn
    }

    fn key_hash(&self) -> usize {
        self.key().hash_fn
    }

    fn key_eq(&self) -> usize {
        self.key().eq_fn
    }
}

/// Creates a new empty map (rc = 1).
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub extern "C" fn __spark_map_new(key_desc: *mut u8, value_desc: *mut u8) -> *mut u8 {
    if key_desc.is_null() || value_desc.is_null() {
        eprintln!("spark: null map descriptor");
        std::process::abort();
    }
    let map = Box::new(SparkMap {
        rc: 1,
        key_desc: key_desc.cast(),
        value_desc: value_desc.cast(),
        hashes: std::ptr::null_mut(),
        keys: std::ptr::null_mut(),
        values: std::ptr::null_mut(),
        len: 0,
        cap: 0,
        tombstones: 0,
    });
    Box::into_raw(map).cast::<u8>()
}

/// Inserts or updates a key-value pair. Uses FNV-1a hashing with linear
/// probing for O(1) amortized lookup. Overwrites the value if the key
/// already exists, inserts a new entry otherwise.
///
/// # Safety
///
/// `map` must be a valid pointer from `__spark_map_new`.
/// `key_ptr` must point to at least `key_size` bytes.
/// `value_ptr` must point to at least `value_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_truncation)]
pub unsafe extern "C" fn __spark_map_insert(
    map: *mut u8,
    key_ptr: *const u8,
    value_ptr: *const u8,
) {
    let map = unsafe { &mut *(map.cast::<SparkMap>()) };

    // Lazily allocate or grow when load factor exceeds 75%.
    if map.cap == 0 {
        map_hash_init(map);
    } else if (map.len + map.tombstones) * 4 >= map.cap * 3 {
        map_hash_grow(map);
    }

    let h = unsafe { hash_elem(key_ptr, map.key_size(), map.key_hash()) };
    let slot = unsafe {
        insertion_slot(map.hashes, map.cap, h, |idx| {
            let existing_key = map.keys.add(idx * map.key_size());
            eq_elem(existing_key, key_ptr, map.key_size(), map.key_eq())
        })
    };

    match slot {
        InsertionSlot::Vacant {
            index,
            reused_tombstone,
        } => {
            if reused_tombstone {
                map.tombstones -= 1;
            }
            unsafe { *map.hashes.add(index) = h };
            let key_dest = unsafe { map.keys.add(index * map.key_size()) };
            let val_dest = unsafe { map.values.add(index * map.value_size()) };
            unsafe { std::ptr::copy_nonoverlapping(key_ptr, key_dest, map.key_size()) };
            unsafe { std::ptr::copy_nonoverlapping(value_ptr, val_dest, map.value_size()) };
            map.len += 1;
        }
        InsertionSlot::Occupied(index) => {
            unsafe { drop_elem_if_needed(map.key_drop(), key_ptr.cast_mut()) };

            let existing_val = unsafe { map.values.add(index * map.value_size()) };
            unsafe { drop_elem_if_needed(map.value_drop(), existing_val) };
            unsafe {
                std::ptr::copy_nonoverlapping(value_ptr, existing_val, map.value_size());
            }
        }
    }
}

/// Looks up a key and returns a pointer to its value, or null if not found.
///
/// # Safety
///
/// `map` must be a valid pointer from `__spark_map_new`.
/// `key_ptr` must point to at least `key_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_truncation)]
pub unsafe extern "C" fn __spark_map_get(map: *mut u8, key_ptr: *const u8) -> *mut u8 {
    let map = unsafe { &*(map.cast::<SparkMap>()) };
    if map.cap == 0 {
        return std::ptr::null_mut();
    }

    let h = unsafe { hash_elem(key_ptr, map.key_size(), map.key_hash()) };
    let slot = unsafe {
        lookup_slot(map.hashes, map.cap, h, |idx| {
            let existing_key = map.keys.add(idx * map.key_size());
            eq_elem(existing_key, key_ptr, map.key_size(), map.key_eq())
        })
    };
    match slot {
        LookupSlot::Occupied(idx) => unsafe { map.values.add(idx * map.value_size()) },
        LookupSlot::Empty => std::ptr::null_mut(),
    }
}

/// Allocates initial hash table storage for a map (capacity = [`MIN_HASH_CAP`]).
#[allow(clippy::cast_ptr_alignment)] // Buffers are 8-byte aligned by design
fn map_hash_init(map: &mut SparkMap) {
    let cap = MIN_HASH_CAP;
    let key_align = buffer_align(map.key_align());
    let value_align = buffer_align(map.value_align());

    let hashes = unsafe { alloc_hashes(cap) };

    let keys = unsafe {
        alloc_buffer(
            byte_size(cap, map.key_size()),
            key_align,
            "spark: out of memory",
        )
    };

    let values = unsafe {
        alloc_buffer(
            byte_size(cap, map.value_size()),
            value_align,
            "spark: out of memory",
        )
    };

    map.hashes = hashes;
    map.keys = keys;
    map.values = values;
    map.cap = cap;
}

/// Doubles the map's capacity and re-inserts all occupied entries,
/// cleaning tombstones in the process.
#[allow(clippy::cast_ptr_alignment)] // Buffers are 8-byte aligned by design
fn map_hash_grow(map: &mut SparkMap) {
    let old_cap = map.cap;
    let old_hashes = map.hashes;
    let old_keys = map.keys;
    let old_values = map.values;

    let new_cap = old_cap * 2;
    let key_align = buffer_align(map.key_align());
    let value_align = buffer_align(map.value_align());

    let new_hashes = unsafe { alloc_hashes(new_cap) };

    let new_keys = unsafe {
        alloc_buffer(
            byte_size(new_cap, map.key_size()),
            key_align,
            "spark: out of memory",
        )
    };

    let new_values = unsafe {
        alloc_buffer(
            byte_size(new_cap, map.value_size()),
            value_align,
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
                let src_key = old_keys.add(old_idx * map.key_size());
                let dst_key = new_keys.add(new_idx * map.key_size());
                std::ptr::copy_nonoverlapping(src_key, dst_key, map.key_size());

                let src_val = old_values.add(old_idx * map.value_size());
                let dst_val = new_values.add(new_idx * map.value_size());
                std::ptr::copy_nonoverlapping(src_val, dst_val, map.value_size());
            },
        );
    }

    unsafe {
        dealloc_hashes(old_hashes, old_cap);
        dealloc_buffer(old_keys, byte_size(old_cap, map.key_size()), key_align);
        dealloc_buffer(
            old_values,
            byte_size(old_cap, map.value_size()),
            value_align,
        );
    }

    map.hashes = new_hashes;
    map.keys = new_keys;
    map.values = new_values;
    map.cap = new_cap;
    map.tombstones = 0;
}

/// Returns the number of entries in a map.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_map_new`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_wrap)]
pub unsafe extern "C" fn __spark_map_len(ptr: *mut u8) -> i64 {
    let map = unsafe { &*(ptr.cast::<SparkMap>()) };
    map.len as i64
}

/// Returns 1 if the map contains the given key, 0 otherwise.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_map_new`.
/// `key_ptr` must point to at least `key_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_map_contains(ptr: *mut u8, key_ptr: *const u8) -> i8 {
    i8::from(!unsafe { __spark_map_get(ptr, key_ptr) }.is_null())
}

/// Removes the entry with the given key from the map by tombstoning.
/// Calls key/value destructors when the map descriptors have them.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_map_new`.
/// `key_ptr` must point to at least `key_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_truncation)]
pub unsafe extern "C" fn __spark_map_remove(ptr: *mut u8, key_ptr: *const u8) {
    let map = unsafe { &mut *(ptr.cast::<SparkMap>()) };
    if map.cap == 0 {
        return;
    }

    let h = unsafe { hash_elem(key_ptr, map.key_size(), map.key_hash()) };
    let slot = unsafe {
        lookup_slot(map.hashes, map.cap, h, |idx| {
            let existing_key = map.keys.add(idx * map.key_size());
            eq_elem(existing_key, key_ptr, map.key_size(), map.key_eq())
        })
    };

    let LookupSlot::Occupied(idx) = slot else {
        return;
    };

    let existing_key = unsafe { map.keys.add(idx * map.key_size()) };
    if map.key_drop() != 0 {
        let key_drop_fn: unsafe extern "C" fn(*mut u8) =
            unsafe { std::mem::transmute(map.key_drop()) };
        unsafe { key_drop_fn(existing_key) };
    }
    if map.value_drop() != 0 {
        let val_addr = unsafe { map.values.add(idx * map.value_size()) };
        let val_drop_fn: unsafe extern "C" fn(*mut u8) =
            unsafe { std::mem::transmute(map.value_drop()) };
        unsafe { val_drop_fn(val_addr) };
    }
    unsafe { *map.hashes.add(idx) = HASH_TOMBSTONE };
    map.len -= 1;
    map.tombstones += 1;
}

/// Decrements the reference count of a `SparkMap`.
/// When the count reaches zero, calls key/value destructors on each occupied
/// slot for recursive cleanup, then frees buffers.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_map_new`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_map_rc_dec(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let map = unsafe { &mut *(ptr.cast::<SparkMap>()) };
    map.rc -= 1;
    if map.rc == 0 {
        // Scan all cap slots — occupied entries are scattered across the table.
        if map.cap > 0 && !map.hashes.is_null() {
            let key_drop_fn: Option<unsafe extern "C" fn(*mut u8)> = if map.key_drop() != 0 {
                Some(unsafe {
                    std::mem::transmute::<usize, unsafe extern "C" fn(*mut u8)>(map.key_drop())
                })
            } else {
                None
            };
            let val_drop_fn: Option<unsafe extern "C" fn(*mut u8)> = if map.value_drop() != 0 {
                Some(unsafe {
                    std::mem::transmute::<usize, unsafe extern "C" fn(*mut u8)>(map.value_drop())
                })
            } else {
                None
            };
            for i in 0..map.cap {
                if is_occupied(unsafe { *map.hashes.add(i) }) {
                    if let Some(f) = key_drop_fn {
                        unsafe { f(map.keys.add(i * map.key_size())) };
                    }
                    if let Some(f) = val_drop_fn {
                        unsafe { f(map.values.add(i * map.value_size())) };
                    }
                }
            }
        }
        // Free all buffers.
        unsafe {
            dealloc_hashes(map.hashes, map.cap);
            dealloc_buffer(
                map.keys,
                byte_size(map.cap, map.key_size()),
                buffer_align(map.key_align()),
            );
            dealloc_buffer(
                map.values,
                byte_size(map.cap, map.value_size()),
                buffer_align(map.value_align()),
            );
            value_desc_free(map.key_desc);
            value_desc_free(map.value_desc);
        }
        drop(unsafe { Box::from_raw(ptr.cast::<SparkMap>()) });
    }
}

/// COW for maps. Copies all three backing arrays and retains occupied
/// keys/values when their descriptors have retain glue.
///
/// # Safety
///
/// `ptr` must be a valid `SparkMap` pointer.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_map_cow(ptr: *mut u8) -> *mut u8 {
    if ptr.is_null() {
        return ptr;
    }
    let map = unsafe { &*(ptr.cast::<SparkMap>()) };
    if map.rc == 1 {
        return ptr;
    }
    let key_align = buffer_align(map.key_align());
    let value_align = buffer_align(map.value_align());

    let hash_size = byte_size(map.cap, size_of::<u64>());
    let new_hashes = unsafe {
        clone_buffer(
            map.hashes.cast::<u8>(),
            hash_size,
            hash_size,
            std::mem::align_of::<u64>(),
            "spark: out of memory",
        )
    }
    .cast::<u64>();

    let key_buf_size = byte_size(map.cap, map.key_size());
    let new_keys = unsafe {
        clone_buffer(
            map.keys,
            key_buf_size,
            key_buf_size,
            key_align,
            "spark: out of memory",
        )
    };

    let val_buf_size = byte_size(map.cap, map.value_size());
    let new_values = unsafe {
        clone_buffer(
            map.values,
            val_buf_size,
            val_buf_size,
            value_align,
            "spark: out of memory",
        )
    };

    if (map.key_retain() != 0 || map.value_retain() != 0) && map.cap > 0 && !map.hashes.is_null() {
        unsafe {
            for_occupied_slots(map.hashes, map.cap, |i| {
                if map.key_retain() != 0 && !new_keys.is_null() {
                    let key_slot = new_keys.add(i * map.key_size());
                    retain_elem_if_needed(map.key_retain(), key_slot);
                }
                if map.value_retain() != 0 && !new_values.is_null() {
                    let value_slot = new_values.add(i * map.value_size());
                    retain_elem_if_needed(map.value_retain(), value_slot);
                }
            });
        }
    }

    let new = Box::new(SparkMap {
        rc: 1,
        key_desc: unsafe { value_desc_clone(map.key_desc) },
        value_desc: unsafe { value_desc_clone(map.value_desc) },
        hashes: new_hashes,
        keys: new_keys,
        values: new_values,
        len: map.len,
        cap: map.cap,
        tombstones: map.tombstones,
    });
    // Decrement old rc.
    let old = unsafe { &mut *(ptr.cast::<SparkMap>()) };
    old.rc -= 1;
    Box::into_raw(new).cast::<u8>()
}
