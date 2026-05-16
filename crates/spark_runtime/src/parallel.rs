use rayon::prelude::*;

use crate::callback::Callback;
use crate::value::value_desc_new;
use crate::{SparkList, __spark_list_new, __spark_list_push};

#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
/// Runs a callback for each list element in parallel.
///
/// # Safety
///
/// `list_ptr` must be a valid `SparkList`, and `fn_ptr` must match `cb_kind`.
pub unsafe extern "C" fn __spark_parallel_foreach(
    list_ptr: *mut u8,
    fn_ptr: *mut u8,
    env_ptr: *mut u8,
    _elem_size: i64,
    cb_kind: u8,
) {
    let list = unsafe { &*(list_ptr.cast::<SparkList>()) };
    if list.len == 0 {
        return;
    }

    let callback = Callback::new(fn_ptr, env_ptr, cb_kind);
    let elem_size = list.elem_size();
    let data_addr = list.data as usize;

    (0..list.len).into_par_iter().for_each(|i| unsafe {
        let elem_ptr = (data_addr as *const u8).add(i * elem_size);
        callback.call_foreach_elem(elem_ptr);
    });
}

#[no_mangle]
#[allow(
    clippy::cast_ptr_alignment,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
/// Maps a list in parallel and returns a new list.
///
/// # Safety
///
/// `list_ptr` must be a valid `SparkList`, and `fn_ptr` must match `cb_kind`.
pub unsafe extern "C" fn __spark_parallel_map(
    list_ptr: *mut u8,
    fn_ptr: *mut u8,
    env_ptr: *mut u8,
    _elem_size: i64,
    result_elem_size: i64,
    cb_kind: u8,
) -> *mut u8 {
    let list = unsafe { &*(list_ptr.cast::<SparkList>()) };
    let result_list = new_scalar_list(result_elem_size);

    if list.len == 0 {
        return result_list;
    }

    let callback = Callback::new(fn_ptr, env_ptr, cb_kind);
    let elem_size = list.elem_size();
    let data_addr = list.data as usize;

    let results: Vec<u64> = (0..list.len)
        .into_par_iter()
        .map(|i| unsafe {
            let elem_ptr = (data_addr as *const u8).add(i * elem_size);
            callback.call_map_elem(elem_ptr)
        })
        .collect();

    push_scalar_results(result_list, &results, false);
    result_list
}

#[no_mangle]
#[allow(
    clippy::cast_ptr_alignment,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
/// Runs a fused parallel map/reduce over a list.
///
/// # Safety
///
/// Function pointers and environment pointers must match their callback kinds.
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
    let list = unsafe { &*(list_ptr.cast::<SparkList>()) };
    if list.len == 0 {
        return initial;
    }

    let mapper = Callback::new(map_fn, map_env, map_cb_kind);
    let reducer = Callback::new(reduce_fn, reduce_env, reduce_cb_kind);
    let elem_size = list.elem_size();
    let data_addr = list.data as usize;

    let result = (0..list.len)
        .into_par_iter()
        .map(|i| unsafe {
            let elem_ptr = (data_addr as *const u8).add(i * elem_size);
            mapper.call_map_elem(elem_ptr)
        })
        .reduce(
            || initial as u64,
            |acc, val| unsafe { reducer.call_reduce(acc, val) },
        );

    result as i64
}

#[no_mangle]
#[allow(clippy::cast_possible_wrap)]
pub extern "C" fn __spark_range_len(start: i64, end: i64, inclusive: i8, step: i64) -> i64 {
    if step == 0 {
        return 0;
    }

    let diff = if inclusive != 0 {
        if step > 0 {
            end - start + 1
        } else {
            start - end + 1
        }
    } else if step > 0 {
        end - start
    } else {
        start - end
    };

    if diff <= 0 {
        return 0;
    }

    let abs_step = step.unsigned_abs() as i64;
    (diff + abs_step - 1) / abs_step
}

#[no_mangle]
/// Runs a callback for each range element in parallel.
///
/// # Safety
///
/// `fn_ptr` must match `cb_kind`; `env_ptr` must be null or a valid environment.
pub unsafe extern "C" fn __spark_parallel_range_foreach(
    start: i64,
    end: i64,
    inclusive: i8,
    step: i64,
    fn_ptr: *mut u8,
    env_ptr: *mut u8,
    cb_kind: u8,
) {
    let len = __spark_range_len(start, end, inclusive, step);
    if len <= 0 {
        return;
    }

    let callback = Callback::new(fn_ptr, env_ptr, cb_kind);
    (0..len).into_par_iter().for_each(|i| unsafe {
        callback.call_foreach_int(start + i * step);
    });
}

#[no_mangle]
#[allow(
    clippy::cast_ptr_alignment,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
/// Maps a range in parallel and returns a new list.
///
/// # Safety
///
/// `fn_ptr` must match `cb_kind`; `env_ptr` must be null or a valid environment.
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
    let list = new_scalar_list(result_elem_size);
    let len = __spark_range_len(start, end, inclusive, step);
    if len <= 0 {
        return list;
    }

    let callback = Callback::new(fn_ptr, env_ptr, cb_kind);
    let results: Vec<u64> = (0..len)
        .into_par_iter()
        .map(|i| unsafe { callback.call_map_int(start + i * step) })
        .collect();

    push_scalar_results(list, &results, callback.output_float());
    list
}

#[no_mangle]
#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
/// Runs a fused parallel map/reduce over a range.
///
/// # Safety
///
/// Function pointers and environment pointers must match their callback kinds.
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
    let len = __spark_range_len(start, end, inclusive, step);
    if len <= 0 {
        return initial;
    }

    let mapper = Callback::new(map_fn, map_env, map_cb_kind);
    let reducer = Callback::new(reduce_fn, reduce_env, reduce_cb_kind);
    let result = (0..len)
        .into_par_iter()
        .map(|i| unsafe { mapper.call_map_int(start + i * step) })
        .reduce(
            || initial as u64,
            |acc, val| unsafe { reducer.call_reduce(acc, val) },
        );

    result as i64
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn new_scalar_list(elem_size: i64) -> *mut u8 {
    let desc = value_desc_new(elem_size as usize, std::mem::align_of::<u64>(), 0, 0, 0, 0);
    __spark_list_new(desc.cast())
}

fn push_scalar_results(list: *mut u8, results: &[u64], output_float: bool) {
    for result in results {
        let bytes = if output_float {
            f64::from_bits(*result).to_ne_bytes()
        } else {
            result.cast_signed().to_ne_bytes()
        };
        unsafe { __spark_list_push(list, bytes.as_ptr()) };
    }
}
