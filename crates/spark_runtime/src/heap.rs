use std::alloc::Layout;
use std::mem::{align_of, size_of};
use std::sync::atomic::{fence, AtomicUsize, Ordering};

// TAG [SPARK-152]
#[repr(transparent)]
pub struct RefCount(AtomicUsize);

impl RefCount {
    pub const fn new() -> Self {
        Self(AtomicUsize::new(1))
    }

    pub fn get(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    pub fn retain(&self) {
        let previous = self.0.fetch_add(1, Ordering::Relaxed);
        if previous >= usize::MAX / 2 {
            abort("spark: too many shared owners");
        }
    }

    #[must_use = "the final release requires cleanup"]
    pub fn release(&self) -> bool {
        if self.0.fetch_sub(1, Ordering::Release) == 1 {
            fence(Ordering::Acquire);
            true
        } else {
            false
        }
    }
}

const _: () = assert!(size_of::<RefCount>() == size_of::<usize>());
const _: () = assert!(align_of::<RefCount>() == align_of::<usize>());

#[repr(C)]
struct SparkRcHeader {
    rc: RefCount,
}

#[repr(C)]
struct SparkEnvHeader {
    rc: RefCount,
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
/// `ptr` must be null or point to a live Spark heap object using the common
/// atomic reference-count header.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_rc_inc(ptr: *mut u8) {
    if let Some(header) = unsafe { ptr.cast::<SparkRcHeader>().as_ref() } {
        header.rc.retain();
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
        rc: RefCount::new(),
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
    let Some(header) = (unsafe { ptr.cast::<SparkEnvHeader>().as_ref() }) else {
        return;
    };
    if header.rc.release() {
        let size = header.alloc_size;
        if header.dtor != 0 {
            let dtor_fn = unsafe { env_dtor_fn(header.dtor) };
            unsafe { dtor_fn(ptr) };
        }
        unsafe { std::alloc::dealloc(ptr, env_layout(size)) };
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

#[cfg(test)]
mod tests {
    use super::RefCount;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;

    #[test]
    fn shared_retains_and_releases_preserve_the_live_owner() {
        let count = RefCount::new();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let count = &count;
                scope.spawn(move || {
                    for _ in 0..10000 {
                        count.retain();
                        assert!(!count.release());
                    }
                });
            }
        });
        assert_eq!(count.get(), 1);
        assert!(count.release());
    }

    #[test]
    fn concurrent_final_release_has_one_winner() {
        let count = RefCount::new();
        let barrier = Barrier::new(8);
        let final_releases = AtomicUsize::new(0);
        for _ in 1..8 {
            count.retain();
        }
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let count = &count;
                let barrier = &barrier;
                let final_releases = &final_releases;
                scope.spawn(move || {
                    barrier.wait();
                    if count.release() {
                        final_releases.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(count.get(), 0);
        assert_eq!(final_releases.load(Ordering::SeqCst), 1);
    }
}
