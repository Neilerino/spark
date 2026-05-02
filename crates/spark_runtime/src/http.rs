//! `spark.http` runtime implementations.
//!
//! Provides `extern "C"` functions for the `spark.http` standard library module.
//!
#![allow(clippy::option_if_let_else)]
#![allow(clippy::similar_names)]
//!
//! ## Client
//!
//! HTTP client powered by `reqwest`. Supports GET, POST, PUT, DELETE with
//! optional custom headers. All requests block via `tokio_runtime().block_on()`.
//!
//! ## Server
//!
//! HTTP server powered by `tokio::net::TcpListener` with manual HTTP/1.1
//! parsing. Each `recv()` call accepts a connection, reads the HTTP request,
//! and stores both the parsed request and raw stream for later `respond()`.
//!
//! ## Error Model
//!
//! Thread-local `LAST_HTTP_ERROR` stores the last error message. Functions
//! return status codes or -1 handles on failure.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use crate::error as runtime_error;
use crate::handles::HandleTable;
use crate::string::{read_spark_str, rust_str_to_spark};
use crate::tokio_runtime;

// =============================================================================
// Handle tables
// =============================================================================

/// A parsed HTTP request with its underlying TCP stream for responding.
struct HttpRequest {
    method: String,
    url: String,
    headers: HashMap<String, String>,
    body: String,
    /// The TCP stream for sending the response.
    stream: tokio::sync::Mutex<tokio::net::TcpStream>,
}

/// An HTTP server backed by a TCP listener.
struct HttpServer {
    listener: Arc<tokio::net::TcpListener>,
}

/// Global handle table for HTTP servers.
static SERVER_TABLE: std::sync::LazyLock<HandleTable<HttpServer>> =
    std::sync::LazyLock::new(HandleTable::new);

/// Global handle table for pending HTTP requests.
static REQUEST_TABLE: std::sync::LazyLock<HandleTable<HttpRequest>> =
    std::sync::LazyLock::new(HandleTable::new);

// =============================================================================
// Error handling
// =============================================================================

thread_local! {
    static LAST_HTTP_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_error(msg: impl Into<String>) {
    runtime_error::set(&LAST_HTTP_ERROR, msg);
}

fn clear_error() {
    runtime_error::clear(&LAST_HTTP_ERROR);
}

// =============================================================================
// HTTP Client — helpers
// =============================================================================

/// Parses newline-separated `"Key: Value"` headers into a `reqwest::header::HeaderMap`.
fn parse_headers(headers_str: &str) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for line in headers_str.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            if let (Ok(name), Ok(val)) = (
                reqwest::header::HeaderName::from_bytes(key.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                map.insert(name, val);
            }
        }
    }
    map
}

/// Formats response headers as newline-separated `"Key: Value"` string.
fn format_response_headers(headers: &reqwest::header::HeaderMap) -> String {
    let mut result = String::new();
    for (key, value) in headers {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(key.as_str());
        result.push_str(": ");
        result.push_str(value.to_str().unwrap_or("<non-ascii>"));
    }
    result
}

/// Returns a shared reqwest client (reuses connections).
fn http_client() -> &'static reqwest::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .build()
            .expect("spark: failed to create HTTP client")
    })
}

/// Executes an HTTP request and returns (status, body, headers) as `SparkString` pointers.
///
/// Returns (`status_code`, `body_ptr`, `headers_ptr`). On error, status = -1.
fn execute_request(
    method: reqwest::Method,
    url: &str,
    headers_str: &str,
    body: Option<&str>,
) -> (i64, *mut u8, *mut u8) {
    let client = http_client();
    let rt = tokio_runtime();

    let result = rt.block_on(async {
        let mut builder = client.request(method, url);
        if !headers_str.is_empty() {
            builder = builder.headers(parse_headers(headers_str));
        }
        if let Some(b) = body {
            builder = builder.body(b.to_owned());
        }
        builder.send().await
    });

    match result {
        Ok(response) => {
            let status = i64::from(response.status().as_u16());
            let resp_headers = format_response_headers(response.headers());
            match rt.block_on(response.text()) {
                Ok(body_text) => {
                    clear_error();
                    (
                        status,
                        rust_str_to_spark(&body_text),
                        rust_str_to_spark(&resp_headers),
                    )
                }
                Err(e) => {
                    set_error(format!("{e}"));
                    (
                        status,
                        rust_str_to_spark(""),
                        rust_str_to_spark(&resp_headers),
                    )
                }
            }
        }
        Err(e) => {
            set_error(format!("{e}"));
            (-1, rust_str_to_spark(""), rust_str_to_spark(""))
        }
    }
}

// =============================================================================
// HTTP Client FFI
// =============================================================================

/// Performs an HTTP GET request.
///
/// Returns the HTTP status code (e.g. 200), or -1 on error.
/// The response body and headers are stored and retrievable via
/// `__spark_http_response_body` and `__spark_http_response_headers`.
///
/// # Safety
///
/// `url` must be a valid `SparkString` pointer.
#[no_mangle]
pub unsafe extern "C" fn __spark_http_get(url: *mut u8, headers: *mut u8) -> i64 {
    let url_str = unsafe { read_spark_str(url) };
    let headers_str = unsafe { read_spark_str(headers) };
    let (status, body_ptr, headers_ptr) =
        execute_request(reqwest::Method::GET, &url_str, &headers_str, None);
    LAST_RESPONSE_BODY.with(|b| b.set(body_ptr));
    LAST_RESPONSE_HEADERS.with(|h| h.set(headers_ptr));
    status
}

/// Performs an HTTP POST request.
///
/// # Safety
///
/// `url`, `body`, and `headers` must be valid `SparkString` pointers.
#[no_mangle]
pub unsafe extern "C" fn __spark_http_post(url: *mut u8, body: *mut u8, headers: *mut u8) -> i64 {
    let url_str = unsafe { read_spark_str(url) };
    let body_str = unsafe { read_spark_str(body) };
    let headers_str = unsafe { read_spark_str(headers) };
    let (status, body_ptr, headers_ptr) = execute_request(
        reqwest::Method::POST,
        &url_str,
        &headers_str,
        Some(&body_str),
    );
    LAST_RESPONSE_BODY.with(|b| b.set(body_ptr));
    LAST_RESPONSE_HEADERS.with(|h| h.set(headers_ptr));
    status
}

/// Performs an HTTP PUT request.
///
/// # Safety
///
/// `url`, `body`, and `headers` must be valid `SparkString` pointers.
#[no_mangle]
pub unsafe extern "C" fn __spark_http_put(url: *mut u8, body: *mut u8, headers: *mut u8) -> i64 {
    let url_str = unsafe { read_spark_str(url) };
    let body_str = unsafe { read_spark_str(body) };
    let headers_str = unsafe { read_spark_str(headers) };
    let (status, body_ptr, headers_ptr) = execute_request(
        reqwest::Method::PUT,
        &url_str,
        &headers_str,
        Some(&body_str),
    );
    LAST_RESPONSE_BODY.with(|b| b.set(body_ptr));
    LAST_RESPONSE_HEADERS.with(|h| h.set(headers_ptr));
    status
}

/// Performs an HTTP DELETE request.
///
/// # Safety
///
/// `url` and `headers` must be valid `SparkString` pointers.
#[no_mangle]
pub unsafe extern "C" fn __spark_http_delete(url: *mut u8, headers: *mut u8) -> i64 {
    let url_str = unsafe { read_spark_str(url) };
    let headers_str = unsafe { read_spark_str(headers) };
    let (status, body_ptr, headers_ptr) =
        execute_request(reqwest::Method::DELETE, &url_str, &headers_str, None);
    LAST_RESPONSE_BODY.with(|b| b.set(body_ptr));
    LAST_RESPONSE_HEADERS.with(|h| h.set(headers_ptr));
    status
}

// Thread-local storage for the last response body and headers.
// These are set by __spark_http_get/post/put/delete and read by
// __spark_http_response_body/headers.
thread_local! {
    static LAST_RESPONSE_BODY: std::cell::Cell<*mut u8> =
        const { std::cell::Cell::new(std::ptr::null_mut()) };
    static LAST_RESPONSE_HEADERS: std::cell::Cell<*mut u8> =
        const { std::cell::Cell::new(std::ptr::null_mut()) };
}

/// Returns the body from the last HTTP response.
#[no_mangle]
pub extern "C" fn __spark_http_response_body() -> *mut u8 {
    LAST_RESPONSE_BODY.with(|b| {
        let ptr = b.get();
        if ptr.is_null() {
            rust_str_to_spark("")
        } else {
            ptr
        }
    })
}

/// Returns the headers from the last HTTP response.
#[no_mangle]
pub extern "C" fn __spark_http_response_headers() -> *mut u8 {
    LAST_RESPONSE_HEADERS.with(|h| {
        let ptr = h.get();
        if ptr.is_null() {
            rust_str_to_spark("")
        } else {
            ptr
        }
    })
}

/// Returns the last HTTP error message.
#[no_mangle]
pub extern "C" fn __spark_http_error() -> *mut u8 {
    runtime_error::to_spark(&LAST_HTTP_ERROR)
}

// =============================================================================
// HTTP Server
// =============================================================================

/// Creates an HTTP server listening on `addr:port`.
///
/// Returns a server handle (> 0) on success, or -1 on error.
///
/// # Safety
///
/// `addr` must be a valid `SparkString` pointer.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_http_serve(addr: *mut u8, port: i64) -> i64 {
    let addr_str = unsafe { read_spark_str(addr) };
    let bind_addr = format!("{addr_str}:{port}");
    let rt = tokio_runtime();
    match rt.block_on(tokio::net::TcpListener::bind(&bind_addr)) {
        Ok(listener) => {
            clear_error();
            let server = HttpServer {
                listener: Arc::new(listener),
            };
            SERVER_TABLE.insert(server)
        }
        Err(e) => {
            set_error(format!("{e}"));
            -1
        }
    }
}

/// Returns the local address of an HTTP server as a `SparkString`.
#[no_mangle]
pub extern "C" fn __spark_http_server_addr(server_handle: i64) -> *mut u8 {
    let Some(addr_result) = SERVER_TABLE.with(server_handle, |server| server.listener.local_addr())
    else {
        return rust_str_to_spark("<error>");
    };
    match addr_result {
        Ok(addr) => rust_str_to_spark(&addr.to_string()),
        Err(e) => {
            set_error(format!("{e}"));
            rust_str_to_spark("<error>")
        }
    }
}

/// Accepts the next HTTP request on a server.
///
/// Blocks until a client connects. Parses the HTTP/1.1 request and stores
/// the request + stream for later `respond()`.
///
/// Returns a request handle (> 0) on success, or -1 on error.
#[no_mangle]
pub extern "C" fn __spark_http_recv(server_handle: i64) -> i64 {
    let Some(listener) = SERVER_TABLE.with(server_handle, |server| Arc::clone(&server.listener))
    else {
        set_error("invalid server handle");
        return -1;
    };

    let rt = tokio_runtime();
    match rt.block_on(async { accept_and_parse_request(&listener).await }) {
        Ok(request) => {
            clear_error();
            REQUEST_TABLE.insert(request)
        }
        Err(e) => {
            set_error(e);
            -1
        }
    }
}

/// Accepts a TCP connection and parses the HTTP/1.1 request.
async fn accept_and_parse_request(
    listener: &tokio::net::TcpListener,
) -> Result<HttpRequest, String> {
    use tokio::io::AsyncReadExt;

    let (mut stream, _addr) = listener
        .accept()
        .await
        .map_err(|e| format!("accept failed: {e}"))?;

    // Read the request data. We use a 64KB buffer and read until we have
    // the full headers (delimited by \r\n\r\n).
    let mut buf = Vec::with_capacity(65536);
    let mut temp = [0u8; 4096];
    let mut headers_end = None;

    loop {
        let n = stream
            .read(&mut temp)
            .await
            .map_err(|e| format!("read failed: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&temp[..n]);

        // Check for end of headers.
        if let Some(pos) = find_header_end(&buf) {
            headers_end = Some(pos);
            break;
        }

        if buf.len() > 65536 {
            return Err("request too large".to_string());
        }
    }

    let header_end_pos = headers_end.unwrap_or(buf.len());
    let header_section = String::from_utf8_lossy(&buf[..header_end_pos]).to_string();

    // Parse request line.
    let mut lines = header_section.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let url = parts.next().unwrap_or("/").to_string();

    // Parse headers.
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_lowercase(), value.trim().to_string());
        }
    }

    // Read body if Content-Length is set.
    let body_start = header_end_pos + 4; // skip \r\n\r\n
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut body_bytes = if body_start < buf.len() {
        buf[body_start..].to_vec()
    } else {
        Vec::new()
    };

    // Read remaining body bytes if needed.
    while body_bytes.len() < content_length {
        let n = stream
            .read(&mut temp)
            .await
            .map_err(|e| format!("read body failed: {e}"))?;
        if n == 0 {
            break;
        }
        body_bytes.extend_from_slice(&temp[..n]);
    }
    body_bytes.truncate(content_length);

    let body = String::from_utf8_lossy(&body_bytes).to_string();

    Ok(HttpRequest {
        method,
        url,
        headers,
        body,
        stream: tokio::sync::Mutex::new(stream),
    })
}

/// Finds the position of `\r\n\r\n` in a byte slice.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Returns the HTTP method of a request.
#[no_mangle]
pub extern "C" fn __spark_http_request_method(req_handle: i64) -> *mut u8 {
    REQUEST_TABLE
        .with(req_handle, |req| rust_str_to_spark(&req.method))
        .unwrap_or_else(|| rust_str_to_spark(""))
}

/// Returns the URL path of a request.
#[no_mangle]
pub extern "C" fn __spark_http_request_url(req_handle: i64) -> *mut u8 {
    REQUEST_TABLE
        .with(req_handle, |req| rust_str_to_spark(&req.url))
        .unwrap_or_else(|| rust_str_to_spark(""))
}

/// Returns the body of a request.
#[no_mangle]
pub extern "C" fn __spark_http_request_body(req_handle: i64) -> *mut u8 {
    REQUEST_TABLE
        .with(req_handle, |req| rust_str_to_spark(&req.body))
        .unwrap_or_else(|| rust_str_to_spark(""))
}

/// Returns a specific header value from a request.
///
/// # Safety
///
/// `name` must be a valid `SparkString` pointer.
#[no_mangle]
pub unsafe extern "C" fn __spark_http_request_header(req_handle: i64, name: *mut u8) -> *mut u8 {
    let name_str = unsafe { read_spark_str(name) }.to_lowercase();
    REQUEST_TABLE
        .with(req_handle, |req| {
            req.headers
                .get(&name_str)
                .map_or_else(|| rust_str_to_spark(""), |val| rust_str_to_spark(val))
        })
        .unwrap_or_else(|| rust_str_to_spark(""))
}

/// Sends an HTTP response for a pending request.
///
/// Formats an HTTP/1.1 response and writes it to the TCP stream.
/// Returns 0 on success, 1 on error.
///
/// # Safety
///
/// `body` and `headers` must be valid `SparkString` pointers.
#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_http_respond(
    req_handle: i64,
    status: i64,
    body: *mut u8,
    headers: *mut u8,
) -> i64 {
    let body_str = unsafe { read_spark_str(body) };
    let headers_str = unsafe { read_spark_str(headers) };

    // Remove the request from the table (we consume it).
    let request = REQUEST_TABLE.remove(req_handle);

    let Some(request) = request else {
        set_error("invalid request handle");
        return 1;
    };

    let reason = match status {
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        // 200 and any unrecognized status
        _ => "OK",
    };

    // Build the HTTP response.
    let mut response = format!("HTTP/1.1 {status} {reason}\r\n");
    let _ = write!(response, "Content-Length: {}\r\n", body_str.len());

    // Add custom headers.
    for line in headers_str.lines() {
        if !line.is_empty() {
            response.push_str(line);
            response.push_str("\r\n");
        }
    }

    response.push_str("Connection: close\r\n");
    response.push_str("\r\n");
    response.push_str(&body_str);

    let rt = tokio_runtime();
    match rt.block_on(async {
        use tokio::io::AsyncWriteExt;
        let mut stream = request.stream.lock().await;
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await
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

/// Closes an HTTP server.
#[no_mangle]
pub extern "C" fn __spark_http_close_server(server_handle: i64) {
    SERVER_TABLE.remove(server_handle);
}

/// Closes a pending HTTP request (without responding).
#[no_mangle]
pub extern "C" fn __spark_http_close_request(req_handle: i64) {
    REQUEST_TABLE.remove(req_handle);
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SparkString;

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
    fn http_server_roundtrip() {
        let addr = make_spark_str("127.0.0.1");
        let server = unsafe { __spark_http_serve(addr, 0) };
        assert!(server > 0, "server should start");

        let server_addr = unsafe { read_result_string(__spark_http_server_addr(server)) };
        assert!(server_addr.starts_with("127.0.0.1:"));

        // Client thread sends a request.
        let url = format!("http://{server_addr}/test");
        let client_thread = std::thread::spawn(move || {
            let rt = tokio_runtime();
            rt.block_on(async {
                let client = reqwest::Client::new();
                let resp = client.get(&url).send().await.unwrap();
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap();
                (status, body)
            })
        });

        // Server side: receive request, send response.
        let req = __spark_http_recv(server);
        assert!(req > 0, "recv should return valid handle");

        let method = unsafe { read_result_string(__spark_http_request_method(req)) };
        assert_eq!(method, "GET");

        let req_url = unsafe { read_result_string(__spark_http_request_url(req)) };
        assert_eq!(req_url, "/test");

        let body = make_spark_str("hello from spark");
        let headers = make_spark_str("X-Custom: test-value");
        let result = unsafe { __spark_http_respond(req, 200, body, headers) };
        assert_eq!(result, 0, "respond should succeed");

        // Check client received the response.
        let (status, resp_body) = client_thread.join().expect("client should not panic");
        assert_eq!(status, 200);
        assert_eq!(resp_body, "hello from spark");

        __spark_http_close_server(server);
    }

    #[test]
    fn http_server_post_with_body() {
        let addr = make_spark_str("127.0.0.1");
        let server = unsafe { __spark_http_serve(addr, 0) };
        assert!(server > 0);

        let server_addr = unsafe { read_result_string(__spark_http_server_addr(server)) };

        let url = format!("http://{server_addr}/submit");
        let client_thread = std::thread::spawn(move || {
            let rt = tokio_runtime();
            rt.block_on(async {
                let client = reqwest::Client::new();
                client
                    .post(&url)
                    .body("request body data")
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap()
            })
        });

        let req = __spark_http_recv(server);
        assert!(req > 0);

        let method = unsafe { read_result_string(__spark_http_request_method(req)) };
        assert_eq!(method, "POST");

        let req_body = unsafe { read_result_string(__spark_http_request_body(req)) };
        assert_eq!(req_body, "request body data");

        let body = make_spark_str("accepted");
        let headers = make_spark_str("");
        unsafe { __spark_http_respond(req, 200, body, headers) };

        let resp = client_thread.join().unwrap();
        assert_eq!(resp, "accepted");

        __spark_http_close_server(server);
    }

    #[test]
    fn http_server_request_header() {
        let addr = make_spark_str("127.0.0.1");
        let server = unsafe { __spark_http_serve(addr, 0) };
        assert!(server > 0);

        let server_addr = unsafe { read_result_string(__spark_http_server_addr(server)) };

        let url = format!("http://{server_addr}/headers");
        let client_thread = std::thread::spawn(move || {
            let rt = tokio_runtime();
            rt.block_on(async {
                let client = reqwest::Client::new();
                client
                    .get(&url)
                    .header("X-Test-Header", "test-value-123")
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap()
            })
        });

        let req = __spark_http_recv(server);
        assert!(req > 0);

        let header_name = make_spark_str("x-test-header");
        let header_val =
            unsafe { read_result_string(__spark_http_request_header(req, header_name)) };
        assert_eq!(header_val, "test-value-123");

        let body = make_spark_str("ok");
        let headers = make_spark_str("");
        unsafe { __spark_http_respond(req, 200, body, headers) };

        client_thread.join().unwrap();
        __spark_http_close_server(server);
    }

    #[test]
    fn error_returns_empty_on_success() {
        // After a successful operation, error should be empty.
        let addr = make_spark_str("127.0.0.1");
        let server = unsafe { __spark_http_serve(addr, 0) };
        assert!(server > 0);

        let err = unsafe { read_result_string(__spark_http_error()) };
        assert!(err.is_empty(), "error should be empty after success");

        __spark_http_close_server(server);
    }
}
