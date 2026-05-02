//! Shared last-error helpers for runtime stdlib modules.

use std::cell::RefCell;
use std::thread::LocalKey;

use crate::string::rust_str_to_spark;

pub(crate) fn set(slot: &'static LocalKey<RefCell<String>>, msg: impl Into<String>) {
    slot.with(|error| {
        *error.borrow_mut() = msg.into();
    });
}

pub(crate) fn clear(slot: &'static LocalKey<RefCell<String>>) {
    slot.with(|error| {
        error.borrow_mut().clear();
    });
}

pub(crate) fn to_spark(slot: &'static LocalKey<RefCell<String>>) -> *mut u8 {
    slot.with(|error| {
        let msg = error.borrow();
        rust_str_to_spark(&msg)
    })
}
