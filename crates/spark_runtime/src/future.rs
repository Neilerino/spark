use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use crate::__spark_env_rc_dec;

#[repr(C)]
struct SparkFuture {
    rc: usize,
    result: *mut u8,
    result_size: usize,
    handle: *mut std::ffi::c_void,
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
pub unsafe extern "C" fn __spark_spawn(
    fn_ptr: *mut u8,
    env_ptr: *mut u8,
    result_size: i64,
    is_float: i8,
) -> *mut u8 {
    let size = result_size as usize;
    let result_buf = alloc_result_buffer(size);
    let future_ptr = new_future(result_buf, size);

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

    unsafe { (*future_ptr).handle = Box::into_raw(Box::new(handle)).cast() };
    future_ptr.cast::<u8>()
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
    let future = unsafe { &mut *(future_ptr.cast::<SparkFuture>()) };
    join_future_if_needed(future, true);
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
    let future = unsafe { &mut *(ptr.cast::<SparkFuture>()) };
    future.rc -= 1;
    if future.rc == 0 {
        join_future_if_needed(future, false);
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
    future_ptr: *mut u8,
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
            if !self.future_ptr.is_null() && self.result_size > 0 {
                let future = unsafe { &mut *(self.future_ptr.cast::<SparkFuture>()) };
                if !future.result.is_null() {
                    let src = unsafe { self.state_ptr.add(self.result_offset) };
                    unsafe { std::ptr::copy_nonoverlapping(src, future.result, self.result_size) };
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
pub unsafe extern "C" fn __spark_spawn_coop(
    poll_fn: *mut u8,
    state_ptr: *mut u8,
    state_size: i64,
    result_size: i64,
    result_offset: i64,
) -> *mut u8 {
    let result_size = result_size as usize;
    let future_ptr = new_future(alloc_result_buffer(result_size), result_size);
    let task = SparkTask {
        poll_fn: poll_fn as usize,
        state_ptr,
        state_size: state_size as usize,
        result_size,
        result_offset: result_offset as usize,
        future_ptr: future_ptr.cast(),
    };

    let handle = tokio_runtime().spawn(task);
    unsafe { (*future_ptr).handle = Box::into_raw(Box::new(handle)).cast() };
    future_ptr.cast::<u8>()
}

#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::missing_panics_doc)]
/// Tries to poll a Spark future without blocking.
///
/// # Safety
///
/// `future_ptr` must be null or a valid Spark future pointer.
pub unsafe extern "C" fn __spark_try_poll(future_ptr: *mut u8) -> *mut u8 {
    if future_ptr.is_null() {
        return std::ptr::null_mut();
    }

    let future = unsafe { &*(future_ptr.cast::<SparkFuture>()) };
    if future.handle.is_null() {
        return future.result;
    }

    let handle = unsafe { &*future.handle.cast::<tokio::task::JoinHandle<()>>() };
    if !handle.is_finished() {
        return std::ptr::null_mut();
    }

    let future = unsafe { &mut *(future_ptr.cast::<SparkFuture>()) };
    join_future_if_needed(future, true);
    future.result
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

fn new_future(result: *mut u8, result_size: usize) -> *mut SparkFuture {
    Box::into_raw(Box::new(SparkFuture {
        rc: 1,
        result,
        result_size,
        handle: std::ptr::null_mut(),
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

fn join_future_if_needed(future: &mut SparkFuture, propagate_panic: bool) {
    if future.handle.is_null() {
        return;
    }

    let handle: Box<tokio::task::JoinHandle<()>> = unsafe { Box::from_raw(future.handle.cast()) };
    future.handle = std::ptr::null_mut();

    if propagate_panic {
        tokio_runtime().block_on(async { handle.await.expect("spark: spawned task panicked") });
    } else {
        tokio_runtime().block_on(async {
            let _ = handle.await;
        });
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
                let pending = __spark_spawn(seven as *const () as *mut u8, env, 8, 0);
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
}
