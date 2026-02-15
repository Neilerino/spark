//! `Channel[T]` runtime implementation.
//!
//! Provides `extern "C"` functions for Spark's built-in `Channel[T]` type.
//! Channels are type-erased MPSC (multi-producer, single-consumer) channels
//! backed by `std::sync::mpsc`. Elements are copied as raw bytes.

use std::sync::mpsc;
use std::sync::Mutex;

/// Type alias for the sender half, wrapped in Option for close().
type SenderMutex = Mutex<Option<mpsc::Sender<Vec<u8>>>>;

/// Type alias for the receiver half.
type ReceiverMutex = Mutex<mpsc::Receiver<Vec<u8>>>;

/// A heap-allocated MPSC channel with reference counting.
///
/// The `rc` field is at offset 0 (like all Spark heap types) so generic
/// `__spark_rc_inc` works.
#[repr(C)]
struct SparkChannel {
    /// Reference count.
    rc: usize,
    /// `Box<SenderMutex>` — wrapped in Option for close().
    sender: *mut std::ffi::c_void,
    /// `Box<ReceiverMutex>`.
    receiver: *mut std::ffi::c_void,
    /// Size of each element in bytes.
    elem_size: usize,
}

/// Creates a new MPSC channel.
///
/// Returns a `SparkChannel*` with `rc = 1`. The channel is unbounded.
///
/// # Parameters
///
/// - `elem_size`: byte size of each element (used for validation, stored in struct).
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub extern "C" fn __spark_channel_new(elem_size: i64) -> *mut u8 {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();

    let sender = Box::into_raw(Box::new(Mutex::new(Some(tx)))).cast::<std::ffi::c_void>();
    let receiver = Box::into_raw(Box::new(Mutex::new(rx))).cast::<std::ffi::c_void>();

    let ch = Box::new(SparkChannel {
        rc: 1,
        sender,
        receiver,
        elem_size: elem_size as usize,
    });
    Box::into_raw(ch).cast::<u8>()
}

/// Sends a value through the channel.
///
/// Copies `elem_size` bytes from `value_ptr` and sends them. If the channel
/// is closed (sender dropped), the send is silently dropped.
///
/// # Safety
///
/// - `ch` must be a valid `SparkChannel*` from `__spark_channel_new`.
/// - `value_ptr` must point to at least `elem_size` bytes of valid data.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_channel_send(
    ch: *mut u8,
    value_ptr: *const u8,
    elem_size: i64,
) {
    let channel = unsafe { &*(ch.cast::<SparkChannel>()) };
    let size = elem_size as usize;

    // Copy the value bytes.
    let mut data = vec![0u8; size];
    if size > 0 && !value_ptr.is_null() {
        unsafe { std::ptr::copy_nonoverlapping(value_ptr, data.as_mut_ptr(), size) };
    }

    // Send through the channel.
    let sender_mutex = unsafe { &*(channel.sender.cast::<SenderMutex>()) };
    let guard = sender_mutex.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ref tx) = *guard {
        let _ = tx.send(data); // Silently ignore if receiver is dropped.
    }
}

/// Receives a value from the channel (blocking).
///
/// Blocks until a value is available or the channel is closed (all senders
/// dropped). Returns a pointer to the received data, or null if the channel
/// is closed and empty.
///
/// The returned pointer is heap-allocated and valid until the caller is done
/// reading from it. The runtime manages the allocation.
///
/// # Safety
///
/// - `ch` must be a valid `SparkChannel*`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_channel_recv(ch: *mut u8, _elem_size: i64) -> *mut u8 {
    let channel = unsafe { &*(ch.cast::<SparkChannel>()) };

    let receiver_mutex = unsafe { &*(channel.receiver.cast::<ReceiverMutex>()) };
    let guard = receiver_mutex.lock().unwrap_or_else(|e| e.into_inner());

    match guard.recv() {
        Ok(data) => {
            // Leak the Vec's buffer so the pointer remains valid for the caller.
            let mut data = data.into_boxed_slice();
            let ptr = data.as_mut_ptr();
            std::mem::forget(data);
            ptr
        }
        Err(_) => {
            // Channel closed — return null.
            std::ptr::null_mut()
        }
    }
}

/// Attempts to receive a value without blocking.
///
/// Writes an Optional struct directly to `dest_ptr`:
/// - tag byte at offset 0: 0 = empty, 1 = has value
/// - payload at `payload_offset`: the received value bytes
///
/// # Safety
///
/// - `ch` must be a valid `SparkChannel*`.
/// - `dest_ptr` must point to a buffer large enough for the Optional struct.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_channel_try_recv(
    ch: *mut u8,
    elem_size: i64,
    dest_ptr: *mut u8,
    payload_offset: i64,
) {
    let channel = unsafe { &*(ch.cast::<SparkChannel>()) };
    let size = elem_size as usize;
    let offset = payload_offset as usize;

    let receiver_mutex = unsafe { &*(channel.receiver.cast::<ReceiverMutex>()) };
    let guard = receiver_mutex.lock().unwrap_or_else(|e| e.into_inner());

    match guard.try_recv() {
        Ok(data) => {
            // tag = 1 (has value)
            unsafe { *dest_ptr = 1 };
            // Copy payload
            if size > 0 && data.len() >= size {
                unsafe {
                    std::ptr::copy_nonoverlapping(data.as_ptr(), dest_ptr.add(offset), size);
                }
            }
        }
        Err(_) => {
            // tag = 0 (empty)
            unsafe { *dest_ptr = 0 };
        }
    }
}

/// Closes the channel by dropping the sender.
///
/// After closing, any pending `recv()` calls will return remaining buffered
/// values and then receive null (closed signal). Further `send()` calls are
/// silently ignored.
///
/// # Safety
///
/// - `ch` must be a valid `SparkChannel*`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_channel_close(ch: *mut u8) {
    let channel = unsafe { &*(ch.cast::<SparkChannel>()) };

    let sender_mutex = unsafe { &*(channel.sender.cast::<SenderMutex>()) };
    let mut guard = sender_mutex.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None; // Drop the sender.
}

/// Decrements the reference count of a `SparkChannel`.
///
/// When the count reaches zero, drops the sender and receiver and frees
/// all allocations.
///
/// # Safety
///
/// - `ch` must be null or a valid `SparkChannel*`.
#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn __spark_channel_rc_dec(ch: *mut u8) {
    if ch.is_null() {
        return;
    }
    let channel = unsafe { &mut *(ch.cast::<SparkChannel>()) };
    channel.rc -= 1;
    if channel.rc == 0 {
        // Drop the sender.
        if !channel.sender.is_null() {
            drop(unsafe { Box::from_raw(channel.sender.cast::<SenderMutex>()) });
        }
        // Drop the receiver.
        if !channel.receiver.is_null() {
            drop(unsafe { Box::from_raw(channel.receiver.cast::<ReceiverMutex>()) });
        }
        // Free the SparkChannel struct.
        drop(unsafe { Box::from_raw(ch.cast::<SparkChannel>()) });
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_drop_channel() {
        let ch = __spark_channel_new(8);
        assert!(!ch.is_null());
        unsafe { __spark_channel_rc_dec(ch) };
    }

    #[test]
    fn send_and_recv_int() {
        let ch = __spark_channel_new(8);
        let val: i64 = 42;
        unsafe {
            __spark_channel_send(ch, (&val as *const i64).cast(), 8);
        }
        let result_ptr = unsafe { __spark_channel_recv(ch, 8) };
        assert!(!result_ptr.is_null());
        let received = unsafe { *(result_ptr.cast::<i64>()) };
        assert_eq!(received, 42);
        unsafe { __spark_channel_rc_dec(ch) };
    }

    #[test]
    fn try_recv_empty() {
        let ch = __spark_channel_new(8);
        // Optional layout for i64: tag at offset 0, payload at offset 8 (aligned).
        let mut buf = [0u8; 16]; // tag(1) + padding(7) + i64(8)
        unsafe { __spark_channel_try_recv(ch, 8, buf.as_mut_ptr(), 8) };
        assert_eq!(buf[0], 0, "tag should be 0 (empty)");
        unsafe { __spark_channel_rc_dec(ch) };
    }

    #[test]
    fn try_recv_with_value() {
        let ch = __spark_channel_new(8);
        let val: i64 = 99;
        unsafe {
            __spark_channel_send(ch, (&val as *const i64).cast(), 8);
        }
        let mut buf = [0u8; 16]; // tag(1) + padding(7) + i64(8)
        unsafe { __spark_channel_try_recv(ch, 8, buf.as_mut_ptr(), 8) };
        assert_eq!(buf[0], 1, "tag should be 1 (has value)");
        let received = i64::from_ne_bytes(buf[8..16].try_into().unwrap());
        assert_eq!(received, 99);
        unsafe { __spark_channel_rc_dec(ch) };
    }

    #[test]
    fn close_channel() {
        let ch = __spark_channel_new(8);
        unsafe { __spark_channel_close(ch) };
        // recv after close returns null
        let result = unsafe { __spark_channel_recv(ch, 8) };
        assert!(result.is_null());
        unsafe { __spark_channel_rc_dec(ch) };
    }

    #[test]
    fn send_after_close_is_silent() {
        let ch = __spark_channel_new(8);
        unsafe { __spark_channel_close(ch) };
        let val: i64 = 123;
        // Should not panic
        unsafe { __spark_channel_send(ch, (&val as *const i64).cast(), 8) };
        unsafe { __spark_channel_rc_dec(ch) };
    }

    #[test]
    fn multiple_sends_and_recvs() {
        let ch = __spark_channel_new(8);
        for i in 0..5i64 {
            unsafe { __spark_channel_send(ch, (&i as *const i64).cast(), 8) };
        }
        for expected in 0..5i64 {
            let ptr = unsafe { __spark_channel_recv(ch, 8) };
            assert!(!ptr.is_null());
            let received = unsafe { *(ptr.cast::<i64>()) };
            assert_eq!(received, expected);
        }
        unsafe { __spark_channel_rc_dec(ch) };
    }

    #[test]
    fn cross_thread_send_recv() {
        let ch = __spark_channel_new(8);
        let ch_addr = ch as usize;

        let handle = std::thread::spawn(move || {
            let ch = ch_addr as *mut u8;
            let val: i64 = 777;
            unsafe { __spark_channel_send(ch, (&val as *const i64).cast(), 8) };
        });

        handle.join().unwrap();
        let ptr = unsafe { __spark_channel_recv(ch, 8) };
        assert!(!ptr.is_null());
        let received = unsafe { *(ptr.cast::<i64>()) };
        assert_eq!(received, 777);
        unsafe { __spark_channel_rc_dec(ch) };
    }

    #[test]
    fn rc_dec_null_is_noop() {
        unsafe { __spark_channel_rc_dec(std::ptr::null_mut()) };
    }
}
