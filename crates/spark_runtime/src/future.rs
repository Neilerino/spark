// TAG [SPARK-117]
// TAG [SPARK-152]
use crate::heap::RefCount;

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use crate::__spark_env_rc_dec;

mod completion;
use completion::Completion;

#[repr(C)]
struct SparkFuture {
    rc: RefCount,
    result: *mut u8,
    result_size: usize,
    result_drop: Option<unsafe extern "C" fn(*mut u8)>,
    completion: Completion,
}

pub fn tokio_runtime() -> &'static tokio::runtime::Runtime {
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

#[no_mangle]
#[allow(
    clippy::cast_ptr_alignment,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
/// Spawns a blocking runtime task.
///
/// # Safety
///
/// `fn_ptr` must match the result ABI. `env_ptr` transfers one owned Spark
/// environment reference, or is null for a callback without captures.
/// `result_drop`, when present, must accept the initialized result slot and
/// release its owned value without deallocating the slot itself.
pub unsafe extern "C" fn __spark_spawn(
    fn_ptr: *mut u8,
    env_ptr: *mut u8,
    result_size: i64,
    is_float: i8,
    result_drop: Option<unsafe extern "C" fn(*mut u8)>,
) -> *mut u8 {
    let size = result_size as usize;
    let result_buf = alloc_result_buffer(size);

    let fn_addr = fn_ptr as usize;
    let env_addr = env_ptr as usize;
    let result_addr = result_buf as usize;
    let is_float_flag = is_float != 0;

    // TAG [SPARK-149]
    let handle = tokio_runtime().spawn_blocking(move || {
        let env = env_addr as *mut u8;
        let result_dst = result_addr as *mut u8;

        if is_float_flag {
            let f: unsafe extern "C" fn(*mut u8) -> f64 = unsafe { std::mem::transmute(fn_addr) };
            let val = unsafe { f(env) };
            if !result_dst.is_null() {
                unsafe { *(result_dst.cast::<f64>()) = val };
            }
        } else {
            let f: unsafe extern "C" fn(*mut u8) -> i64 = unsafe { std::mem::transmute(fn_addr) };
            let val = unsafe { f(env) };
            if !result_dst.is_null() {
                unsafe { *(result_dst.cast::<i64>()) = val };
            }
        }

        unsafe { __spark_env_rc_dec(env) };
    });

    new_future(result_buf, size, result_drop, handle).cast::<u8>()
}

#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::missing_panics_doc)]
/// Waits for a Spark future and returns its result buffer.
///
/// # Safety
///
/// `future_ptr` must be null or a valid pointer returned by a Spark spawn function.
pub unsafe extern "C" fn __spark_await(future_ptr: *mut u8) -> *mut u8 {
    if future_ptr.is_null() {
        return std::ptr::null_mut();
    }
    let future = unsafe { &*(future_ptr.cast::<SparkFuture>()) };
    future.completion.wait(true);
    future.result
}

#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
/// Decrements a Spark future reference count.
///
/// # Safety
///
/// `ptr` must be null or a valid Spark future pointer.
pub unsafe extern "C" fn __spark_future_rc_dec(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    let future = unsafe { &*(ptr.cast::<SparkFuture>()) };
    if future.rc.release() {
        if future.completion.wait(false) {
            if let Some(drop_result) = future.result_drop {
                unsafe { drop_result(future.result) };
            }
        }
        dealloc_result_buffer(future.result, future.result_size);
        drop(unsafe { Box::from_raw(ptr.cast::<SparkFuture>()) });
    }
}

thread_local! {
    static CURRENT_WAKER: Cell<Option<Waker>> = const { Cell::new(None) };
}

struct SparkTask {
    poll_fn: usize,
    state_ptr: *mut u8,
    state_size: usize,
    result_size: usize,
    result_offset: usize,
    result_ptr: *mut u8,
}

// Safety: each SparkTask owns its state allocation and poll function while scheduled.
unsafe impl Send for SparkTask {}

impl Future for SparkTask {
    type Output = ();

    #[allow(clippy::cast_ptr_alignment)]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        CURRENT_WAKER.set(Some(cx.waker().clone()));

        let poll_fn: unsafe extern "C" fn(*mut u8) -> i8 =
            unsafe { std::mem::transmute(self.poll_fn) };
        let status = unsafe { poll_fn(self.state_ptr) };

        CURRENT_WAKER.set(None);

        if status == 1 {
            if self.result_size > 0 && !self.result_ptr.is_null() {
                let src = unsafe { self.state_ptr.add(self.result_offset) };
                unsafe { std::ptr::copy_nonoverlapping(src, self.result_ptr, self.result_size) };
            }
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for SparkTask {
    fn drop(&mut self) {
        dealloc_result_buffer(self.state_ptr, self.state_size);
    }
}

#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub extern "C" fn __spark_alloc_state(size: i64) -> *mut u8 {
    alloc_result_buffer(size as usize)
}

#[no_mangle]
#[allow(
    clippy::cast_ptr_alignment,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
/// Spawns a cooperative Spark task.
///
/// # Safety
///
/// `poll_fn` and `state_ptr` must follow the cooperative async ABI.
/// On Ready, the initialized result moves from the state into the cache.
/// `result_drop`, when present, releases that cached value from its slot.
pub unsafe extern "C" fn __spark_spawn_coop(
    poll_fn: *mut u8,
    state_ptr: *mut u8,
    state_size: i64,
    result_size: i64,
    result_offset: i64,
    result_drop: Option<unsafe extern "C" fn(*mut u8)>,
) -> *mut u8 {
    let result_size = result_size as usize;
    let result_buf = alloc_result_buffer(result_size);
    let task = SparkTask {
        poll_fn: poll_fn as usize,
        state_ptr,
        state_size: state_size as usize,
        result_size,
        result_offset: result_offset as usize,
        result_ptr: result_buf,
    };

    let handle = tokio_runtime().spawn(task);
    new_future(result_buf, result_size, result_drop, handle).cast::<u8>()
}

#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::missing_panics_doc)]
/// Tries to poll a Spark future without blocking.
///
/// Null means Pending. Ready returns the result buffer, or an aligned non-null
/// zero-length buffer marker when the result has no bytes. The marker must not
/// be read or freed.
///
/// # Safety
///
/// `future_ptr` must be null or a valid Spark future pointer.
pub unsafe extern "C" fn __spark_try_poll(future_ptr: *mut u8) -> *mut u8 {
    if future_ptr.is_null() {
        return std::ptr::null_mut();
    }

    let future = unsafe { &*(future_ptr.cast::<SparkFuture>()) };
    let waker = CURRENT_WAKER.take();
    CURRENT_WAKER.set(waker.clone());
    if future.completion.poll(waker.as_ref()) {
        if future.result_size == 0 {
            std::ptr::NonNull::<u64>::dangling().as_ptr().cast()
        } else {
            future.result
        }
    } else {
        std::ptr::null_mut()
    }
}

#[no_mangle]
pub extern "C" fn __spark_wake_current() {
    CURRENT_WAKER.with(|w| {
        if let Some(waker) = w.take() {
            waker.wake_by_ref();
            w.set(Some(waker));
        }
    });
}

fn new_future(
    result: *mut u8,
    result_size: usize,
    result_drop: Option<unsafe extern "C" fn(*mut u8)>,
    handle: tokio::task::JoinHandle<()>,
) -> *mut SparkFuture {
    Box::into_raw(Box::new(SparkFuture {
        rc: RefCount::new(),
        result,
        result_size,
        result_drop,
        completion: Completion::new(handle),
    }))
}

fn alloc_result_buffer(size: usize) -> *mut u8 {
    if size == 0 {
        return std::ptr::null_mut();
    }
    let align = std::mem::align_of::<u64>();
    let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(size, align) };
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    if ptr.is_null() {
        eprintln!("spark: out of memory allocating future result buffer");
        std::process::abort();
    }
    ptr
}

fn dealloc_result_buffer(ptr: *mut u8, size: usize) {
    if !ptr.is_null() && size > 0 {
        let align = std::mem::align_of::<u64>();
        let layout = unsafe { std::alloc::Layout::from_size_align_unchecked(size, align) };
        unsafe { std::alloc::dealloc(ptr, layout) };
    }
}

// TAG [SPARK-149]
#[cfg(test)]
mod tests {
    use super::*;
    use spark_abi::{ENV_CAPTURES_OFFSET, ENV_DTOR_OFFSET};
    use std::sync::atomic::{AtomicUsize, Ordering};

    unsafe extern "C" fn count_environment_drop(env: *mut u8) {
        let counter = unsafe {
            *env.add(ENV_CAPTURES_OFFSET as usize)
                .cast::<*const AtomicUsize>()
        };
        unsafe { &*counter }.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C" fn seven(_env: *mut u8) -> i64 {
        7
    }

    #[test]
    fn blocking_spawn_drops_its_owned_environment_once() {
        for await_first in [false, true] {
            let drops = AtomicUsize::new(0);
            let env = crate::__spark_env_alloc(std::mem::size_of::<usize>() as i64);
            unsafe {
                *env.add(ENV_DTOR_OFFSET as usize).cast::<usize>() =
                    count_environment_drop as *const () as usize;
                *env.add(ENV_CAPTURES_OFFSET as usize)
                    .cast::<*const AtomicUsize>() = std::ptr::from_ref(&drops);
                let pending = __spark_spawn(seven as *const () as *mut u8, env, 8, 0, None);
                if await_first {
                    let result = __spark_await(pending);
                    assert_eq!(*result.cast::<i64>(), 7);
                    assert_eq!(__spark_await(pending), result);
                    assert_eq!(drops.load(Ordering::SeqCst), 1);
                }
                __spark_future_rc_dec(pending);
            }
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
    }
    #[test]
    fn concurrent_owned_aliases_await_the_same_result() {
        for _ in 0..16 {
            let pending = unsafe {
                __spark_spawn(
                    seven as *const () as *mut u8,
                    std::ptr::null_mut(),
                    8,
                    0,
                    None,
                )
            };
            for _ in 1..4 {
                unsafe { crate::__spark_rc_inc(pending) };
            }
            let start = std::sync::Barrier::new(4);
            std::thread::scope(|scope| {
                for _ in 0..4 {
                    let address = pending as usize;
                    let start = &start;
                    scope.spawn(move || {
                        start.wait();
                        let pending = address as *mut u8;
                        unsafe {
                            let result = __spark_await(pending);
                            assert_eq!(*result.cast::<i64>(), 7);
                            __spark_future_rc_dec(pending);
                        }
                    });
                }
            });
        }
    }
    // TAG [SPARK-117]
    #[test]
    fn completed_zero_sized_results_are_distinct_from_pending() {
        let (release, held) = tokio::sync::oneshot::channel();
        let task = tokio_runtime().spawn(async move {
            held.await.expect("release empty task");
        });
        let future = new_future(std::ptr::null_mut(), 0, None, task).cast();
        let was_pending = unsafe { __spark_try_poll(future).is_null() };
        release.send(()).expect("release empty task");
        unsafe {
            __spark_await(future);
        }
        let ready = unsafe { __spark_try_poll(future) };
        unsafe {
            __spark_future_rc_dec(future);
        }
        assert!(was_pending);
        assert!(
            !ready.is_null(),
            "a completed empty result must signal Ready"
        );
    }
    unsafe extern "C" fn return_counter(env: *mut u8) -> i64 {
        unsafe { *env.add(ENV_CAPTURES_OFFSET as usize).cast::<usize>() as i64 }
    }

    unsafe extern "C" fn count_result_drop(slot: *mut u8) {
        let counter = unsafe { *slot.cast::<*const AtomicUsize>() };
        unsafe { &*counter }.fetch_add(1, Ordering::SeqCst);
    }

    fn counter_future(counter: &AtomicUsize) -> *mut u8 {
        let env = crate::__spark_env_alloc(std::mem::size_of::<usize>() as i64);
        unsafe {
            *env.add(ENV_CAPTURES_OFFSET as usize)
                .cast::<*const AtomicUsize>() = counter;
            __spark_spawn(
                return_counter as *const () as *mut u8,
                env,
                8,
                0,
                Some(count_result_drop),
            )
        }
    }

    #[test]
    fn cached_result_drops_only_on_the_last_future_release() {
        for await_first in [false, true] {
            let drops = AtomicUsize::new(0);
            let pending = counter_future(&drops);
            unsafe { crate::__spark_rc_inc(pending) };
            if await_first {
                unsafe {
                    assert_eq!(__spark_await(pending), __spark_await(pending));
                }
            }
            unsafe { __spark_future_rc_dec(pending) };
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            unsafe { __spark_future_rc_dec(pending) };
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn concurrent_future_aliases_drop_the_cached_result_once() {
        let drops = AtomicUsize::new(0);
        let pending = counter_future(&drops);
        for _ in 1..4 {
            unsafe { crate::__spark_rc_inc(pending) };
        }
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let address = pending as usize;
                scope.spawn(move || unsafe {
                    let future = address as *mut u8;
                    __spark_await(future);
                    __spark_future_rc_dec(future);
                });
            }
        });
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_completion_does_not_destroy_an_uninitialized_result() {
        let drops = AtomicUsize::new(0);
        let result = alloc_result_buffer(std::mem::size_of::<usize>());
        // Keep a valid counter in the bytes so an erroneous callback is a safe failure.
        unsafe { *result.cast::<*const AtomicUsize>() = &drops };
        let task = tokio_runtime().spawn(async { panic!("expected task failure") });
        let pending = new_future(
            result,
            std::mem::size_of::<usize>(),
            Some(count_result_drop),
            task,
        );
        unsafe { __spark_future_rc_dec(pending.cast()) };
        assert_eq!(drops.load(Ordering::SeqCst), 0);
    }
}
