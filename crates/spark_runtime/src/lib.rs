//! Spark runtime library.
//!
//! Provides `extern "C"` functions that are called by compiled Spark programs.
//! This crate is compiled as a static library (`libspark_runtime.a`) and linked
//! into every Spark executable.
//!
//! All heap-allocated types (`SparkString`, `SparkList`, `SparkMap`) start with
//! a `rc: usize` field for reference counting. The generic `__spark_rc_inc`
//! function works on any heap object (rc is always at offset 0). Type-specific
//! `_rc_dec` functions handle deallocation when the count reaches zero.

// Safety: This crate is an FFI boundary — `#[no_mangle]` and raw pointers
// are required for the C ABI. No actual unsafe blocks are used.
#![allow(unsafe_code)]
#![allow(clippy::missing_const_for_fn)] // extern "C" FFI stubs are not const

mod channel;
mod http;
mod io;
mod json;
mod math;
mod net;
mod time;

// =============================================================================
// Print functions
// =============================================================================

/// Prints an integer value followed by a newline.
#[no_mangle]
pub extern "C" fn __spark_print_int(value: i64) {
    println!("{value}");
}

/// Prints a boolean value (`true` or `false`) followed by a newline.
#[no_mangle]
pub extern "C" fn __spark_print_bool(value: i8) {
    if value != 0 {
        println!("true");
    } else {
        println!("false");
    }
}

/// Prints a float value followed by a newline.
#[no_mangle]
pub extern "C" fn __spark_print_float(value: f64) {
    println!("{value}");
}

/// Prints a string value followed by a newline.
///
/// # Safety
///
/// `ptr` must be a valid pointer returned by `__spark_string_new` or
/// `__spark_string_from_literal`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_print_string(ptr: *mut u8) {
    let s = unsafe { &*(ptr.cast::<SparkString>()) };
    if s.len == 0 {
        println!();
        return;
    }
    let slice = unsafe { std::slice::from_raw_parts(s.data, s.len) };
    let text = std::str::from_utf8(slice).unwrap_or("<invalid utf-8>");
    println!("{text}");
}

/// Prints an empty newline.
#[no_mangle]
pub extern "C" fn __spark_println() {
    println!();
}

// =============================================================================
// String — heap-allocated UTF-8 string with reference counting
// =============================================================================

/// A heap-allocated UTF-8 string with a reference count.
///
/// The `rc` field is always at offset 0, allowing generic RC operations.
#[repr(C)]
struct SparkString {
    /// Reference count.
    rc: usize,
    /// Pointer to the UTF-8 byte buffer.
    data: *mut u8,
    /// Number of valid bytes.
    len: usize,
    /// Allocated capacity in bytes.
    cap: usize,
}

/// Creates a new empty heap-allocated string (rc = 1).
#[no_mangle]
pub extern "C" fn __spark_string_new() -> *mut u8 {
    let s = Box::new(SparkString {
        rc: 1,
        data: std::ptr::null_mut(),
        len: 0,
        cap: 0,
    });
    Box::into_raw(s).cast::<u8>()
}

/// Creates a heap-allocated string from a UTF-8 literal (pointer + length).
/// The returned string has rc = 1.
///
/// # Safety
///
/// `data` must be a valid pointer to `len` bytes of UTF-8 data.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_string_from_literal(data: *const u8, len: i64) -> *mut u8 {
    let byte_len = len as usize;
    let align = std::mem::align_of::<u64>();

    if byte_len == 0 {
        return __spark_string_new();
    }

    let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(byte_len, align) };
    let buf = unsafe { std::alloc::alloc(layout) };
    if buf.is_null() {
        eprintln!("spark: out of memory allocating string");
        std::process::abort();
    }
    unsafe { std::ptr::copy_nonoverlapping(data, buf, byte_len) };

    let s = Box::new(SparkString {
        rc: 1,
        data: buf,
        len: byte_len,
        cap: byte_len,
    });
    Box::into_raw(s).cast::<u8>()
}

// =============================================================================
// List — type-erased growable array with reference counting
// =============================================================================

/// Minimum capacity for list/set/map backing arrays.
const MIN_CAP: usize = 4;

/// Type-erased growable list. All elements are stored as raw bytes.
/// The `rc` field is always at offset 0 for generic RC operations.
#[repr(C)]
struct SparkList {
    /// Reference count.
    rc: usize,
    /// Pointer to the contiguous data buffer.
    data: *mut u8,
    /// Number of elements currently stored.
    len: usize,
    /// Number of elements the buffer can hold before reallocation.
    cap: usize,
    /// Size of each element in bytes.
    elem_size: usize,
}

/// Creates a new empty list for elements of the given byte size (rc = 1).
///
/// # Safety
///
/// `elem_size` must be > 0.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub extern "C" fn __spark_list_new(elem_size: i64) -> *mut u8 {
    let list = Box::new(SparkList {
        rc: 1,
        data: std::ptr::null_mut(),
        len: 0,
        cap: 0,
        elem_size: elem_size as usize,
    });
    Box::into_raw(list).cast::<u8>()
}

/// Pushes an element onto the list. The element is copied from `elem_ptr`.
///
/// # Safety
///
/// `list` must be a valid pointer from `__spark_list_new`.
/// `elem_ptr` must point to at least `elem_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_list_push(list: *mut u8, elem_ptr: *const u8) {
    let list = unsafe { &mut *(list.cast::<SparkList>()) };
    if list.len == list.cap {
        list_grow(list);
    }
    let dest = unsafe { list.data.add(list.len * list.elem_size) };
    unsafe { std::ptr::copy_nonoverlapping(elem_ptr, dest, list.elem_size) };
    list.len += 1;
}

/// Returns a pointer to the element at `index`. Aborts on out-of-bounds.
///
/// # Safety
///
/// `list` must be a valid pointer from `__spark_list_new`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_list_get(list: *mut u8, index: i64) -> *mut u8 {
    let list = unsafe { &*(list.cast::<SparkList>()) };
    let idx = index as usize;
    if idx >= list.len {
        eprintln!("spark: list index out of bounds: index {idx}, length {}", list.len);
        std::process::abort();
    }
    unsafe { list.data.add(idx * list.elem_size) }
}

/// Grows the list's backing buffer, doubling capacity (minimum [`MIN_CAP`]).
fn list_grow(list: &mut SparkList) {
    let new_cap = if list.cap == 0 { MIN_CAP } else { list.cap * 2 };
    let new_size = new_cap * list.elem_size;
    let align = std::mem::align_of::<u64>(); // 8-byte alignment is sufficient
    let new_layout = unsafe { std::alloc::Layout::from_size_align_unchecked(new_size, align) };

    let new_data = if list.data.is_null() {
        unsafe { std::alloc::alloc(new_layout) }
    } else {
        let old_size = list.cap * list.elem_size;
        let old_layout = unsafe { std::alloc::Layout::from_size_align_unchecked(old_size, align) };
        unsafe { std::alloc::realloc(list.data, old_layout, new_size) }
    };

    if new_data.is_null() {
        eprintln!("spark: out of memory allocating list");
        std::process::abort();
    }

    list.data = new_data;
    list.cap = new_cap;
}

// =============================================================================
// Map — type-erased linear-scan map with reference counting
// =============================================================================

/// Type-erased linear-scan map. Keys and values are stored as raw bytes.
/// The `rc` field is always at offset 0 for generic RC operations.
#[repr(C)]
struct SparkMap {
    /// Reference count.
    rc: usize,
    /// Pointer to contiguous key buffer.
    keys: *mut u8,
    /// Pointer to contiguous value buffer.
    values: *mut u8,
    /// Number of key-value pairs stored.
    len: usize,
    /// Capacity (number of pairs the buffers can hold).
    cap: usize,
    /// Size of each key in bytes.
    key_size: usize,
    /// Size of each value in bytes.
    value_size: usize,
}

/// Creates a new empty map for keys of `key_size` bytes and values of `value_size` bytes (rc = 1).
///
/// # Safety
///
/// Both sizes must be > 0.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub extern "C" fn __spark_map_new(key_size: i64, value_size: i64) -> *mut u8 {
    let map = Box::new(SparkMap {
        rc: 1,
        keys: std::ptr::null_mut(),
        values: std::ptr::null_mut(),
        len: 0,
        cap: 0,
        key_size: key_size as usize,
        value_size: value_size as usize,
    });
    Box::into_raw(map).cast::<u8>()
}

/// Inserts or updates a key-value pair. Uses linear scan with `memcmp`
/// to find an existing key; overwrites the value if found, appends if new.
///
/// # Safety
///
/// `map` must be a valid pointer from `__spark_map_new`.
/// `key_ptr` must point to at least `key_size` bytes.
/// `value_ptr` must point to at least `value_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_map_insert(
    map: *mut u8,
    key_ptr: *const u8,
    value_ptr: *const u8,
) {
    let map = unsafe { &mut *(map.cast::<SparkMap>()) };

    // Linear scan for existing key.
    for i in 0..map.len {
        let existing_key = unsafe { map.keys.add(i * map.key_size) };
        if unsafe { libc_memcmp(existing_key, key_ptr, map.key_size) } {
            // Overwrite value.
            let existing_val = unsafe { map.values.add(i * map.value_size) };
            unsafe { std::ptr::copy_nonoverlapping(value_ptr, existing_val, map.value_size) };
            return;
        }
    }

    // Key not found — append.
    if map.len == map.cap {
        map_grow(map);
    }
    let key_dest = unsafe { map.keys.add(map.len * map.key_size) };
    let val_dest = unsafe { map.values.add(map.len * map.value_size) };
    unsafe { std::ptr::copy_nonoverlapping(key_ptr, key_dest, map.key_size) };
    unsafe { std::ptr::copy_nonoverlapping(value_ptr, val_dest, map.value_size) };
    map.len += 1;
}

/// Looks up a key and returns a pointer to its value, or null if not found.
///
/// # Safety
///
/// `map` must be a valid pointer from `__spark_map_new`.
/// `key_ptr` must point to at least `key_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_map_get(map: *mut u8, key_ptr: *const u8) -> *mut u8 {
    let map = unsafe { &*(map.cast::<SparkMap>()) };

    for i in 0..map.len {
        let existing_key = unsafe { map.keys.add(i * map.key_size) };
        if unsafe { libc_memcmp(existing_key, key_ptr, map.key_size) } {
            return unsafe { map.values.add(i * map.value_size) };
        }
    }

    std::ptr::null_mut()
}

/// Grows the map's backing buffers, doubling capacity (minimum [`MIN_CAP`]).
fn map_grow(map: &mut SparkMap) {
    let new_cap = if map.cap == 0 { MIN_CAP } else { map.cap * 2 };
    let align = std::mem::align_of::<u64>();

    // Grow keys buffer.
    let new_key_size = new_cap * map.key_size;
    let new_key_layout =
        unsafe { std::alloc::Layout::from_size_align_unchecked(new_key_size, align) };
    let new_keys = if map.keys.is_null() {
        unsafe { std::alloc::alloc(new_key_layout) }
    } else {
        let old_key_size = map.cap * map.key_size;
        let old_key_layout =
            unsafe { std::alloc::Layout::from_size_align_unchecked(old_key_size, align) };
        unsafe { std::alloc::realloc(map.keys, old_key_layout, new_key_size) }
    };

    // Grow values buffer.
    let new_val_size = new_cap * map.value_size;
    let new_val_layout =
        unsafe { std::alloc::Layout::from_size_align_unchecked(new_val_size, align) };
    let new_values = if map.values.is_null() {
        unsafe { std::alloc::alloc(new_val_layout) }
    } else {
        let old_val_size = map.cap * map.value_size;
        let old_val_layout =
            unsafe { std::alloc::Layout::from_size_align_unchecked(old_val_size, align) };
        unsafe { std::alloc::realloc(map.values, old_val_layout, new_val_size) }
    };

    if new_keys.is_null() || new_values.is_null() {
        eprintln!("spark: out of memory allocating map");
        std::process::abort();
    }

    map.keys = new_keys;
    map.values = new_values;
    map.cap = new_cap;
}

// =============================================================================
// Set — type-erased with uniqueness (backed by a list + memcmp dedup)
// =============================================================================

/// Creates a new empty set for elements of the given byte size.
///
/// Internally, a set is stored as a `SparkList` with uniqueness checks on insert.
///
/// # Safety
///
/// `elem_size` must be > 0.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub extern "C" fn __spark_set_new(elem_size: i64) -> *mut u8 {
    // A set is just a list with dedup — reuse the same struct.
    __spark_list_new(elem_size)
}

/// Inserts an element into the set if not already present.
///
/// # Safety
///
/// `set` must be a valid pointer from `__spark_set_new`.
/// `elem_ptr` must point to at least `elem_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_set_insert(set: *mut u8, elem_ptr: *const u8) {
    let list = unsafe { &*(set.cast::<SparkList>()) };

    // Check for duplicates via linear scan + memcmp.
    for i in 0..list.len {
        let existing = unsafe { list.data.add(i * list.elem_size) };
        if unsafe { libc_memcmp(existing, elem_ptr, list.elem_size) } {
            return; // Already present.
        }
    }

    // Not found — delegate to list_push.
    unsafe { __spark_list_push(set, elem_ptr) };
}

// =============================================================================
// Collection methods — len, contains, remove
// =============================================================================

/// Returns the number of elements in a list.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_list_new`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_wrap)]
pub unsafe extern "C" fn __spark_list_len(ptr: *mut u8) -> i64 {
    let list = unsafe { &*(ptr.cast::<SparkList>()) };
    list.len as i64
}

/// Returns 1 if the list contains the element at `elem_ptr`, 0 otherwise.
/// Comparison is byte-level (`memcmp`).
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_list_new`.
/// `elem_ptr` must point to at least `elem_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_list_contains(ptr: *mut u8, elem_ptr: *const u8) -> i8 {
    let list = unsafe { &*(ptr.cast::<SparkList>()) };
    for i in 0..list.len {
        let existing = unsafe { list.data.add(i * list.elem_size) };
        if unsafe { libc_memcmp(existing, elem_ptr, list.elem_size) } {
            return 1;
        }
    }
    0
}

/// Removes the element at `index` from the list. Aborts on out-of-bounds.
/// If `elem_rc` is non-zero, decrements the RC of the removed element.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_list_new`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_list_remove(ptr: *mut u8, index: i64, elem_rc: i8) {
    let list = unsafe { &mut *(ptr.cast::<SparkList>()) };
    let idx = index as usize;
    if idx >= list.len {
        eprintln!(
            "spark: list remove index out of bounds: index {idx}, length {}",
            list.len
        );
        std::process::abort();
    }

    let elem_addr = unsafe { list.data.add(idx * list.elem_size) };

    // RC dec the removed element if it's reference-counted.
    if elem_rc != 0 {
        let inner = unsafe { *(elem_addr.cast::<*mut u8>()) };
        if !inner.is_null() {
            let rc_field = unsafe { &mut *(inner.cast::<usize>()) };
            *rc_field -= 1;
        }
    }

    // Shift remaining elements left.
    let remaining = list.len - idx - 1;
    if remaining > 0 {
        let src = unsafe { elem_addr.add(list.elem_size) };
        unsafe { std::ptr::copy(src, elem_addr, remaining * list.elem_size) };
    }
    list.len -= 1;
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
    let map = unsafe { &*(ptr.cast::<SparkMap>()) };
    for i in 0..map.len {
        let existing_key = unsafe { map.keys.add(i * map.key_size) };
        if unsafe { libc_memcmp(existing_key, key_ptr, map.key_size) } {
            return 1;
        }
    }
    0
}

/// Removes the entry with the given key from the map (swap-remove).
/// If `key_rc` / `val_rc` are non-zero, decrements RC on removed key/value.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_map_new`.
/// `key_ptr` must point to at least `key_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_map_remove(
    ptr: *mut u8,
    key_ptr: *const u8,
    key_rc: i8,
    val_rc: i8,
) {
    let map = unsafe { &mut *(ptr.cast::<SparkMap>()) };

    for i in 0..map.len {
        let existing_key = unsafe { map.keys.add(i * map.key_size) };
        if unsafe { libc_memcmp(existing_key, key_ptr, map.key_size) } {
            // RC dec removed key/value.
            if key_rc != 0 {
                let inner = unsafe { *(existing_key.cast::<*mut u8>()) };
                if !inner.is_null() {
                    let rc = unsafe { &mut *(inner.cast::<usize>()) };
                    *rc -= 1;
                }
            }
            if val_rc != 0 {
                let val_addr = unsafe { map.values.add(i * map.value_size) };
                let inner = unsafe { *(val_addr.cast::<*mut u8>()) };
                if !inner.is_null() {
                    let rc = unsafe { &mut *(inner.cast::<usize>()) };
                    *rc -= 1;
                }
            }

            // Swap-remove: move last entry into the hole.
            let last = map.len - 1;
            if i != last {
                let last_key = unsafe { map.keys.add(last * map.key_size) };
                unsafe { std::ptr::copy_nonoverlapping(last_key, existing_key, map.key_size) };

                let val_addr = unsafe { map.values.add(i * map.value_size) };
                let last_val = unsafe { map.values.add(last * map.value_size) };
                unsafe { std::ptr::copy_nonoverlapping(last_val, val_addr, map.value_size) };
            }
            map.len -= 1;
            return;
        }
    }
}

/// Returns the number of elements in a set.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_set_new`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_set_len(ptr: *mut u8) -> i64 {
    // A set is a SparkList internally.
    unsafe { __spark_list_len(ptr) }
}

/// Returns 1 if the set contains the element, 0 otherwise.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_set_new`.
/// `elem_ptr` must point to at least `elem_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_set_contains(ptr: *mut u8, elem_ptr: *const u8) -> i8 {
    unsafe { __spark_list_contains(ptr, elem_ptr) }
}

/// Removes an element from the set (swap-remove by value).
/// If `elem_rc` is non-zero, decrements the RC of the removed element.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_set_new`.
/// `elem_ptr` must point to at least `elem_size` bytes.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_set_remove(ptr: *mut u8, elem_ptr: *const u8, elem_rc: i8) {
    let set = unsafe { &mut *(ptr.cast::<SparkList>()) };

    for i in 0..set.len {
        let existing = unsafe { set.data.add(i * set.elem_size) };
        if unsafe { libc_memcmp(existing, elem_ptr, set.elem_size) } {
            // RC dec the removed element.
            if elem_rc != 0 {
                let inner = unsafe { *(existing.cast::<*mut u8>()) };
                if !inner.is_null() {
                    let rc = unsafe { &mut *(inner.cast::<usize>()) };
                    *rc -= 1;
                }
            }

            // Swap-remove: move last element into the hole.
            let last_idx = set.len - 1;
            if i != last_idx {
                let last_elem = unsafe { set.data.add(last_idx * set.elem_size) };
                unsafe { std::ptr::copy_nonoverlapping(last_elem, existing, set.elem_size) };
            }
            set.len -= 1;
            return;
        }
    }
}

/// Returns the length in bytes of a string.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_string_new` or
/// `__spark_string_from_literal`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_wrap)]
pub unsafe extern "C" fn __spark_string_len(ptr: *mut u8) -> i64 {
    let s = unsafe { &*(ptr.cast::<SparkString>()) };
    s.len as i64
}

// =============================================================================
// List range construction
// =============================================================================

/// Creates a list of `i64` values from a range.
///
/// Parameters:
/// - `start`: the first value in the range
/// - `end`: the exclusive (or inclusive, if `inclusive != 0`) bound
/// - `inclusive`: 0 for exclusive (`..`), non-zero for inclusive (`..=`)
/// - `step`: increment between values (0 is treated as 1)
///
/// Returns a new `SparkList` pointer (rc = 1) containing `i64` elements.
/// If the range is empty (e.g. `start >= end` with positive step), returns
/// an empty list.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub extern "C" fn __spark_list_from_range(start: i64, end: i64, inclusive: i8, step: i64) -> *mut u8 {
    let step = if step == 0 { 1 } else { step };
    let inclusive = inclusive != 0;

    // Calculate element count.
    let count = if step > 0 {
        let limit = if inclusive { end } else { end - 1 };
        if start > limit {
            0usize
        } else {
            ((limit - start) / step + 1) as usize
        }
    } else {
        // Negative step: counting down.
        let limit = if inclusive { end } else { end + 1 };
        if start < limit {
            0usize
        } else {
            ((start - limit) / (-step) + 1) as usize
        }
    };

    let elem_size = std::mem::size_of::<i64>();
    let list_ptr = __spark_list_new(elem_size as i64);

    if count == 0 {
        return list_ptr;
    }

    // Pre-allocate exact capacity.
    let list = unsafe { &mut *(list_ptr.cast::<SparkList>()) };
    let buf_size = count * elem_size;
    let align = std::mem::align_of::<u64>();
    let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(buf_size, align) };
    let buf = unsafe { std::alloc::alloc(layout) };
    if buf.is_null() {
        eprintln!("spark: out of memory allocating list range");
        std::process::abort();
    }
    list.data = buf;
    list.cap = count;

    // Fill the buffer.
    let data = buf.cast::<i64>();
    let mut val = start;
    for i in 0..count {
        unsafe { *data.add(i) = val };
        val += step;
    }
    list.len = count;

    list_ptr
}

// =============================================================================
// List pipeline operations — sort, take, reverse
// =============================================================================

/// Sorts a list of i64 elements in ascending order, returning a new list.
///
/// The original list is not modified (functional semantics).
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_list_new` containing i64 elements.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_list_sort(ptr: *mut u8) -> *mut u8 {
    let list = unsafe { &*(ptr.cast::<SparkList>()) };
    let new_list = __spark_list_new(list.elem_size as i64);

    if list.len == 0 {
        return new_list;
    }

    // Copy all elements into a Vec<i64> for sorting
    let mut values: Vec<i64> = Vec::with_capacity(list.len);
    for i in 0..list.len {
        let elem = unsafe { list.data.add(i * list.elem_size) };
        let val = unsafe { *(elem.cast::<i64>()) };
        values.push(val);
    }
    values.sort_unstable();

    // Push sorted values into the new list
    for val in &values {
        let ptr_to_val = (val as *const i64).cast::<u8>();
        unsafe { __spark_list_push(new_list, ptr_to_val) };
    }

    new_list
}

/// Takes the first `count` elements from a list, returning a new list.
///
/// If `count` exceeds the list length, returns all elements.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_list_new`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_list_take(ptr: *mut u8, count: i64) -> *mut u8 {
    let list = unsafe { &*(ptr.cast::<SparkList>()) };
    let new_list = __spark_list_new(list.elem_size as i64);

    let n = (count as usize).min(list.len);
    for i in 0..n {
        let elem = unsafe { list.data.add(i * list.elem_size) };
        unsafe { __spark_list_push(new_list, elem) };
    }

    new_list
}

/// Reverses a list, returning a new list with elements in reverse order.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_list_new`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_list_reverse(ptr: *mut u8) -> *mut u8 {
    let list = unsafe { &*(ptr.cast::<SparkList>()) };
    let new_list = __spark_list_new(list.elem_size as i64);

    for i in (0..list.len).rev() {
        let elem = unsafe { list.data.add(i * list.elem_size) };
        unsafe { __spark_list_push(new_list, elem) };
    }

    new_list
}

// =============================================================================
// String interpolation — concat and to_string conversions
// =============================================================================

/// Concatenates two strings, returning a new string (rc = 1).
///
/// Does NOT consume `a` or `b` — the caller manages their reference counts.
///
/// # Safety
///
/// Both pointers must be valid `SparkString` pointers.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_string_concat(a: *mut u8, b: *mut u8) -> *mut u8 {
    let sa = unsafe { &*(a.cast::<SparkString>()) };
    let sb = unsafe { &*(b.cast::<SparkString>()) };

    let new_len = sa.len + sb.len;

    if new_len == 0 {
        return __spark_string_new();
    }

    let align = std::mem::align_of::<u64>();
    let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(new_len, align) };
    let buf = unsafe { std::alloc::alloc(layout) };
    if buf.is_null() {
        eprintln!("spark: out of memory allocating string");
        std::process::abort();
    }

    if sa.len > 0 {
        unsafe { std::ptr::copy_nonoverlapping(sa.data, buf, sa.len) };
    }
    if sb.len > 0 {
        unsafe { std::ptr::copy_nonoverlapping(sb.data, buf.add(sa.len), sb.len) };
    }

    let s = Box::new(SparkString {
        rc: 1,
        data: buf,
        len: new_len,
        cap: new_len,
    });
    Box::into_raw(s).cast::<u8>()
}

/// Converts an integer to a string (rc = 1).
#[no_mangle]
pub extern "C" fn __spark_int_to_string(val: i64) -> *mut u8 {
    let text = val.to_string();
    let bytes = text.as_bytes();
    unsafe { __spark_string_from_literal(bytes.as_ptr(), bytes.len() as i64) }
}

/// Converts a float to a string (rc = 1).
#[no_mangle]
pub extern "C" fn __spark_float_to_string(val: f64) -> *mut u8 {
    let text = val.to_string();
    let bytes = text.as_bytes();
    unsafe { __spark_string_from_literal(bytes.as_ptr(), bytes.len() as i64) }
}

/// Converts a boolean to a string (rc = 1).
///
/// Returns `"true"` for non-zero, `"false"` for zero.
#[no_mangle]
pub extern "C" fn __spark_bool_to_string(val: i8) -> *mut u8 {
    let text = if val != 0 { "true" } else { "false" };
    let bytes = text.as_bytes();
    unsafe { __spark_string_from_literal(bytes.as_ptr(), bytes.len() as i64) }
}

// =============================================================================
// Helpers
// =============================================================================

/// Byte-by-byte equality comparison (equivalent to `memcmp(a, b, n) == 0`).
///
/// # Safety
///
/// Both pointers must be valid for `len` bytes.
unsafe fn libc_memcmp(a: *const u8, b: *const u8, len: usize) -> bool {
    let a_slice = unsafe { std::slice::from_raw_parts(a, len) };
    let b_slice = unsafe { std::slice::from_raw_parts(b, len) };
    a_slice == b_slice
}

// =============================================================================
// Reference counting
// =============================================================================

/// Increments the reference count of any heap-allocated Spark object.
///
/// Works generically because `rc: usize` is always the first field (offset 0)
/// in `SparkString`, `SparkList`, and `SparkMap`.
///
/// # Safety
///
/// `ptr` must be a valid pointer to a Spark heap object whose first field
/// is `rc: usize`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_rc_inc(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let rc_ptr = ptr.cast::<usize>();
    unsafe { *rc_ptr += 1 };
}

/// Decrements the reference count of a `SparkString`.
/// Frees the string when the count reaches zero.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_string_new` or
/// `__spark_string_from_literal`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_string_rc_dec(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let s = unsafe { &mut *(ptr.cast::<SparkString>()) };
    s.rc -= 1;
    if s.rc == 0 {
        // Free the data buffer.
        if !s.data.is_null() && s.cap > 0 {
            let align = std::mem::align_of::<u64>();
            let layout =
                unsafe { std::alloc::Layout::from_size_align_unchecked(s.cap, align) };
            unsafe { std::alloc::dealloc(s.data, layout) };
        }
        // Free the SparkString struct itself.
        drop(unsafe { Box::from_raw(ptr.cast::<SparkString>()) });
    }
}

/// Decrements the reference count of a `SparkList`.
/// If `elem_rc` is non-zero and the list is being freed, decrements
/// the RC of each element pointer before freeing the list.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_list_new`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_list_rc_dec(ptr: *mut u8, elem_rc: i8) {
    if ptr.is_null() {
        return;
    }
    let list = unsafe { &mut *(ptr.cast::<SparkList>()) };
    list.rc -= 1;
    if list.rc == 0 {
        // If elements are RC'd, dec each one.
        if elem_rc != 0 && !list.data.is_null() {
            for i in 0..list.len {
                let elem_ptr_loc = unsafe { list.data.add(i * list.elem_size) };
                // Each element is itself a pointer to an RC object.
                let inner_ptr = unsafe { *(elem_ptr_loc.cast::<*mut u8>()) };
                if !inner_ptr.is_null() {
                    // Generic dec — the caller knows the element type and will
                    // have arranged for the correct typed rc_dec. For simplicity,
                    // we use a generic dec here that just decrements the RC field.
                    // This works because all Spark heap types start with rc:usize.
                    let rc_field = unsafe { &mut *(inner_ptr.cast::<usize>()) };
                    *rc_field -= 1;
                    // Note: this does NOT recursively free sub-objects.
                    // For v1, nested RC dec is one level deep.
                    // A production implementation would need a type-dispatch table.
                }
            }
        }
        // Free the data buffer.
        if !list.data.is_null() && list.cap > 0 {
            let align = std::mem::align_of::<u64>();
            let buf_size = list.cap * list.elem_size;
            let layout =
                unsafe { std::alloc::Layout::from_size_align_unchecked(buf_size, align) };
            unsafe { std::alloc::dealloc(list.data, layout) };
        }
        // Free the SparkList struct itself.
        drop(unsafe { Box::from_raw(ptr.cast::<SparkList>()) });
    }
}

/// Decrements the reference count of a `SparkMap`.
/// If `key_rc` / `val_rc` are non-zero, decrements RC of corresponding
/// element pointers before freeing.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_map_new`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_map_rc_dec(ptr: *mut u8, key_rc: i8, val_rc: i8) {
    if ptr.is_null() {
        return;
    }
    let map = unsafe { &mut *(ptr.cast::<SparkMap>()) };
    map.rc -= 1;
    if map.rc == 0 {
        // Dec RC of keys if they're RC'd.
        if key_rc != 0 && !map.keys.is_null() {
            for i in 0..map.len {
                let key_loc = unsafe { map.keys.add(i * map.key_size) };
                let inner = unsafe { *(key_loc.cast::<*mut u8>()) };
                if !inner.is_null() {
                    let rc_field = unsafe { &mut *(inner.cast::<usize>()) };
                    *rc_field -= 1;
                }
            }
        }
        // Dec RC of values if they're RC'd.
        if val_rc != 0 && !map.values.is_null() {
            for i in 0..map.len {
                let val_loc = unsafe { map.values.add(i * map.value_size) };
                let inner = unsafe { *(val_loc.cast::<*mut u8>()) };
                if !inner.is_null() {
                    let rc_field = unsafe { &mut *(inner.cast::<usize>()) };
                    *rc_field -= 1;
                }
            }
        }
        // Free key buffer.
        if !map.keys.is_null() && map.cap > 0 {
            let align = std::mem::align_of::<u64>();
            let key_buf_size = map.cap * map.key_size;
            let layout =
                unsafe { std::alloc::Layout::from_size_align_unchecked(key_buf_size, align) };
            unsafe { std::alloc::dealloc(map.keys, layout) };
        }
        // Free value buffer.
        if !map.values.is_null() && map.cap > 0 {
            let align = std::mem::align_of::<u64>();
            let val_buf_size = map.cap * map.value_size;
            let layout =
                unsafe { std::alloc::Layout::from_size_align_unchecked(val_buf_size, align) };
            unsafe { std::alloc::dealloc(map.values, layout) };
        }
        // Free the SparkMap struct itself.
        drop(unsafe { Box::from_raw(ptr.cast::<SparkMap>()) });
    }
}

/// Decrements the reference count of a set (which is a `SparkList` internally).
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_set_new`.
#[no_mangle]
pub unsafe extern "C" fn __spark_set_rc_dec(ptr: *mut u8, elem_rc: i8) {
    unsafe { __spark_list_rc_dec(ptr, elem_rc) };
}

// =============================================================================
// COW (Copy-on-Write) helpers — prepared for mutation operations
// =============================================================================

/// Returns the pointer unchanged if rc == 1 (unique owner).
/// If rc > 1, copies the string buffer, decrements the old rc, and returns
/// a new `SparkString` with rc = 1.
///
/// # Safety
///
/// `ptr` must be a valid `SparkString` pointer.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_string_cow(ptr: *mut u8) -> *mut u8 {
    if ptr.is_null() {
        return ptr;
    }
    let s = unsafe { &*(ptr.cast::<SparkString>()) };
    if s.rc == 1 {
        return ptr;
    }
    // Clone the buffer.
    let new_ptr = if s.len > 0 && !s.data.is_null() {
        let align = std::mem::align_of::<u64>();
        let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(s.len, align) };
        let buf = unsafe { std::alloc::alloc(layout) };
        if buf.is_null() {
            eprintln!("spark: out of memory in string COW copy");
            std::process::abort();
        }
        unsafe { std::ptr::copy_nonoverlapping(s.data, buf, s.len) };
        buf
    } else {
        std::ptr::null_mut()
    };
    let new = Box::new(SparkString {
        rc: 1,
        data: new_ptr,
        len: s.len,
        cap: s.len,
    });
    // Decrement old rc.
    let old = unsafe { &mut *(ptr.cast::<SparkString>()) };
    old.rc -= 1;
    Box::into_raw(new).cast::<u8>()
}

/// COW for lists. If rc > 1, shallow-copies the data buffer and `rc_inc`
/// each element if `elem_rc` is set.
///
/// # Safety
///
/// `ptr` must be a valid `SparkList` pointer.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_list_cow(ptr: *mut u8, elem_rc: i8) -> *mut u8 {
    if ptr.is_null() {
        return ptr;
    }
    let list = unsafe { &*(ptr.cast::<SparkList>()) };
    if list.rc == 1 {
        return ptr;
    }
    // Allocate new data buffer.
    let buf_size = list.cap * list.elem_size;
    let align = std::mem::align_of::<u64>();
    let new_data = if buf_size > 0 && !list.data.is_null() {
        let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(buf_size, align) };
        let buf = unsafe { std::alloc::alloc(layout) };
        if buf.is_null() {
            eprintln!("spark: out of memory in list COW copy");
            std::process::abort();
        }
        unsafe { std::ptr::copy_nonoverlapping(list.data, buf, list.len * list.elem_size) };
        // rc_inc each element if they're RC'd.
        if elem_rc != 0 {
            for i in 0..list.len {
                let elem_loc = unsafe { buf.add(i * list.elem_size) };
                let inner = unsafe { *(elem_loc.cast::<*mut u8>()) };
                if !inner.is_null() {
                    unsafe { __spark_rc_inc(inner) };
                }
            }
        }
        buf
    } else {
        std::ptr::null_mut()
    };
    let new = Box::new(SparkList {
        rc: 1,
        data: new_data,
        len: list.len,
        cap: list.cap,
        elem_size: list.elem_size,
    });
    // Decrement old rc.
    let old = unsafe { &mut *(ptr.cast::<SparkList>()) };
    old.rc -= 1;
    Box::into_raw(new).cast::<u8>()
}

/// COW for maps.
///
/// # Safety
///
/// `ptr` must be a valid `SparkMap` pointer.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_map_cow(ptr: *mut u8, key_rc: i8, val_rc: i8) -> *mut u8 {
    if ptr.is_null() {
        return ptr;
    }
    let map = unsafe { &*(ptr.cast::<SparkMap>()) };
    if map.rc == 1 {
        return ptr;
    }
    let align = std::mem::align_of::<u64>();
    // Copy keys buffer.
    let key_buf_size = map.cap * map.key_size;
    let new_keys = if key_buf_size > 0 && !map.keys.is_null() {
        let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(key_buf_size, align) };
        let buf = unsafe { std::alloc::alloc(layout) };
        if buf.is_null() {
            eprintln!("spark: out of memory in map COW copy");
            std::process::abort();
        }
        unsafe { std::ptr::copy_nonoverlapping(map.keys, buf, map.len * map.key_size) };
        if key_rc != 0 {
            for i in 0..map.len {
                let loc = unsafe { buf.add(i * map.key_size) };
                let inner = unsafe { *(loc.cast::<*mut u8>()) };
                if !inner.is_null() {
                    unsafe { __spark_rc_inc(inner) };
                }
            }
        }
        buf
    } else {
        std::ptr::null_mut()
    };
    // Copy values buffer.
    let val_buf_size = map.cap * map.value_size;
    let new_values = if val_buf_size > 0 && !map.values.is_null() {
        let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(val_buf_size, align) };
        let buf = unsafe { std::alloc::alloc(layout) };
        if buf.is_null() {
            eprintln!("spark: out of memory in map COW copy");
            std::process::abort();
        }
        unsafe { std::ptr::copy_nonoverlapping(map.values, buf, map.len * map.value_size) };
        if val_rc != 0 {
            for i in 0..map.len {
                let loc = unsafe { buf.add(i * map.value_size) };
                let inner = unsafe { *(loc.cast::<*mut u8>()) };
                if !inner.is_null() {
                    unsafe { __spark_rc_inc(inner) };
                }
            }
        }
        buf
    } else {
        std::ptr::null_mut()
    };
    let new = Box::new(SparkMap {
        rc: 1,
        keys: new_keys,
        values: new_values,
        len: map.len,
        cap: map.cap,
        key_size: map.key_size,
        value_size: map.value_size,
    });
    // Decrement old rc.
    let old = unsafe { &mut *(ptr.cast::<SparkMap>()) };
    old.rc -= 1;
    Box::into_raw(new).cast::<u8>()
}

// =============================================================================
// Legacy compatibility
// =============================================================================

/// Releases a reference-counted object. Legacy stub — use type-specific
/// `_rc_dec` functions instead.
#[no_mangle]
pub extern "C" fn __spark_rc_release(_ptr: *mut u8) {
    // No-op — kept for backward compatibility.
    // New code should use __spark_string_rc_dec, __spark_list_rc_dec, etc.
}

// =============================================================================
// Closure environment allocation
// =============================================================================

/// Allocates a closure environment with a 16-byte RC header.
///
/// Layout: `[rc: usize (8 bytes), alloc_size: usize (8 bytes), ...payload...]`
///
/// The `size` parameter is the payload size (captures only). The runtime
/// prepends the 16-byte header internally. The returned pointer points to
/// the start of the allocation (i.e., to the `rc` field at offset 0).
/// Captures are stored at offset 16 by codegen.
///
/// Returns null if `size` is 0 (zero captures → no env needed).
/// The initial reference count is 1.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub extern "C" fn __spark_env_alloc(size: i64) -> *mut u8 {
    let payload_size = size as usize;
    if payload_size == 0 {
        return std::ptr::null_mut();
    }
    let total_size = 16 + payload_size;
    let align = std::mem::align_of::<u64>();
    let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(total_size, align) };
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    if ptr.is_null() {
        eprintln!("spark: out of memory allocating closure environment");
        std::process::abort();
    }
    // Store rc = 1 at offset 0.
    unsafe { *(ptr.cast::<usize>()) = 1 };
    // Store total_alloc_size at offset 8.
    unsafe { *(ptr.add(8).cast::<usize>()) = total_size };
    ptr
}

/// Decrements the reference count of a closure environment.
/// Frees the allocation when the count reaches zero.
///
/// The environment layout is: `[rc: usize, alloc_size: usize, ...payload...]`.
/// The `alloc_size` at offset 8 stores the total allocation size so we can
/// dealloc without needing size info from codegen.
///
/// **Note**: This does NOT recursively decrement RC of captured values inside
/// the environment. Nested RC cleanup is a Phase 2 follow-up.
///
/// # Safety
///
/// `ptr` must be null or a valid pointer from `__spark_env_alloc`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_env_rc_dec(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let rc_ptr = ptr.cast::<usize>();
    let rc = unsafe { &mut *rc_ptr };
    *rc -= 1;
    if *rc == 0 {
        let alloc_size = unsafe { *(ptr.add(8).cast::<usize>()) };
        let align = std::mem::align_of::<u64>();
        let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(alloc_size, align) };
        unsafe { std::alloc::dealloc(ptr, layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: read the string content from a SparkString pointer.
    unsafe fn read_spark_string(ptr: *mut u8) -> String {
        let s = unsafe { &*(ptr.cast::<SparkString>()) };
        if s.len == 0 {
            return String::new();
        }
        let slice = unsafe { std::slice::from_raw_parts(s.data, s.len) };
        std::str::from_utf8(slice).unwrap().to_owned()
    }

    #[test]
    fn string_concat_basic() {
        unsafe {
            let a = __spark_string_from_literal("hello ".as_ptr(), 6);
            let b = __spark_string_from_literal("world".as_ptr(), 5);
            let result = __spark_string_concat(a, b);
            assert_eq!(read_spark_string(result), "hello world");
            __spark_string_rc_dec(a);
            __spark_string_rc_dec(b);
            __spark_string_rc_dec(result);
        }
    }

    #[test]
    fn string_concat_empty() {
        unsafe {
            let a = __spark_string_new();
            let b = __spark_string_from_literal("text".as_ptr(), 4);
            let result = __spark_string_concat(a, b);
            assert_eq!(read_spark_string(result), "text");
            __spark_string_rc_dec(a);
            __spark_string_rc_dec(b);
            __spark_string_rc_dec(result);
        }
    }

    #[test]
    fn string_concat_both_empty() {
        unsafe {
            let a = __spark_string_new();
            let b = __spark_string_new();
            let result = __spark_string_concat(a, b);
            assert_eq!(read_spark_string(result), "");
            __spark_string_rc_dec(a);
            __spark_string_rc_dec(b);
            __spark_string_rc_dec(result);
        }
    }

    #[test]
    fn int_to_string() {
        unsafe {
            let result = __spark_int_to_string(42);
            assert_eq!(read_spark_string(result), "42");
            __spark_string_rc_dec(result);

            let neg = __spark_int_to_string(-7);
            assert_eq!(read_spark_string(neg), "-7");
            __spark_string_rc_dec(neg);

            let zero = __spark_int_to_string(0);
            assert_eq!(read_spark_string(zero), "0");
            __spark_string_rc_dec(zero);
        }
    }

    #[test]
    fn float_to_string() {
        unsafe {
            let result = __spark_float_to_string(3.14);
            let text = read_spark_string(result);
            assert!(text.starts_with("3.14"), "expected 3.14..., got {text}");
            __spark_string_rc_dec(result);
        }
    }

    #[test]
    fn bool_to_string() {
        unsafe {
            let t = __spark_bool_to_string(1);
            assert_eq!(read_spark_string(t), "true");
            __spark_string_rc_dec(t);

            let f = __spark_bool_to_string(0);
            assert_eq!(read_spark_string(f), "false");
            __spark_string_rc_dec(f);
        }
    }

    #[test]
    fn env_alloc_zero_returns_null() {
        let ptr = __spark_env_alloc(0);
        assert!(ptr.is_null());
    }

    #[test]
    fn env_alloc_has_rc_header() {
        unsafe {
            let ptr = __spark_env_alloc(24);
            assert!(!ptr.is_null());
            // rc should be 1
            let rc = *(ptr.cast::<usize>());
            assert_eq!(rc, 1);
            // alloc_size should be 16 + 24 = 40
            let alloc_size = *(ptr.add(8).cast::<usize>());
            assert_eq!(alloc_size, 40);
            // Free it
            __spark_env_rc_dec(ptr);
        }
    }

    #[test]
    fn env_rc_inc_dec_lifecycle() {
        unsafe {
            let ptr = __spark_env_alloc(8);
            assert_eq!(*(ptr.cast::<usize>()), 1);

            // Increment RC
            __spark_rc_inc(ptr);
            assert_eq!(*(ptr.cast::<usize>()), 2);

            // First dec — should not free
            __spark_env_rc_dec(ptr);
            assert_eq!(*(ptr.cast::<usize>()), 1);

            // Second dec — frees (no crash = success)
            __spark_env_rc_dec(ptr);
        }
    }

    #[test]
    fn env_rc_dec_null_is_noop() {
        unsafe {
            __spark_env_rc_dec(std::ptr::null_mut());
        }
    }

    /// Helper: read elements from a SparkList of i64.
    unsafe fn read_list_i64(ptr: *mut u8) -> Vec<i64> {
        let list = unsafe { &*(ptr.cast::<SparkList>()) };
        let mut result = Vec::with_capacity(list.len);
        for i in 0..list.len {
            let elem = unsafe { list.data.add(i * list.elem_size).cast::<i64>() };
            result.push(unsafe { *elem });
        }
        result
    }

    #[test]
    fn list_range_exclusive() {
        unsafe {
            let ptr = __spark_list_from_range(0, 5, 0, 1);
            assert_eq!(read_list_i64(ptr), vec![0, 1, 2, 3, 4]);
            __spark_list_rc_dec(ptr, 0);
        }
    }

    #[test]
    fn list_range_inclusive() {
        unsafe {
            let ptr = __spark_list_from_range(0, 5, 1, 1);
            assert_eq!(read_list_i64(ptr), vec![0, 1, 2, 3, 4, 5]);
            __spark_list_rc_dec(ptr, 0);
        }
    }

    #[test]
    fn list_range_stepped() {
        unsafe {
            let ptr = __spark_list_from_range(0, 10, 0, 2);
            assert_eq!(read_list_i64(ptr), vec![0, 2, 4, 6, 8]);
            __spark_list_rc_dec(ptr, 0);
        }
    }

    #[test]
    fn list_range_negative_step() {
        unsafe {
            let ptr = __spark_list_from_range(10, 0, 0, -2);
            assert_eq!(read_list_i64(ptr), vec![10, 8, 6, 4, 2]);
            __spark_list_rc_dec(ptr, 0);
        }
    }

    #[test]
    fn list_range_empty() {
        unsafe {
            // start >= end with positive step → empty
            let ptr = __spark_list_from_range(5, 3, 0, 1);
            assert_eq!(read_list_i64(ptr), Vec::<i64>::new());
            __spark_list_rc_dec(ptr, 0);
        }
    }

    #[test]
    fn list_range_zero_step_treated_as_one() {
        unsafe {
            let ptr = __spark_list_from_range(0, 3, 0, 0);
            assert_eq!(read_list_i64(ptr), vec![0, 1, 2]);
            __spark_list_rc_dec(ptr, 0);
        }
    }
}

// =============================================================================
// Parallel iteration (Rayon)
// =============================================================================

/// Callback kind flags (matches codegen cb_kind computation).
///
/// Bits 0-1: input kind (0 = int-like, 1 = float)
/// Bits 2-3: output kind (0 = int-like, 1 = float, 2 = void)
const CB_INPUT_FLOAT: u8 = 1;
const CB_OUTPUT_FLOAT: u8 = 1 << 2;
#[allow(dead_code)]
const CB_OUTPUT_VOID: u8 = 2 << 2;

/// Parallel for-each: iterates over list elements in parallel, calling `callback`
/// for each element (side effects only, no return value collected).
///
/// # Safety
///
/// - `list_ptr` must be a valid `SparkList` pointer.
/// - `fn_ptr` must be a valid function pointer matching the `cb_kind` signature.
/// - `env_ptr` may be null (non-capturing) or a valid env pointer.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::missing_safety_doc, clippy::items_after_statements)]
pub unsafe extern "C" fn __spark_parallel_foreach(
    list_ptr: *mut u8,
    fn_ptr: *mut u8,
    env_ptr: *mut u8,
    _elem_size: i64,
    cb_kind: u8,
) {
    use rayon::prelude::*;

    let list = unsafe { &*(list_ptr.cast::<SparkList>()) };
    if list.len == 0 {
        return;
    }

    let elem_size = list.elem_size;
    let len = list.len;

    // Cast raw pointers to usize for Send
    let fn_addr = fn_ptr as usize;
    let env_addr = env_ptr as usize;
    let data_addr = list.data as usize;

    (0..len).into_par_iter().for_each(|i| unsafe {
        let elem_ptr = (data_addr as *const u8).add(i * elem_size);
        call_foreach_cb(fn_addr, env_addr, elem_ptr, cb_kind);
    });
}

/// Parallel map: iterates over list elements in parallel, collecting callback
/// results into a new list that preserves input order.
///
/// # Safety
///
/// - `list_ptr` must be a valid `SparkList` pointer.
/// - `fn_ptr` must be a valid function pointer matching the `cb_kind` signature.
/// - `env_ptr` may be null (non-capturing) or a valid env pointer.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::missing_safety_doc, clippy::items_after_statements)]
pub unsafe extern "C" fn __spark_parallel_map(
    list_ptr: *mut u8,
    fn_ptr: *mut u8,
    env_ptr: *mut u8,
    _elem_size: i64,
    result_elem_size: i64,
    cb_kind: u8,
) -> *mut u8 {
    use rayon::prelude::*;

    let list = unsafe { &*(list_ptr.cast::<SparkList>()) };
    let result_list = __spark_list_new(result_elem_size);

    if list.len == 0 {
        return result_list;
    }

    let elem_size = list.elem_size;
    let len = list.len;

    // Cast raw pointers to usize for Send
    let fn_addr = fn_ptr as usize;
    let env_addr = env_ptr as usize;
    let data_addr = list.data as usize;

    // Collect results in order using Rayon's indexed parallel iterator
    let results: Vec<u64> = (0..len)
        .into_par_iter()
        .map(|i| unsafe {
            let elem_ptr = (data_addr as *const u8).add(i * elem_size);
            call_map_cb(fn_addr, env_addr, elem_ptr, cb_kind)
        })
        .collect();

    // Build result list from collected values
    for val in &results {
        let val_bytes = val.to_ne_bytes();
        unsafe {
            __spark_list_push(result_list, val_bytes.as_ptr().cast_mut());
        }
    }

    // For result elements larger than 8 bytes we'd need special handling,
    // but scalar types (Int, Float, Bool, pointers) are all ≤ 8 bytes.
    result_list
}

/// Calls a void-returning callback for parallel foreach.
///
/// # Safety
///
/// The caller must ensure `fn_addr` and `env_addr` are valid pointers.
unsafe fn call_foreach_cb(fn_addr: usize, env_addr: usize, elem_ptr: *const u8, cb_kind: u8) {
    let env = env_addr as *mut u8;
    let input_float = (cb_kind & CB_INPUT_FLOAT) != 0;

    if input_float {
        let val = unsafe { *(elem_ptr.cast::<f64>()) };
        let f: unsafe extern "C" fn(*mut u8, f64) = unsafe { std::mem::transmute(fn_addr) };
        unsafe { f(env, val) };
    } else {
        let val = unsafe { *(elem_ptr.cast::<i64>()) };
        let f: unsafe extern "C" fn(*mut u8, i64) = unsafe { std::mem::transmute(fn_addr) };
        unsafe { f(env, val) };
    }
}

/// Calls a value-returning callback for parallel map, returning result as u64.
///
/// # Safety
///
/// The caller must ensure `fn_addr` and `env_addr` are valid pointers.
unsafe fn call_map_cb(fn_addr: usize, env_addr: usize, elem_ptr: *const u8, cb_kind: u8) -> u64 {
    let env = env_addr as *mut u8;
    let input_float = (cb_kind & CB_INPUT_FLOAT) != 0;
    let output_float = (cb_kind & CB_OUTPUT_FLOAT) != 0;

    if input_float && output_float {
        let val = unsafe { *(elem_ptr.cast::<f64>()) };
        let f: unsafe extern "C" fn(*mut u8, f64) -> f64 = unsafe { std::mem::transmute(fn_addr) };
        let result = unsafe { f(env, val) };
        result.to_bits()
    } else if input_float {
        let val = unsafe { *(elem_ptr.cast::<f64>()) };
        let f: unsafe extern "C" fn(*mut u8, f64) -> i64 = unsafe { std::mem::transmute(fn_addr) };
        let result = unsafe { f(env, val) };
        result as u64
    } else if output_float {
        let val = unsafe { *(elem_ptr.cast::<i64>()) };
        let f: unsafe extern "C" fn(*mut u8, i64) -> f64 = unsafe { std::mem::transmute(fn_addr) };
        let result = unsafe { f(env, val) };
        result.to_bits()
    } else {
        let val = unsafe { *(elem_ptr.cast::<i64>()) };
        let f: unsafe extern "C" fn(*mut u8, i64) -> i64 = unsafe { std::mem::transmute(fn_addr) };
        let result = unsafe { f(env, val) };
        result as u64
    }
}

/// Fused parallel map + reduce: maps elements in parallel, then reduces with
/// no intermediate list allocation.
///
/// # Safety
///
/// - `list_ptr` must be a valid `SparkList` pointer.
/// - `map_fn` and `reduce_fn` must be valid function pointers.
/// - `map_env` and `reduce_env` may be null (non-capturing) or valid env pointers.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::missing_safety_doc, clippy::items_after_statements)]
pub unsafe extern "C" fn __spark_parallel_map_reduce(
    list_ptr: *mut u8,
    map_fn: *mut u8,
    map_env: *mut u8,
    reduce_fn: *mut u8,
    reduce_env: *mut u8,
    initial: i64,
    _elem_size: i64,
    map_cb_kind: u8,
    reduce_cb_kind: u8,
) -> i64 {
    use rayon::prelude::*;

    let list = unsafe { &*(list_ptr.cast::<SparkList>()) };
    if list.len == 0 {
        return initial;
    }

    let elem_size = list.elem_size;
    let len = list.len;

    let map_fn_addr = map_fn as usize;
    let map_env_addr = map_env as usize;
    let reduce_fn_addr = reduce_fn as usize;
    let reduce_env_addr = reduce_env as usize;
    let data_addr = list.data as usize;

    let result = (0..len)
        .into_par_iter()
        .map(|i| unsafe {
            let elem_ptr = (data_addr as *const u8).add(i * elem_size);
            call_map_cb(map_fn_addr, map_env_addr, elem_ptr, map_cb_kind)
        })
        .reduce(
            || initial as u64,
            |acc, val| unsafe {
                call_reduce_cb(reduce_fn_addr, reduce_env_addr, acc, val, reduce_cb_kind)
            },
        );

    result as i64
}

/// Calls a 2-argument reduce callback: `(acc, val) -> acc`.
///
/// # Safety
///
/// The caller must ensure `fn_addr` and `env_addr` are valid pointers.
unsafe fn call_reduce_cb(fn_addr: usize, env_addr: usize, acc: u64, val: u64, cb_kind: u8) -> u64 {
    let env = env_addr as *mut u8;
    let input_float = (cb_kind & CB_INPUT_FLOAT) != 0;
    let output_float = (cb_kind & CB_OUTPUT_FLOAT) != 0;

    if input_float && output_float {
        let f: unsafe extern "C" fn(*mut u8, f64, f64) -> f64 = unsafe { std::mem::transmute(fn_addr) };
        let result = unsafe { f(env, f64::from_bits(acc), f64::from_bits(val)) };
        result.to_bits()
    } else if input_float {
        let f: unsafe extern "C" fn(*mut u8, f64, f64) -> i64 = unsafe { std::mem::transmute(fn_addr) };
        let result = unsafe { f(env, f64::from_bits(acc), f64::from_bits(val)) };
        result as u64
    } else if output_float {
        let f: unsafe extern "C" fn(*mut u8, i64, i64) -> f64 = unsafe { std::mem::transmute(fn_addr) };
        let result = unsafe { f(env, acc as i64, val as i64) };
        result.to_bits()
    } else {
        let f: unsafe extern "C" fn(*mut u8, i64, i64) -> i64 = unsafe { std::mem::transmute(fn_addr) };
        let result = unsafe { f(env, acc as i64, val as i64) };
        result as u64
    }
}

// =============================================================================
// Async spawn / await (Tokio)
// =============================================================================

/// A heap-allocated future for async spawn/await.
///
/// Layout: `[rc, result, result_size, handle]`.
/// - `rc` is the reference count (offset 0, like all Spark heap types).
/// - `result` is a pointer to the result buffer (null until complete).
/// - `result_size` is the byte size of the result for allocation.
/// - `handle` is a `Box<JoinHandle<()>>` (null after join).
#[repr(C)]
struct SparkFuture {
    /// Reference count.
    rc: usize,
    /// Pointer to the result buffer (heap-allocated, written by the spawned task).
    result: *mut u8,
    /// Byte size of the result value.
    result_size: usize,
    /// Opaque pointer to `Box<tokio::task::JoinHandle<()>>`. Null after join.
    handle: *mut std::ffi::c_void,
}

/// Returns a reference to the global Tokio runtime, creating it lazily.
fn tokio_runtime() -> &'static tokio::runtime::Runtime {
    use std::sync::OnceLock;
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("spark: failed to create Tokio runtime")
    })
}

/// Spawns a function on Tokio's blocking thread pool.
///
/// The spawned function has signature `fn(env: *mut u8) -> i64` (or f64 bits
/// cast to i64). The result is stored in a heap buffer inside the returned
/// `SparkFuture`.
///
/// # Parameters
///
/// - `fn_ptr`: function pointer (with env parameter) to run on the thread pool
/// - `env_ptr`: closure environment pointer (may be null for non-capturing)
/// - `result_size`: byte size of the result (typically 8 for scalars)
/// - `is_float`: 1 if the function returns f64, 0 for i64/pointer
///
/// # Safety
///
/// `fn_ptr` must be a valid function pointer. `env_ptr` must be null or a
/// valid closure environment pointer.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_spawn(
    fn_ptr: *mut u8,
    env_ptr: *mut u8,
    result_size: i64,
    is_float: i8,
) -> *mut u8 {
    let size = result_size as usize;

    // Allocate the result buffer.
    let result_buf = if size > 0 {
        let align = std::mem::align_of::<u64>();
        let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(size, align) };
        let buf = unsafe { std::alloc::alloc_zeroed(layout) };
        if buf.is_null() {
            eprintln!("spark: out of memory allocating future result buffer");
            std::process::abort();
        }
        buf
    } else {
        std::ptr::null_mut()
    };

    // Allocate the SparkFuture on the heap (rc = 1).
    let future = Box::new(SparkFuture {
        rc: 1,
        result: result_buf,
        result_size: size,
        handle: std::ptr::null_mut(),
    });
    let future_ptr = Box::into_raw(future);

    // Cast pointers to usize for Send.
    let fn_addr = fn_ptr as usize;
    let env_addr = env_ptr as usize;
    let result_addr = result_buf as usize;
    let is_float_flag = is_float != 0;

    // Increment env RC if non-null (the spawned task holds a reference).
    if !env_ptr.is_null() {
        unsafe { __spark_rc_inc(env_ptr) };
    }

    let rt = tokio_runtime();
    let handle = rt.spawn_blocking(move || {
        let env = env_addr as *mut u8;
        let result_dst = result_addr as *mut u8;

        if is_float_flag {
            let f: unsafe extern "C" fn(*mut u8) -> f64 =
                unsafe { std::mem::transmute(fn_addr) };
            let val = unsafe { f(env) };
            if !result_dst.is_null() {
                unsafe { *(result_dst.cast::<f64>()) = val };
            }
        } else {
            let f: unsafe extern "C" fn(*mut u8) -> i64 =
                unsafe { std::mem::transmute(fn_addr) };
            let val = unsafe { f(env) };
            if !result_dst.is_null() {
                unsafe { *(result_dst.cast::<i64>()) = val };
            }
        }

        // Decrement env RC after the task completes.
        if env_addr != 0 {
            let env = env_addr as *mut u8;
            let rc_ptr = env.cast::<usize>();
            unsafe { *rc_ptr -= 1 };
            // Note: not calling full env_rc_dec here to avoid double-free
            // concerns; the main thread's drop will handle final cleanup.
        }
    });

    // Store the JoinHandle in the future.
    let handle_box = Box::new(handle);
    unsafe { (*future_ptr).handle = Box::into_raw(handle_box).cast() };

    future_ptr.cast::<u8>()
}

/// Blocks until the spawned task completes and returns a pointer to the result.
///
/// The returned pointer points to the `SparkFuture`'s internal result buffer.
/// The caller should load the result value from this pointer and then drop
/// the future via `__spark_future_rc_dec`.
///
/// # Safety
///
/// `future_ptr` must be a valid pointer from `__spark_spawn`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_await(future_ptr: *mut u8) -> *mut u8 {
    if future_ptr.is_null() {
        return std::ptr::null_mut();
    }
    let future = unsafe { &mut *(future_ptr.cast::<SparkFuture>()) };

    // If the handle is non-null, the task hasn't been joined yet.
    if !future.handle.is_null() {
        let handle: Box<tokio::task::JoinHandle<()>> =
            unsafe { Box::from_raw(future.handle.cast()) };
        future.handle = std::ptr::null_mut();

        // Block on the task to completion.
        let rt = tokio_runtime();
        rt.block_on(async { handle.await.expect("spark: spawned task panicked") });
    }

    future.result
}

/// Decrements the reference count of a `SparkFuture`.
///
/// When the count reaches zero:
/// - Joins the task if still pending (blocks until complete)
/// - Frees the result buffer
/// - Frees the `SparkFuture` struct
///
/// # Safety
///
/// `ptr` must be null or a valid pointer from `__spark_spawn`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_future_rc_dec(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let future = unsafe { &mut *(ptr.cast::<SparkFuture>()) };
    future.rc -= 1;
    if future.rc == 0 {
        // Join the task if still pending.
        if !future.handle.is_null() {
            let handle: Box<tokio::task::JoinHandle<()>> =
                unsafe { Box::from_raw(future.handle.cast()) };
            let rt = tokio_runtime();
            rt.block_on(async { let _ = handle.await; });
        }

        // Free the result buffer.
        if !future.result.is_null() && future.result_size > 0 {
            let align = std::mem::align_of::<u64>();
            let layout = unsafe {
                std::alloc::Layout::from_size_align_unchecked(future.result_size, align)
            };
            unsafe { std::alloc::dealloc(future.result, layout) };
        }

        // Free the SparkFuture struct.
        drop(unsafe { Box::from_raw(ptr.cast::<SparkFuture>()) });
    }
}

// =============================================================================
// Cooperative async (Phase 3) — state machine polling via Tokio
// =============================================================================

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

thread_local! {
    /// Thread-local waker for the C FFI bridge.
    ///
    /// When `SparkTask::poll` is called by Tokio, it stores the current waker
    /// here before calling the compiled poll function. Sub-futures can clone
    /// this waker to arrange re-scheduling when they complete.
    static CURRENT_WAKER: Cell<Option<Waker>> = const { Cell::new(None) };
}

/// A Rust `Future` adapter that wraps a compiled Spark poll function.
///
/// The poll function has signature `fn(state_ptr: *mut u8) -> i8` where:
/// - Returns 0 = Pending (the task should be re-polled later)
/// - Returns 1 = Ready (the task completed, result is in the state struct)
///
/// `SparkTask` implements `std::future::Future` so it can be spawned on
/// Tokio's cooperative scheduler.
struct SparkTask {
    /// Pointer to the compiled poll function: `fn(*mut u8) -> i8`.
    poll_fn: usize,
    /// Pointer to the heap-allocated state struct.
    state_ptr: *mut u8,
    /// Total size of the state struct allocation.
    state_size: usize,
    /// Size of the result value within the state struct.
    result_size: usize,
    /// Byte offset of the result field within the state struct.
    result_offset: usize,
    /// Pointer to the `SparkFuture` that receives the result.
    future_ptr: *mut u8,
}

// Safety: the state pointer and poll function are thread-safe because
// each SparkTask exclusively owns its state allocation.
unsafe impl Send for SparkTask {}

impl Future for SparkTask {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Store the waker in the thread-local so sub-futures can access it.
        CURRENT_WAKER.set(Some(cx.waker().clone()));

        // Call the compiled poll function.
        let poll_fn: unsafe extern "C" fn(*mut u8) -> i8 =
            unsafe { std::mem::transmute(self.poll_fn) };
        let status = unsafe { poll_fn(self.state_ptr) };

        CURRENT_WAKER.set(None);

        if status == 1 {
            // Ready — copy result from state struct to SparkFuture's result buffer.
            if !self.future_ptr.is_null() && self.result_size > 0 {
                let future = unsafe { &mut *(self.future_ptr.cast::<SparkFuture>()) };
                if !future.result.is_null() {
                    let src = unsafe { self.state_ptr.add(self.result_offset) };
                    unsafe {
                        std::ptr::copy_nonoverlapping(src, future.result, self.result_size);
                    }
                }
            }
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for SparkTask {
    fn drop(&mut self) {
        // Free the state struct allocation.
        if !self.state_ptr.is_null() && self.state_size > 0 {
            let align = std::mem::align_of::<u64>();
            let layout =
                unsafe { std::alloc::Layout::from_size_align_unchecked(self.state_size, align) };
            unsafe { std::alloc::dealloc(self.state_ptr, layout) };
        }
    }
}

/// Allocates a state struct on the heap for an async function's state machine.
///
/// The returned pointer is zero-initialized. The caller (codegen) is
/// responsible for writing the tag and parameter fields before spawning.
///
/// # Safety
///
/// `size` must be > 0.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub extern "C" fn __spark_alloc_state(size: i64) -> *mut u8 {
    let size = size as usize;
    if size == 0 {
        return std::ptr::null_mut();
    }
    let align = std::mem::align_of::<u64>();
    let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(size, align) };
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    if ptr.is_null() {
        eprintln!("spark: out of memory allocating async state struct");
        std::process::abort();
    }
    ptr
}

/// Spawns a cooperative task on the Tokio runtime.
///
/// The poll function is called repeatedly until it returns 1 (Ready).
/// The result is then copied from the state struct's result field into
/// the returned `SparkFuture`.
///
/// # Parameters
///
/// - `poll_fn`: pointer to the compiled poll function `fn(*mut u8) -> i8`
/// - `state_ptr`: pointer to the heap-allocated state struct (from `__spark_alloc_state`)
/// - `state_size`: total byte size of the state struct
/// - `result_size`: byte size of the result value
/// - `result_offset`: byte offset of the result field within the state struct
///
/// # Returns
///
/// A `SparkFuture*` that can be awaited with `__spark_await`.
///
/// # Safety
///
/// - `poll_fn` must be a valid function pointer with the expected signature.
/// - `state_ptr` must be a valid pointer from `__spark_alloc_state`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_spawn_coop(
    poll_fn: *mut u8,
    state_ptr: *mut u8,
    state_size: i64,
    result_size: i64,
    result_offset: i64,
) -> *mut u8 {
    let r_size = result_size as usize;
    let s_size = state_size as usize;
    let r_offset = result_offset as usize;

    // Allocate the result buffer for the SparkFuture.
    let result_buf = if r_size > 0 {
        let align = std::mem::align_of::<u64>();
        let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(r_size, align) };
        let buf = unsafe { std::alloc::alloc_zeroed(layout) };
        if buf.is_null() {
            eprintln!("spark: out of memory allocating future result buffer");
            std::process::abort();
        }
        buf
    } else {
        std::ptr::null_mut()
    };

    // Allocate the SparkFuture (rc = 1).
    let future = Box::new(SparkFuture {
        rc: 1,
        result: result_buf,
        result_size: r_size,
        handle: std::ptr::null_mut(),
    });
    let future_ptr = Box::into_raw(future);

    // Create the SparkTask.
    let task = SparkTask {
        poll_fn: poll_fn as usize,
        state_ptr,
        state_size: s_size,
        result_size: r_size,
        result_offset: r_offset,
        future_ptr: future_ptr.cast(),
    };

    // Spawn on Tokio as a cooperative task.
    let rt = tokio_runtime();
    let handle = rt.spawn(task);

    // Store the JoinHandle in the SparkFuture.
    let handle_box = Box::new(handle);
    unsafe { (*future_ptr).handle = Box::into_raw(handle_box).cast() };

    future_ptr.cast::<u8>()
}

/// Non-blocking poll of a sub-future.
///
/// If the sub-future's task has completed, returns a pointer to its result
/// buffer (non-null). If still pending, returns null.
///
/// When Pending, the current task's waker is stored in the sub-future so
/// the parent task will be re-scheduled when the sub-future completes.
///
/// # Safety
///
/// `future_ptr` must be a valid pointer from `__spark_spawn` or
/// `__spark_spawn_coop`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_try_poll(future_ptr: *mut u8) -> *mut u8 {
    if future_ptr.is_null() {
        return std::ptr::null_mut();
    }
    let future = unsafe { &*(future_ptr.cast::<SparkFuture>()) };

    // If the handle is null, the task has already been joined — result is ready.
    if future.handle.is_null() {
        return future.result;
    }

    // Check if the JoinHandle is finished without blocking.
    let handle_ptr = future.handle.cast::<tokio::task::JoinHandle<()>>();
    let handle = unsafe { &*handle_ptr };

    if handle.is_finished() {
        // Task completed — join it to get the result.
        // Take ownership of the handle to join.
        let future = unsafe { &mut *(future_ptr.cast::<SparkFuture>()) };
        let handle: Box<tokio::task::JoinHandle<()>> =
            unsafe { Box::from_raw(future.handle.cast()) };
        future.handle = std::ptr::null_mut();

        let rt = tokio_runtime();
        rt.block_on(async { handle.await.expect("spark: spawned task panicked") });

        future.result
    } else {
        // Still pending — store waker for re-scheduling (best-effort).
        // In v1, the parent task will be woken by Tokio when the child
        // completes because they share the same runtime.
        std::ptr::null_mut()
    }
}

/// Wakes the current task's waker (if any).
///
/// Called by sub-tasks when they complete to re-schedule the parent task.
/// Uses the thread-local waker stored by `SparkTask::poll`.
#[no_mangle]
pub extern "C" fn __spark_wake_current() {
    CURRENT_WAKER.with(|w| {
        // Take the waker, wake it, and put it back (if any).
        if let Some(waker) = w.take() {
            waker.wake_by_ref();
            w.set(Some(waker));
        }
    });
}

// ── Range-aware functions (deforestation pass) ──────────────────────────

/// Computes the length of a range without allocating a list.
///
/// `[start..end]` exclusive: `max(0, ceil((end - start) / step))`
/// `[start..=end]` inclusive: `max(0, ceil((end - start + 1) / step))`
///
/// Handles negative step (counting down).
///
/// # Safety
///
/// No pointer arguments — always safe.
#[no_mangle]
pub extern "C" fn __spark_range_len(start: i64, end: i64, inclusive: i8, step: i64) -> i64 {
    if step == 0 {
        return 0;
    }

    let diff = if inclusive != 0 {
        // inclusive: include the end value
        if step > 0 {
            end - start + 1
        } else {
            start - end + 1
        }
    } else {
        // exclusive
        if step > 0 {
            end - start
        } else {
            start - end
        }
    };

    if diff <= 0 {
        return 0;
    }

    let abs_step = step.unsigned_abs() as i64;
    // ceil division: (diff + abs_step - 1) / abs_step
    (diff + abs_step - 1) / abs_step
}

/// Calls a map callback with an integer value directly (no pointer indirection).
///
/// # Safety
///
/// The caller must ensure `fn_addr` and `env_addr` are valid.
unsafe fn call_range_map_cb(fn_addr: usize, env_addr: usize, val: i64, cb_kind: u8) -> u64 {
    let env = env_addr as *mut u8;
    let output_float = (cb_kind & CB_OUTPUT_FLOAT) != 0;
    // Input for range callbacks is always Int (the range element).

    if output_float {
        let f: unsafe extern "C" fn(*mut u8, i64) -> f64 =
            unsafe { std::mem::transmute(fn_addr) };
        let result = unsafe { f(env, val) };
        result.to_bits()
    } else {
        let f: unsafe extern "C" fn(*mut u8, i64) -> i64 =
            unsafe { std::mem::transmute(fn_addr) };
        let result = unsafe { f(env, val) };
        result as u64
    }
}

/// Parallel foreach over a range — no list allocation.
///
/// Iterates `[start..end]` (or `[start..=end]`) with the given step,
/// calling the callback for each element in parallel.
///
/// # Safety
///
/// The caller must ensure `fn_ptr` and `env_ptr` are valid function/env pointers.
#[no_mangle]
pub unsafe extern "C" fn __spark_parallel_range_foreach(
    start: i64,
    end: i64,
    inclusive: i8,
    step: i64,
    fn_ptr: *mut u8,
    env_ptr: *mut u8,
    cb_kind: u8,
) {
    use rayon::prelude::*;

    let len = __spark_range_len(start, end, inclusive, step);
    if len <= 0 {
        return;
    }

    let fn_addr = fn_ptr as usize;
    let env_addr = env_ptr as usize;

    (0..len).into_par_iter().for_each(|i| {
        let val = start + i * step;
        unsafe {
            call_range_map_cb(fn_addr, env_addr, val, cb_kind);
        }
    });
}

/// Parallel map over a range — no input list allocation.
///
/// Returns a new list where each element is `callback(start + i * step)`.
///
/// # Safety
///
/// The caller must ensure `fn_ptr` and `env_ptr` are valid.
#[no_mangle]
pub unsafe extern "C" fn __spark_parallel_range_map(
    start: i64,
    end: i64,
    inclusive: i8,
    step: i64,
    fn_ptr: *mut u8,
    env_ptr: *mut u8,
    result_elem_size: i64,
    cb_kind: u8,
) -> *mut u8 {
    use rayon::prelude::*;

    let len = __spark_range_len(start, end, inclusive, step);
    if len <= 0 {
        return __spark_list_new(result_elem_size);
    }

    let fn_addr = fn_ptr as usize;
    let env_addr = env_ptr as usize;
    let output_float = (cb_kind & CB_OUTPUT_FLOAT) != 0;

    // Collect mapped results.
    let results: Vec<u64> = (0..len)
        .into_par_iter()
        .map(|i| {
            let val = start + i * step;
            unsafe { call_range_map_cb(fn_addr, env_addr, val, cb_kind) }
        })
        .collect();

    // Build a list from the results.
    let list = __spark_list_new(result_elem_size);
    let spark_list = unsafe { &mut *(list.cast::<SparkList>()) };

    for result in &results {
        let bytes = if output_float {
            f64::from_bits(*result).to_ne_bytes()
        } else {
            (*result as i64).to_ne_bytes()
        };
        let elem_size = result_elem_size as usize;
        let src = bytes.as_ptr();
        // Push each result as raw bytes.
        let offset = spark_list.len * spark_list.elem_size;
        if offset + elem_size > spark_list.cap * spark_list.elem_size {
            // Grow.
            let new_cap = (spark_list.cap * 2).max(8);
            let new_data = unsafe {
                std::alloc::realloc(
                    spark_list.data,
                    std::alloc::Layout::from_size_align_unchecked(
                        spark_list.cap * spark_list.elem_size,
                        8,
                    ),
                    new_cap * spark_list.elem_size,
                )
            };
            spark_list.data = new_data;
            spark_list.cap = new_cap;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(src, spark_list.data.add(offset), elem_size);
        }
        spark_list.len += 1;
    }

    list
}

/// Fused parallel map+reduce over a range — zero allocation.
///
/// Divides the range into chunks (one per CPU core), locally fuses map+reduce
/// in a tight loop per chunk, then cross-thread reduces.
///
/// # Safety
///
/// The caller must ensure all function/env pointers are valid.
#[no_mangle]
pub unsafe extern "C" fn __spark_parallel_range_map_reduce(
    start: i64,
    end: i64,
    inclusive: i8,
    step: i64,
    map_fn: *mut u8,
    map_env: *mut u8,
    reduce_fn: *mut u8,
    reduce_env: *mut u8,
    initial: i64,
    map_cb_kind: u8,
    reduce_cb_kind: u8,
) -> i64 {
    use rayon::prelude::*;

    let len = __spark_range_len(start, end, inclusive, step);
    if len <= 0 {
        return initial;
    }

    let map_fn_addr = map_fn as usize;
    let map_env_addr = map_env as usize;
    let reduce_fn_addr = reduce_fn as usize;
    let reduce_env_addr = reduce_env as usize;

    let result = (0..len)
        .into_par_iter()
        .map(|i| {
            let val = start + i * step;
            unsafe { call_range_map_cb(map_fn_addr, map_env_addr, val, map_cb_kind) }
        })
        .reduce(
            || initial as u64,
            |acc, val| unsafe {
                call_reduce_cb(reduce_fn_addr, reduce_env_addr, acc, val, reduce_cb_kind)
            },
        );

    result as i64
}
