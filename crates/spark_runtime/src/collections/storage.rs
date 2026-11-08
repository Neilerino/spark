//! Raw byte-buffer helpers for collection storage.

use crate::value::SparkValueDesc;

#[must_use]
pub(super) const fn byte_size(cap: usize, elem_size: usize) -> usize {
    cap * elem_size
}

pub(super) unsafe fn alloc_buffer(byte_size: usize, align: usize, oom_message: &str) -> *mut u8 {
    let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(byte_size, align) };
    let ptr = unsafe { std::alloc::alloc(layout) };
    abort_if_null(ptr, oom_message)
}

pub(super) unsafe fn alloc_zeroed_buffer(
    byte_size: usize,
    align: usize,
    oom_message: &str,
) -> *mut u8 {
    let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(byte_size, align) };
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    abort_if_null(ptr, oom_message)
}

pub(super) unsafe fn realloc_buffer(
    ptr: *mut u8,
    old_byte_size: usize,
    new_byte_size: usize,
    align: usize,
    oom_message: &str,
) -> *mut u8 {
    let old_layout = unsafe { std::alloc::Layout::from_size_align_unchecked(old_byte_size, align) };
    let ptr = unsafe { std::alloc::realloc(ptr, old_layout, new_byte_size) };
    abort_if_null(ptr, oom_message)
}

pub(super) unsafe fn clone_buffer(
    src: *const u8,
    alloc_byte_size: usize,
    copy_byte_size: usize,
    align: usize,
    oom_message: &str,
) -> *mut u8 {
    if src.is_null() || alloc_byte_size == 0 {
        return std::ptr::null_mut();
    }
    let dst = unsafe { alloc_buffer(alloc_byte_size, align, oom_message) };
    if copy_byte_size > 0 {
        unsafe { std::ptr::copy_nonoverlapping(src, dst, copy_byte_size) };
    }
    dst
}

/// Copies one live payload slot and retains the copied owner.
///
/// # Safety
///
/// The source buffer must contain the indexed slot. The destination must be
/// separate, aligned storage large enough for the descriptor.
pub(super) unsafe fn copy_owned_slot(
    descriptor: &SparkValueDesc,
    source: *const u8,
    index: usize,
    destination: *mut u8,
) {
    unsafe {
        std::ptr::copy_nonoverlapping(
            source.add(index * descriptor.size),
            destination,
            descriptor.size,
        );
        descriptor.retain_elem(destination);
    }
}

pub(super) unsafe fn dealloc_buffer(ptr: *mut u8, byte_size: usize, align: usize) {
    if !ptr.is_null() && byte_size > 0 {
        let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(byte_size, align) };
        unsafe { std::alloc::dealloc(ptr, layout) };
    }
}

fn abort_if_null(ptr: *mut u8, message: &str) -> *mut u8 {
    if ptr.is_null() {
        eprintln!("{message}");
        std::process::abort();
    }
    ptr
}
