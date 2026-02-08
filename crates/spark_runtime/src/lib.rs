//! Spark runtime library.
//!
//! Provides `extern "C"` functions that are called by compiled Spark programs.
//! This crate is compiled as a static library (`libspark_runtime.a`) and linked
//! into every Spark executable.

// Safety: This crate is an FFI boundary — `#[no_mangle]` and raw pointers
// are required for the C ABI. No actual unsafe blocks are used.
#![allow(unsafe_code)]
#![allow(clippy::missing_const_for_fn)] // extern "C" FFI stubs are not const

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
    let s = unsafe { &*(ptr.cast::<String>()) };
    println!("{s}");
}

/// Prints an empty newline.
#[no_mangle]
pub extern "C" fn __spark_println() {
    println!();
}

// =============================================================================
// String functions
// =============================================================================

/// Creates a new empty heap-allocated string.
#[no_mangle]
pub extern "C" fn __spark_string_new() -> *mut u8 {
    let s = Box::new(String::new());
    Box::into_raw(s).cast::<u8>()
}

/// Creates a heap-allocated string from a UTF-8 literal (pointer + length).
///
/// # Safety
///
/// `data` must be a valid pointer to `len` bytes of UTF-8 data.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_string_from_literal(data: *const u8, len: i64) -> *mut u8 {
    let slice = unsafe { std::slice::from_raw_parts(data, len as usize) };
    let s = Box::new(String::from_utf8_lossy(slice).into_owned());
    Box::into_raw(s).cast::<u8>()
}

// =============================================================================
// List — type-erased growable array
// =============================================================================

/// Minimum capacity for list/set/map backing arrays.
const MIN_CAP: usize = 4;

/// Type-erased growable list. All elements are stored as raw bytes.
#[repr(C)]
struct SparkList {
    /// Pointer to the contiguous data buffer.
    data: *mut u8,
    /// Number of elements currently stored.
    len: usize,
    /// Number of elements the buffer can hold before reallocation.
    cap: usize,
    /// Size of each element in bytes.
    elem_size: usize,
}

/// Creates a new empty list for elements of the given byte size.
///
/// # Safety
///
/// `elem_size` must be > 0.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub extern "C" fn __spark_list_new(elem_size: i64) -> *mut u8 {
    let list = Box::new(SparkList {
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
// Map — type-erased linear-scan map
// =============================================================================

/// Type-erased linear-scan map. Keys and values are stored as raw bytes.
#[repr(C)]
struct SparkMap {
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

/// Creates a new empty map for keys of `key_size` bytes and values of `value_size` bytes.
///
/// # Safety
///
/// Both sizes must be > 0.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub extern "C" fn __spark_map_new(key_size: i64, value_size: i64) -> *mut u8 {
    let map = Box::new(SparkMap {
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
// Reference counting stubs (placeholders)
// =============================================================================

/// Releases a reference-counted object. No-op (placeholder).
#[no_mangle]
pub extern "C" fn __spark_rc_release(_ptr: *mut u8) {
    // No-op until RC is implemented
}
