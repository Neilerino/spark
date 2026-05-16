use std::alloc::Layout;
use std::mem::{align_of, size_of};

#[repr(C)]
struct SparkRcHeader {
    rc: usize,
}

#[repr(C)]
struct SparkEnvHeader {
    rc: usize,
    alloc_size: usize,
    dtor: usize,
}

type EnvDtorFn = unsafe extern "C" fn(*mut u8);

const ENV_HEADER_SIZE: usize = size_of::<SparkEnvHeader>();
const ENV_ALIGN: usize = align_of::<SparkEnvHeader>();
const _: () = assert!(ENV_HEADER_SIZE == 24);

/// Increments the reference count of any Spark heap object.
///
/// # Safety
///
/// `ptr` must be null or point to a Spark heap object whose first field is `rc: usize`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_rc_inc(ptr: *mut u8) {
    if let Some(header) = unsafe { rc_header_mut(ptr) } {
        header.rc += 1;
    }
}

/// Legacy compatibility stub.
#[no_mangle]
pub extern "C" fn __spark_rc_release(_ptr: *mut u8) {}

/// Allocates a closure environment with a 24-byte header.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_ptr_alignment)]
pub extern "C" fn __spark_env_alloc(size: i64) -> *mut u8 {
    let payload_size =
        usize::try_from(size).unwrap_or_else(|_| abort("spark: invalid closure environment size"));
    if payload_size == 0 {
        return std::ptr::null_mut();
    }
    let total_size = ENV_HEADER_SIZE
        .checked_add(payload_size)
        .unwrap_or_else(|| abort("spark: closure environment too large"));
    let ptr = unsafe { alloc_zeroed(env_layout(total_size)) };
    let header = unsafe { &mut *ptr.cast::<SparkEnvHeader>() };
    *header = SparkEnvHeader {
        rc: 1,
        alloc_size: total_size,
        dtor: 0,
    };
    ptr
}

/// Decrements a closure environment reference count.
///
/// # Safety
///
/// `ptr` must be null or a valid pointer returned by `__spark_env_alloc`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_env_rc_dec(ptr: *mut u8) {
    let Some(header) = (unsafe { env_header_mut(ptr) }) else {
        return;
    };
    header.rc -= 1;
    if header.rc == 0 {
        if header.dtor != 0 {
            let dtor_fn = unsafe { env_dtor_fn(header.dtor) };
            unsafe { dtor_fn(ptr) };
        }
        unsafe { std::alloc::dealloc(ptr, env_layout(header.alloc_size)) };
    }
}

#[allow(clippy::cast_ptr_alignment)]
unsafe fn rc_header_mut(ptr: *mut u8) -> Option<&'static mut SparkRcHeader> {
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &mut *ptr.cast::<SparkRcHeader>() })
    }
}

#[allow(clippy::cast_ptr_alignment)]
unsafe fn env_header_mut(ptr: *mut u8) -> Option<&'static mut SparkEnvHeader> {
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &mut *ptr.cast::<SparkEnvHeader>() })
    }
}

fn env_layout(size: usize) -> Layout {
    Layout::from_size_align(size, ENV_ALIGN)
        .unwrap_or_else(|_| abort("spark: invalid closure environment layout"))
}

unsafe fn alloc_zeroed(layout: Layout) -> *mut u8 {
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    if ptr.is_null() {
        abort("spark: out of memory allocating closure environment");
    }
    ptr
}

unsafe fn env_dtor_fn(dtor: usize) -> EnvDtorFn {
    unsafe { std::mem::transmute(dtor) }
}

fn abort(message: &str) -> ! {
    eprintln!("{message}");
    std::process::abort();
}
