//! List runtime storage and operations.

pub use spark_abi::{SORT_TAG_BOOL, SORT_TAG_FLOAT, SORT_TAG_INT, SORT_TAG_STRING};
use spark_abi::{
    SORT_TAG_I16, SORT_TAG_I32, SORT_TAG_I8, SORT_TAG_U16, SORT_TAG_U32, SORT_TAG_U64, SORT_TAG_U8,
};

use crate::value::{
    buffer_align, value_desc_clone, value_desc_free, value_desc_new, SparkValueDesc,
};
use crate::SparkString;

use super::storage::{alloc_buffer, byte_size, clone_buffer, dealloc_buffer, realloc_buffer};
use super::MIN_CAP;

// =============================================================================
// List — type-erased growable array with reference counting
// =============================================================================

/// Type-erased growable list. All elements are stored as raw bytes.
/// The `rc` field is always at offset 0 for generic RC operations.
#[repr(C)]
pub struct SparkList {
    /// Reference count.
    pub rc: usize,
    pub elem_desc: *mut SparkValueDesc,
    /// Pointer to the contiguous data buffer.
    pub data: *mut u8,
    /// Number of elements currently stored.
    pub len: usize,
    /// Number of elements the buffer can hold before reallocation.
    pub cap: usize,
}

impl SparkList {
    fn elem(&self) -> &SparkValueDesc {
        unsafe { &*self.elem_desc }
    }

    #[must_use]
    pub fn elem_size(&self) -> usize {
        self.elem().size
    }

    #[must_use]
    pub fn elem_align(&self) -> usize {
        self.elem().align
    }
}

/// Creates a new empty list (rc = 1).
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub extern "C" fn __spark_list_new(elem_desc: *mut u8) -> *mut u8 {
    if elem_desc.is_null() {
        eprintln!("spark: null list element descriptor");
        std::process::abort();
    }
    let list = Box::new(SparkList {
        rc: 1,
        elem_desc: elem_desc.cast(),
        data: std::ptr::null_mut(),
        len: 0,
        cap: 0,
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
    let dest = unsafe { list.data.add(list.len * list.elem_size()) };
    unsafe { std::ptr::copy_nonoverlapping(elem_ptr, dest, list.elem_size()) };
    list.len += 1;
}

/// Returns a pointer to the element at `index`. Aborts on out-of-bounds.
///
/// # Safety
///
/// `list` must be a valid pointer from `__spark_list_new`.
#[no_mangle]
#[allow(
    clippy::cast_ptr_alignment,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub unsafe extern "C" fn __spark_list_get(list: *mut u8, index: i64) -> *mut u8 {
    let list = unsafe { &*(list.cast::<SparkList>()) };
    let idx = index as usize;
    if idx >= list.len {
        eprintln!(
            "spark: list index out of bounds: index {idx}, length {}",
            list.len
        );
        std::process::abort();
    }
    unsafe { list.data.add(idx * list.elem_size()) }
}

/// Returns a borrowed element pointer, or null when the index is out of bounds.
///
/// # Safety
///
/// `list` must be a valid pointer from `__spark_list_new`. The result remains
/// valid only while the backing buffer is alive and unchanged.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_list_try_get(list: *const u8, index: u64) -> *const u8 {
    let list = unsafe { &*(list.cast::<SparkList>()) };
    let Ok(index) = usize::try_from(index) else {
        return std::ptr::null();
    };
    if index >= list.len {
        return std::ptr::null();
    }
    unsafe { list.data.add(index * list.elem_size()) }
}

/// Gets a list slot for indexed assignment, terminating on a bounds violation.
///
/// # Safety
///
/// `list` must be a live list. The returned slot is borrowed from its buffer.
#[no_mangle]
pub unsafe extern "C" fn __spark_list_write_slot(list: *mut u8, index: u64, signed: u8) -> *mut u8 {
    let slot = unsafe { __spark_list_try_get(list, index) };
    if slot.is_null() {
        let len = unsafe { __spark_list_len(list) };
        if signed != 0 {
            let index = i64::from_ne_bytes(index.to_ne_bytes());
            eprintln!("spark: list write index out of bounds: index {index}, length {len}");
        } else {
            eprintln!("spark: list write index out of bounds: index {index}, length {len}");
        }
        std::process::abort();
    }
    slot.cast_mut()
}

/// Replaces an element, dropping the old value and consuming the incoming bytes.
///
/// # Safety
///
/// `slot` must be a live element slot in a unique list. `value` must point to
/// independently staged bytes of the list's element type.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_list_replace(list: *mut u8, slot: *mut u8, value: *const u8) {
    let list = unsafe { &*list.cast::<SparkList>() };
    if list.elem_size() != 0 {
        unsafe {
            list.elem().drop_elem(slot);
            std::ptr::copy_nonoverlapping(value, slot, list.elem_size());
        }
    }
}

/// Grows the list's backing buffer, doubling capacity (minimum [`MIN_CAP`]).
fn list_grow(list: &mut SparkList) {
    let new_cap = if list.cap == 0 { MIN_CAP } else { list.cap * 2 };
    let new_size = byte_size(new_cap, list.elem_size());
    let align = buffer_align(list.elem_align());

    let new_data = if list.data.is_null() {
        unsafe { alloc_buffer(new_size, align, "spark: out of memory allocating list") }
    } else {
        let old_size = byte_size(list.cap, list.elem_size());
        unsafe {
            realloc_buffer(
                list.data,
                old_size,
                new_size,
                align,
                "spark: out of memory allocating list",
            )
        }
    };

    list.data = new_data;
    list.cap = new_cap;
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
/// Uses `elem_eq` when provided, else byte-level comparison (`memcmp`).
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
        let existing = unsafe { list.data.add(i * list.elem_size()) };
        if unsafe { list.elem().eq_elem(existing, elem_ptr) } {
            return 1;
        }
    }
    0
}

/// Removes the element at `index` from the list. Aborts on out-of-bounds.
/// Calls the element destructor when the list descriptor has one.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_list_new`.
#[no_mangle]
#[allow(
    clippy::cast_ptr_alignment,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub unsafe extern "C" fn __spark_list_remove(ptr: *mut u8, index: i64) {
    let list = unsafe { &mut *(ptr.cast::<SparkList>()) };
    let idx = index as usize;
    if idx >= list.len {
        eprintln!(
            "spark: list remove index out of bounds: index {idx}, length {}",
            list.len
        );
        std::process::abort();
    }

    let elem_addr = unsafe { list.data.add(idx * list.elem_size()) };

    unsafe { list.elem().drop_elem(elem_addr) };

    // Shift remaining elements left.
    let remaining = list.len - idx - 1;
    if remaining > 0 {
        let src = unsafe { elem_addr.add(list.elem_size()) };
        unsafe { std::ptr::copy(src, elem_addr, remaining * list.elem_size()) };
    }
    list.len -= 1;
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
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_ptr_alignment
)]
pub extern "C" fn __spark_list_from_range(
    start: i64,
    end: i64,
    inclusive: i8,
    step: i64,
) -> *mut u8 {
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
    let elem_desc = value_desc_new(elem_size, std::mem::align_of::<i64>(), 0, 0, 0, 0);
    let list_ptr = __spark_list_new(elem_desc.cast());

    if count == 0 {
        return list_ptr;
    }

    // Pre-allocate exact capacity.
    let list = unsafe { &mut *(list_ptr.cast::<SparkList>()) };
    let buf_size = byte_size(count, elem_size);
    let align = std::mem::align_of::<u64>();
    let buf = unsafe {
        alloc_buffer(
            buf_size,
            align,
            "spark: out of memory allocating list range",
        )
    };
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

/// Sorts a list in ascending order, returning a new list.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_list_new`.
/// Elements must have the representation selected by the shared ABI `type_tag`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_list_sort(ptr: *mut u8, type_tag: i64) -> *mut u8 {
    let list = unsafe { &*(ptr.cast::<SparkList>()) };
    let mut indices: Vec<_> = (0..list.len).collect();

    unsafe {
        match type_tag {
            SORT_TAG_I8 => sort_slots::<i8>(list, &mut indices, Ord::cmp),
            SORT_TAG_I16 => sort_slots::<i16>(list, &mut indices, Ord::cmp),
            SORT_TAG_I32 => sort_slots::<i32>(list, &mut indices, Ord::cmp),
            SORT_TAG_INT => sort_slots::<i64>(list, &mut indices, Ord::cmp),
            SORT_TAG_U8 | SORT_TAG_BOOL => sort_slots::<u8>(list, &mut indices, Ord::cmp),
            SORT_TAG_U16 => sort_slots::<u16>(list, &mut indices, Ord::cmp),
            SORT_TAG_U32 => sort_slots::<u32>(list, &mut indices, Ord::cmp),
            SORT_TAG_U64 => sort_slots::<u64>(list, &mut indices, Ord::cmp),
            SORT_TAG_FLOAT => sort_slots::<f64>(list, &mut indices, f64::total_cmp),
            SORT_TAG_STRING => sort_slots::<*const SparkString>(list, &mut indices, |&a, &b| {
                crate::string::spark_str_bytes(&*a).cmp(crate::string::spark_str_bytes(&*b))
            }),
            _ => {
                eprintln!("spark: invalid list sort selector");
                std::process::abort();
            }
        }
    }

    let new_list = __spark_list_new(unsafe { value_desc_clone(list.elem_desc) }.cast());
    for idx in indices {
        let elem = unsafe { list.data.add(idx * list.elem_size()) };
        unsafe { list.elem().retain_elem(elem) };
        unsafe { __spark_list_push(new_list, elem) };
    }
    new_list
}

/// # Safety
///
/// Each slot must contain a valid value of `T`.
unsafe fn sort_slots<T: Copy>(
    list: &SparkList,
    indices: &mut [usize],
    compare: impl Fn(&T, &T) -> std::cmp::Ordering,
) {
    let width = std::mem::size_of::<T>();
    if list.elem_size() != width {
        eprintln!("spark: invalid list sort slot size");
        std::process::abort();
    }
    indices.sort_unstable_by(|&a, &b| {
        let left = unsafe { list.data.add(a * width).cast::<T>().read_unaligned() };
        let right = unsafe { list.data.add(b * width).cast::<T>().read_unaligned() };
        compare(&left, &right)
    });
}

/// Takes the first `count` elements from a list, returning a new list.
///
/// If `count` exceeds the list length, returns all elements.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_list_new`.
#[no_mangle]
#[allow(
    clippy::cast_ptr_alignment,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
pub unsafe extern "C" fn __spark_list_take(ptr: *mut u8, count: i64) -> *mut u8 {
    let list = unsafe { &*(ptr.cast::<SparkList>()) };
    let new_list = __spark_list_new(unsafe { value_desc_clone(list.elem_desc) }.cast());

    let n = (count as usize).min(list.len);
    for i in 0..n {
        let elem = unsafe { list.data.add(i * list.elem_size()) };
        unsafe { list.elem().retain_elem(elem) };
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
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_wrap)]
pub unsafe extern "C" fn __spark_list_reverse(ptr: *mut u8) -> *mut u8 {
    let list = unsafe { &*(ptr.cast::<SparkList>()) };
    let new_list = __spark_list_new(unsafe { value_desc_clone(list.elem_desc) }.cast());

    for i in (0..list.len).rev() {
        let elem = unsafe { list.data.add(i * list.elem_size()) };
        unsafe { list.elem().retain_elem(elem) };
        unsafe { __spark_list_push(new_list, elem) };
    }

    new_list
}

/// Decrements the reference count of a `SparkList`.
/// When the count reaches zero, calls the element destructor on each
/// element for recursive cleanup, then frees the data buffer and struct.
///
/// # Safety
///
/// `ptr` must be a valid pointer from `__spark_list_new`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_list_rc_dec(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let list = unsafe { &mut *(ptr.cast::<SparkList>()) };
    list.rc -= 1;
    if list.rc == 0 {
        if list.elem().has_drop() && !list.data.is_null() {
            for i in 0..list.len {
                let elem_ptr = unsafe { list.data.add(i * list.elem_size()) };
                unsafe { list.elem().drop_elem(elem_ptr) };
            }
        }
        if !list.data.is_null() && list.cap > 0 {
            let align = buffer_align(list.elem_align());
            let buf_size = byte_size(list.cap, list.elem_size());
            unsafe { dealloc_buffer(list.data, buf_size, align) };
        }
        unsafe { value_desc_free(list.elem_desc) };
        drop(unsafe { Box::from_raw(ptr.cast::<SparkList>()) });
    }
}

/// COW for lists. If rc > 1, shallow-copies the data buffer and retains
/// each copied element when the descriptor has retain glue.
///
/// # Safety
///
/// `ptr` must be a valid `SparkList` pointer.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_list_cow(ptr: *mut u8) -> *mut u8 {
    if ptr.is_null() {
        return ptr;
    }
    let list = unsafe { &*(ptr.cast::<SparkList>()) };
    if list.rc == 1 {
        return ptr;
    }
    let data_byte_size = byte_size(list.cap, list.elem_size());
    let copy_byte_size = byte_size(list.len, list.elem_size());
    let align = buffer_align(list.elem_align());
    let new_data = unsafe {
        clone_buffer(
            list.data,
            data_byte_size,
            copy_byte_size,
            align,
            "spark: out of memory in list COW copy",
        )
    };
    if !new_data.is_null() && list.elem().has_retain() {
        for i in 0..list.len {
            let elem_loc = unsafe { new_data.add(i * list.elem_size()) };
            unsafe { list.elem().retain_elem(elem_loc) };
        }
    }
    let new = Box::new(SparkList {
        rc: 1,
        elem_desc: unsafe { value_desc_clone(list.elem_desc) },
        data: new_data,
        len: list.len,
        cap: list.cap,
    });
    // Decrement old rc.
    let old = unsafe { &mut *(ptr.cast::<SparkList>()) };
    old.rc -= 1;
    Box::into_raw(new).cast::<u8>()
}
