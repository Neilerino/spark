//! Shared hash-table allocation and probing helpers.

use std::mem::size_of;

use super::storage::{alloc_zeroed_buffer, byte_size, dealloc_buffer};

// =============================================================================
// Hash infrastructure — FNV-1a for open-addressing hash tables
// =============================================================================

/// Sentinel value for an empty hash slot.
pub(super) const HASH_EMPTY: u64 = 0;
/// Sentinel value for a tombstoned (deleted) hash slot.
pub(super) const HASH_TOMBSTONE: u64 = 1;
/// Minimum capacity for hash tables (must be a power of 2).
pub(super) const MIN_HASH_CAP: usize = 8;

/// Returns true if the hash value represents an occupied slot (not empty or tombstone).
pub(super) const fn is_occupied(h: u64) -> bool {
    h >= 2
}

pub(super) enum LookupSlot {
    Occupied(usize),
    Empty,
}

pub(super) enum InsertionSlot {
    Occupied(usize),
    Vacant {
        index: usize,
        reused_tombstone: bool,
    },
}

/// Finds an occupied slot matching `h`, or the first empty slot that ends lookup.
///
/// # Safety
///
/// `hashes` must point to a valid hash buffer of `cap` u64 values, and `cap`
/// must be a non-zero power of two.
#[allow(clippy::cast_possible_truncation)]
pub(super) unsafe fn lookup_slot(
    hashes: *const u64,
    cap: usize,
    h: u64,
    mut matches_slot: impl FnMut(usize) -> bool,
) -> LookupSlot {
    let mask = cap - 1;
    let mut idx = (h as usize) & mask;

    loop {
        let slot_hash = unsafe { *hashes.add(idx) };
        if slot_hash == HASH_EMPTY {
            return LookupSlot::Empty;
        }
        if slot_hash == h && matches_slot(idx) {
            return LookupSlot::Occupied(idx);
        }
        idx = (idx + 1) & mask;
    }
}

/// Finds an existing matching slot or the slot where a new value should be inserted.
///
/// # Safety
///
/// `hashes` must point to a valid hash buffer of `cap` u64 values, and `cap`
/// must be a non-zero power of two.
#[allow(clippy::cast_possible_truncation)]
pub(super) unsafe fn insertion_slot(
    hashes: *const u64,
    cap: usize,
    h: u64,
    mut matches_slot: impl FnMut(usize) -> bool,
) -> InsertionSlot {
    let mask = cap - 1;
    let mut idx = (h as usize) & mask;
    let mut first_tombstone: Option<usize> = None;

    loop {
        let slot_hash = unsafe { *hashes.add(idx) };
        if slot_hash == HASH_EMPTY {
            let index = first_tombstone.unwrap_or(idx);
            return InsertionSlot::Vacant {
                index,
                reused_tombstone: first_tombstone.is_some(),
            };
        }
        if slot_hash == HASH_TOMBSTONE {
            if first_tombstone.is_none() {
                first_tombstone = Some(idx);
            }
        } else if slot_hash == h && matches_slot(idx) {
            return InsertionSlot::Occupied(idx);
        }
        idx = (idx + 1) & mask;
    }
}

#[allow(clippy::cast_ptr_alignment)]
pub(super) unsafe fn alloc_hashes(cap: usize) -> *mut u64 {
    let byte_size = byte_size(cap, size_of::<u64>());
    unsafe {
        alloc_zeroed_buffer(
            byte_size,
            std::mem::align_of::<u64>(),
            "spark: out of memory",
        )
    }
    .cast::<u64>()
}

pub(super) unsafe fn dealloc_hashes(hashes: *mut u64, cap: usize) {
    unsafe {
        dealloc_buffer(
            hashes.cast::<u8>(),
            byte_size(cap, size_of::<u64>()),
            std::mem::align_of::<u64>(),
        );
    };
}

/// Re-inserts occupied hashes into a fresh table and reports payload moves.
///
/// # Safety
///
/// `old_hashes` must point to `old_cap` hash slots, `new_hashes` must point
/// to `new_cap` zeroed hash slots, and both capacities must be powers of two.
pub(super) unsafe fn rehash_occupied_slots(
    old_hashes: *const u64,
    old_cap: usize,
    new_hashes: *mut u64,
    new_cap: usize,
    mut move_payload: impl FnMut(usize, usize),
) {
    let mask = new_cap - 1;

    for old_idx in 0..old_cap {
        let h = unsafe { *old_hashes.add(old_idx) };
        if is_occupied(h) {
            let new_idx = unsafe { probe_empty(new_hashes, h, mask) };
            unsafe { *new_hashes.add(new_idx) = h };
            move_payload(old_idx, new_idx);
        }
    }
}

/// Calls `visit` once for every occupied slot.
///
/// # Safety
///
/// `hashes` must point to a valid hash buffer of `cap` u64 values.
pub(super) unsafe fn for_occupied_slots(
    hashes: *const u64,
    cap: usize,
    mut visit: impl FnMut(usize),
) {
    for idx in 0..cap {
        if is_occupied(unsafe { *hashes.add(idx) }) {
            visit(idx);
        }
    }
}

/// Finds an empty slot in a fresh hash table (no tombstones) for the given hash.
///
/// # Safety
///
/// `hashes` must point to a valid zeroed buffer of at least `mask + 1` u64 values.
#[allow(clippy::cast_possible_truncation)] // Hash-to-index truncation is masked to valid range
pub(super) unsafe fn probe_empty(hashes: *mut u64, h: u64, mask: usize) -> usize {
    let mut idx = (h as usize) & mask;
    loop {
        if unsafe { *hashes.add(idx) } == HASH_EMPTY {
            return idx;
        }
        idx = (idx + 1) & mask;
    }
}
