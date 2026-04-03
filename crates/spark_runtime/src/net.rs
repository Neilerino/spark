//! `spark.net` runtime implementations.
//!
//! Provides `extern "C"` functions for the `spark.net` standard library module.
//! Wraps `tokio::net` operations (TCP, UDP, DNS) with synchronous FFI functions
//! that use `tokio_runtime().block_on()` internally.
//!
//! ## Handle Table
//!
//! Networking resources (listeners, streams, sockets) are stored in a global
//! handle table keyed by `i64` handle IDs. Each resource is wrapped in `Arc`
//! for safe sharing with tokio tasks.
//!
//! ## Error Model
//!
//! Functions that can fail store the error message in a thread-local string
//! and return an integer error code:
//! - 0 = success
//! - negative handle = error (for functions returning handles)

#![allow(clippy::similar_names)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use crate::{__spark_string_from_literal, tokio_runtime, SparkString};

// =============================================================================
// Handle table
// =============================================================================

/// A networking resource stored in the handle table.
enum NetResource {
    /// An async TCP listener.
    TcpListener(Arc<tokio::net::TcpListener>),
    /// An async TCP stream (needs Mutex for read/write which take &mut).
    TcpStream(Arc<tokio::sync::Mutex<tokio::net::TcpStream>>),
    /// An async UDP socket.
    UdpSocket(Arc<tokio::net::UdpSocket>),
}

/// Global handle table for networking resources.
static HANDLE_TABLE: std::sync::LazyLock<Mutex<HashMap<i64, NetResource>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Next handle ID (starts at 1; 0 is reserved, negative = error).
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

/// Allocates a new handle ID and inserts the resource.
fn insert_resource(resource: NetResource) -> i64 {
    let id = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let mut table = HANDLE_TABLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    table.insert(id, resource);
    id
}

/// Removes a resource from the handle table by ID.
fn remove_resource(id: i64) -> Option<NetResource> {
    let mut table = HANDLE_TABLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    table.remove(&id)
}

/// Gets a clone of the `TcpListener` Arc for the given handle.
fn get_tcp_listener(id: i64) -> Option<Arc<tokio::net::TcpListener>> {
    let table = HANDLE_TABLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match table.get(&id) {
        Some(NetResource::TcpListener(l)) => Some(Arc::clone(l)),
        _ => None,
    }
}

/// Gets a clone of the `TcpStream` Arc for the given handle.
fn get_tcp_stream(id: i64) -> Option<Arc<tokio::sync::Mutex<tokio::net::TcpStream>>> {
    let table = HANDLE_TABLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match table.get(&id) {
        Some(NetResource::TcpStream(s)) => Some(Arc::clone(s)),
        _ => None,
    }
}

/// Gets a clone of the `UdpSocket` Arc for the given handle.
fn get_udp_socket(id: i64) -> Option<Arc<tokio::net::UdpSocket>> {
    let table = HANDLE_TABLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match table.get(&id) {
        Some(NetResource::UdpSocket(s)) => Some(Arc::clone(s)),
        _ => None,
    }
}

// =============================================================================
// Error handling
// =============================================================================

thread_local! {
    static LAST_NET_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Sets the last network error message.
fn set_error(msg: String) {
    LAST_NET_ERROR.with(|e| {
        *e.borrow_mut() = msg;
    });
}

/// Clears the last network error message.
fn clear_error() {
    LAST_NET_ERROR.with(|e| {
        e.borrow_mut().clear();
    });
}

// =============================================================================
// String helpers
// =============================================================================

/// Reads a `SparkString` pointer, returning the content as a Rust `String`.
///
/// # Safety
///
/// `ptr` must be a valid pointer to a `SparkString`.
#[allow(clippy::cast_ptr_alignment)]
unsafe fn read_spark_str(ptr: *mut u8) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let s = unsafe { &*(ptr.cast::<SparkString>()) };
    if s.len == 0 {
        return String::new();
    }
    let slice = unsafe { std::slice::from_raw_parts(s.data, s.len) };
    std::str::from_utf8(slice)
        .unwrap_or("<invalid utf-8>")
        .to_owned()
}

/// Creates a new `SparkString` from a Rust `&str`.
#[allow(clippy::cast_possible_wrap)]
fn rust_str_to_spark(s: &str) -> *mut u8 {
    let bytes = s.as_bytes();
    unsafe { __spark_string_from_literal(bytes.as_ptr(), bytes.len() as i64) }
}

// =============================================================================
// TCP — Listener
// =============================================================================

/// Binds a TCP listener on `addr:port`.
///
/// Returns a handle ID (> 0) on success, or -1 on error.
///
/// # Safety
///
/// `addr` must be a valid `SparkString` pointer.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_net_tcp_listen(addr: *mut u8, port: i64) -> i64 {
    let addr_str = unsafe { read_spark_str(addr) };
    let bind_addr = format!("{addr_str}:{port}");
    let rt = tokio_runtime();
    match rt.block_on(tokio::net::TcpListener::bind(&bind_addr)) {
        Ok(listener) => {
            clear_error();
            insert_resource(NetResource::TcpListener(Arc::new(listener)))
        }
        Err(e) => {
            set_error(format!("{e}"));
            -1
        }
    }
}

/// Accepts a connection on a TCP listener.
///
/// Returns a stream handle ID (> 0) on success, or -1 on error.
#[no_mangle]
pub extern "C" fn __spark_net_tcp_accept(listener_handle: i64) -> i64 {
    let Some(listener) = get_tcp_listener(listener_handle) else {
        set_error("invalid listener handle".to_string());
        return -1;
    };
    let rt = tokio_runtime();
    match rt.block_on(listener.accept()) {
        Ok((stream, _addr)) => {
            clear_error();
            insert_resource(NetResource::TcpStream(Arc::new(tokio::sync::Mutex::new(
                stream,
            ))))
        }
        Err(e) => {
            set_error(format!("{e}"));
            -1
        }
    }
}

/// Returns the local address of a TCP listener as a `SparkString`.
///
/// Returns `"<error>"` if the handle is invalid.
#[no_mangle]
pub extern "C" fn __spark_net_listener_addr(listener_handle: i64) -> *mut u8 {
    let Some(listener) = get_tcp_listener(listener_handle) else {
        return rust_str_to_spark("<error>");
    };
    match listener.local_addr() {
        Ok(addr) => rust_str_to_spark(&addr.to_string()),
        Err(e) => {
            set_error(format!("{e}"));
            rust_str_to_spark("<error>")
        }
    }
}

// =============================================================================
// TCP — Stream
// =============================================================================

/// Connects to a remote TCP address.
///
/// Returns a stream handle ID (> 0) on success, or -1 on error.
///
/// # Safety
///
/// `addr` must be a valid `SparkString` pointer.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_net_tcp_connect(addr: *mut u8, port: i64) -> i64 {
    let addr_str = unsafe { read_spark_str(addr) };
    let connect_addr = format!("{addr_str}:{port}");
    let rt = tokio_runtime();
    match rt.block_on(tokio::net::TcpStream::connect(&connect_addr)) {
        Ok(stream) => {
            clear_error();
            insert_resource(NetResource::TcpStream(Arc::new(tokio::sync::Mutex::new(
                stream,
            ))))
        }
        Err(e) => {
            set_error(format!("{e}"));
            -1
        }
    }
}

/// Reads up to `max_bytes` from a TCP stream.
///
/// Returns data as a `SparkString`. Returns empty string on error.
///
/// # Safety
///
/// Returned pointer is a valid `SparkString`.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub extern "C" fn __spark_net_tcp_read(stream_handle: i64, max_bytes: i64) -> *mut u8 {
    let Some(stream) = get_tcp_stream(stream_handle) else {
        set_error("invalid stream handle".to_string());
        return rust_str_to_spark("");
    };
    let max = max_bytes.max(0) as usize;
    let rt = tokio_runtime();
    match rt.block_on(async {
        use tokio::io::AsyncReadExt;
        let mut guard = stream.lock().await;
        let mut buf = vec![0u8; max];
        guard.read(&mut buf).await.map(|n| {
            buf.truncate(n);
            buf
        })
    }) {
        Ok(data) => {
            clear_error();
            let text = String::from_utf8_lossy(&data);
            rust_str_to_spark(&text)
        }
        Err(e) => {
            set_error(format!("{e}"));
            rust_str_to_spark("")
        }
    }
}

/// Writes data to a TCP stream.
///
/// Returns 0 on success, or a non-zero error code.
///
/// # Safety
///
/// `data` must be a valid `SparkString` pointer.
#[no_mangle]
pub unsafe extern "C" fn __spark_net_tcp_write(stream_handle: i64, data: *mut u8) -> i64 {
    let Some(stream) = get_tcp_stream(stream_handle) else {
        set_error("invalid stream handle".to_string());
        return 1;
    };
    let data_str = unsafe { read_spark_str(data) };
    let rt = tokio_runtime();
    match rt.block_on(async {
        use tokio::io::AsyncWriteExt;
        let mut guard = stream.lock().await;
        guard.write_all(data_str.as_bytes()).await
    }) {
        Ok(()) => {
            clear_error();
            0
        }
        Err(e) => {
            set_error(format!("{e}"));
            1
        }
    }
}

/// Returns the peer address of a TCP stream as a `SparkString`.
#[no_mangle]
pub extern "C" fn __spark_net_tcp_peer_addr(stream_handle: i64) -> *mut u8 {
    let Some(stream) = get_tcp_stream(stream_handle) else {
        return rust_str_to_spark("<error>");
    };
    let rt = tokio_runtime();
    let result = rt.block_on(async {
        let guard = stream.lock().await;
        guard.peer_addr()
    });
    match result {
        Ok(addr) => rust_str_to_spark(&addr.to_string()),
        Err(e) => {
            set_error(format!("{e}"));
            rust_str_to_spark("<error>")
        }
    }
}

/// Returns the local address of a TCP stream as a `SparkString`.
#[no_mangle]
pub extern "C" fn __spark_net_tcp_local_addr(stream_handle: i64) -> *mut u8 {
    let Some(stream) = get_tcp_stream(stream_handle) else {
        return rust_str_to_spark("<error>");
    };
    let rt = tokio_runtime();
    let result = rt.block_on(async {
        let guard = stream.lock().await;
        guard.local_addr()
    });
    match result {
        Ok(addr) => rust_str_to_spark(&addr.to_string()),
        Err(e) => {
            set_error(format!("{e}"));
            rust_str_to_spark("<error>")
        }
    }
}

/// Shuts down a TCP stream.
///
/// Returns 0 on success, 1 on error.
#[no_mangle]
pub extern "C" fn __spark_net_tcp_shutdown(stream_handle: i64) -> i64 {
    let Some(stream) = get_tcp_stream(stream_handle) else {
        set_error("invalid stream handle".to_string());
        return 1;
    };
    let rt = tokio_runtime();
    match rt.block_on(async {
        use tokio::io::AsyncWriteExt;
        let mut guard = stream.lock().await;
        guard.shutdown().await
    }) {
        Ok(()) => {
            clear_error();
            0
        }
        Err(e) => {
            set_error(format!("{e}"));
            1
        }
    }
}

// =============================================================================
// UDP
// =============================================================================

/// Binds a UDP socket on `addr:port`.
///
/// Returns a handle ID (> 0) on success, or -1 on error.
///
/// # Safety
///
/// `addr` must be a valid `SparkString` pointer.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_net_udp_bind(addr: *mut u8, port: i64) -> i64 {
    let addr_str = unsafe { read_spark_str(addr) };
    let bind_addr = format!("{addr_str}:{port}");
    let rt = tokio_runtime();
    match rt.block_on(tokio::net::UdpSocket::bind(&bind_addr)) {
        Ok(socket) => {
            clear_error();
            insert_resource(NetResource::UdpSocket(Arc::new(socket)))
        }
        Err(e) => {
            set_error(format!("{e}"));
            -1
        }
    }
}

/// Sends data via UDP to the specified address.
///
/// Returns 0 on success, 1 on error.
///
/// # Safety
///
/// `data` and `addr` must be valid `SparkString` pointers.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_net_udp_send_to(
    socket_handle: i64,
    data: *mut u8,
    addr: *mut u8,
    port: i64,
) -> i64 {
    let Some(socket) = get_udp_socket(socket_handle) else {
        set_error("invalid socket handle".to_string());
        return 1;
    };
    let data_str = unsafe { read_spark_str(data) };
    let addr_str = unsafe { read_spark_str(addr) };
    let target = format!("{addr_str}:{port}");
    let rt = tokio_runtime();
    match rt.block_on(socket.send_to(data_str.as_bytes(), &target)) {
        Ok(_) => {
            clear_error();
            0
        }
        Err(e) => {
            set_error(format!("{e}"));
            1
        }
    }
}

/// Receives data from a UDP socket.
///
/// Returns the received data as a `SparkString`. Returns empty string on error.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub extern "C" fn __spark_net_udp_recv(socket_handle: i64, max_bytes: i64) -> *mut u8 {
    let Some(socket) = get_udp_socket(socket_handle) else {
        set_error("invalid socket handle".to_string());
        return rust_str_to_spark("");
    };
    let max = max_bytes.max(0) as usize;
    let rt = tokio_runtime();
    match rt.block_on(async {
        let mut buf = vec![0u8; max];
        socket.recv(&mut buf).await.map(|n| {
            buf.truncate(n);
            buf
        })
    }) {
        Ok(data) => {
            clear_error();
            let text = String::from_utf8_lossy(&data);
            rust_str_to_spark(&text)
        }
        Err(e) => {
            set_error(format!("{e}"));
            rust_str_to_spark("")
        }
    }
}

// =============================================================================
// DNS
// =============================================================================

/// Resolves a hostname to a comma-separated list of IP addresses.
///
/// Returns the IPs as a `SparkString`. Returns empty string on error.
///
/// # Safety
///
/// `hostname` must be a valid `SparkString` pointer.
#[no_mangle]
pub unsafe extern "C" fn __spark_net_resolve(hostname: *mut u8) -> *mut u8 {
    let host = unsafe { read_spark_str(hostname) };
    let rt = tokio_runtime();
    match rt.block_on(tokio::net::lookup_host(format!("{host}:0"))) {
        Ok(addrs) => {
            clear_error();
            let ips: Vec<String> = addrs.map(|a| a.ip().to_string()).collect();
            rust_str_to_spark(&ips.join(","))
        }
        Err(e) => {
            set_error(format!("{e}"));
            rust_str_to_spark("")
        }
    }
}

// =============================================================================
// Resource management
// =============================================================================

/// Closes a networking resource by handle ID.
///
/// Removes the resource from the handle table and drops it.
#[no_mangle]
pub extern "C" fn __spark_net_close(handle: i64) {
    let _ = remove_resource(handle);
}

/// Returns the last network error message as a `SparkString`.
#[no_mangle]
pub extern "C" fn __spark_net_error() -> *mut u8 {
    LAST_NET_ERROR.with(|e| {
        let msg = e.borrow();
        rust_str_to_spark(&msg)
    })
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: read a `SparkString` result into a Rust `String`.
    #[allow(clippy::cast_ptr_alignment)] // SparkString pointers are properly aligned
    unsafe fn read_result_string(ptr: *mut u8) -> String {
        let s = unsafe { &*(ptr.cast::<SparkString>()) };
        if s.len == 0 {
            return String::new();
        }
        let slice = unsafe { std::slice::from_raw_parts(s.data, s.len) };
        std::str::from_utf8(slice).unwrap().to_owned()
    }

    fn make_spark_str(s: &str) -> *mut u8 {
        rust_str_to_spark(s)
    }

    #[test]
    fn tcp_listen_and_connect() {
        let addr = make_spark_str("127.0.0.1");

        // Bind listener on port 0 (OS picks a free port).
        let listener_handle = unsafe { __spark_net_tcp_listen(addr, 0) };
        assert!(listener_handle > 0, "listener should get a valid handle");

        // Get the assigned address.
        let addr_str = unsafe { read_result_string(__spark_net_listener_addr(listener_handle)) };
        assert!(addr_str.starts_with("127.0.0.1:"), "should be 127.0.0.1:PORT");

        // Parse the port from the address.
        let port: i64 = addr_str
            .split(':')
            .next_back()
            .unwrap()
            .parse()
            .expect("port should be a number");

        // Connect in a background thread (create SparkString inside thread).
        let client_thread = std::thread::spawn(move || unsafe {
            let connect_addr = make_spark_str("127.0.0.1");
            __spark_net_tcp_connect(connect_addr, port)
        });

        let server_stream = __spark_net_tcp_accept(listener_handle);
        assert!(server_stream > 0, "accept should return valid handle");

        let client_stream = client_thread.join().expect("client thread should not panic");
        assert!(client_stream > 0, "connect should return valid handle");

        // Clean up.
        __spark_net_close(server_stream);
        __spark_net_close(client_stream);
        __spark_net_close(listener_handle);
    }

    #[test]
    fn tcp_echo() {
        let addr = make_spark_str("127.0.0.1");
        let listener_handle = unsafe { __spark_net_tcp_listen(addr, 0) };
        assert!(listener_handle > 0);

        let addr_str = unsafe { read_result_string(__spark_net_listener_addr(listener_handle)) };
        let port: i64 = addr_str.split(':').next_back().unwrap().parse().unwrap();

        let client_thread = std::thread::spawn(move || unsafe {
            let connect_addr = make_spark_str("127.0.0.1");
            let stream = __spark_net_tcp_connect(connect_addr, port);
            assert!(stream > 0);

            let msg = make_spark_str("hello net");
            let write_result = __spark_net_tcp_write(stream, msg);
            assert_eq!(write_result, 0, "write should succeed");

            // Shutdown write side so server sees EOF.
            __spark_net_tcp_shutdown(stream);

            // Read echo back.
            let data = __spark_net_tcp_read(stream, 1024);
            let text = read_result_string(data);
            __spark_net_close(stream);
            text
        });

        // Server side: accept, read, echo back.
        let server_stream = __spark_net_tcp_accept(listener_handle);
        assert!(server_stream > 0);

        unsafe {
            let data = __spark_net_tcp_read(server_stream, 1024);
            let text = read_result_string(data);
            assert_eq!(text, "hello net");

            // Echo back.
            let echo = make_spark_str(&text);
            let write_result = __spark_net_tcp_write(server_stream, echo);
            assert_eq!(write_result, 0);
            __spark_net_tcp_shutdown(server_stream);
        }

        let echoed = client_thread.join().expect("client should not panic");
        assert_eq!(echoed, "hello net");

        __spark_net_close(server_stream);
        __spark_net_close(listener_handle);
    }

    #[test]
    fn tcp_peer_and_local_addr() {
        let addr = make_spark_str("127.0.0.1");
        let listener_handle = unsafe { __spark_net_tcp_listen(addr, 0) };
        let addr_str = unsafe { read_result_string(__spark_net_listener_addr(listener_handle)) };
        let port: i64 = addr_str.split(':').next_back().unwrap().parse().unwrap();

        let client_thread = std::thread::spawn(move || unsafe {
            let connect_addr = make_spark_str("127.0.0.1");
            __spark_net_tcp_connect(connect_addr, port)
        });

        let server_stream = __spark_net_tcp_accept(listener_handle);
        let client_stream = client_thread.join().unwrap();

        unsafe {
            let peer = read_result_string(__spark_net_tcp_peer_addr(server_stream));
            assert!(peer.starts_with("127.0.0.1:"), "peer should be 127.0.0.1:PORT");

            let local = read_result_string(__spark_net_tcp_local_addr(server_stream));
            assert!(
                local.starts_with("127.0.0.1:"),
                "local should be 127.0.0.1:PORT"
            );
        }

        __spark_net_close(server_stream);
        __spark_net_close(client_stream);
        __spark_net_close(listener_handle);
    }

    #[test]
    fn udp_send_recv() {
        let addr = make_spark_str("127.0.0.1");

        // Bind two sockets on port 0.
        let sock_a = unsafe { __spark_net_udp_bind(addr, 0) };
        assert!(sock_a > 0);
        let sock_b = unsafe { __spark_net_udp_bind(addr, 0) };
        assert!(sock_b > 0);

        // Get sock_b's address.
        let b_addr = {
            let table = HANDLE_TABLE.lock().unwrap();
            match table.get(&sock_b) {
                Some(NetResource::UdpSocket(s)) => s.local_addr().unwrap().to_string(),
                _ => panic!("expected UdpSocket"),
            }
        };
        let b_port: i64 = b_addr.split(':').next_back().unwrap().parse().unwrap();

        // Send from A to B.
        let msg = make_spark_str("udp hello");
        let target_addr = make_spark_str("127.0.0.1");
        let send_result = unsafe { __spark_net_udp_send_to(sock_a, msg, target_addr, b_port) };
        assert_eq!(send_result, 0);

        // Receive on B.
        let received = unsafe { read_result_string(__spark_net_udp_recv(sock_b, 1024)) };
        assert_eq!(received, "udp hello");

        __spark_net_close(sock_a);
        __spark_net_close(sock_b);
    }

    #[test]
    fn dns_resolve_localhost() {
        let host = make_spark_str("localhost");
        let result = unsafe { read_result_string(__spark_net_resolve(host)) };
        // localhost should resolve to 127.0.0.1 or ::1.
        assert!(
            result.contains("127.0.0.1") || result.contains("::1"),
            "localhost should resolve to loopback: got {result}"
        );
    }

    #[test]
    fn close_invalid_handle() {
        // Should not panic.
        __spark_net_close(999_999);
    }

    #[test]
    fn connect_to_invalid_addr() {
        let addr = make_spark_str("127.0.0.1");
        // Port 1 is almost certainly not listening.
        let handle = unsafe { __spark_net_tcp_connect(addr, 1) };
        assert_eq!(handle, -1, "connect to port 1 should fail");

        let err = unsafe { read_result_string(__spark_net_error()) };
        assert!(!err.is_empty(), "error message should not be empty");
    }

    #[test]
    fn error_returns_empty_on_success() {
        let addr = make_spark_str("127.0.0.1");
        let handle = unsafe { __spark_net_tcp_listen(addr, 0) };
        assert!(handle > 0);

        let err = unsafe { read_result_string(__spark_net_error()) };
        assert!(err.is_empty(), "error should be empty after success");

        __spark_net_close(handle);
    }
}
