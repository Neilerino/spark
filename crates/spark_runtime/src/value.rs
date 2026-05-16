//! Value descriptor and element behavior helpers.

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SparkValueDesc {
    pub size: usize,
    pub align: usize,
    pub drop_fn: usize,
    pub retain_fn: usize,
    pub hash_fn: usize,
    pub eq_fn: usize,
}

impl SparkValueDesc {
    #[must_use]
    pub fn has_drop(&self) -> bool {
        self.drop_fn != 0
    }

    #[must_use]
    pub fn has_retain(&self) -> bool {
        self.retain_fn != 0
    }

    pub unsafe fn hash_elem(&self, elem_ptr: *const u8) -> u64 {
        unsafe { hash_elem(elem_ptr, self.size, self.hash_fn) }
    }

    pub unsafe fn eq_elem(&self, a: *const u8, b: *const u8) -> bool {
        unsafe { eq_elem(a, b, self.size, self.eq_fn) }
    }

    pub unsafe fn drop_elem(&self, elem_ptr: *mut u8) {
        unsafe { drop_elem_if_needed(self.drop_fn, elem_ptr) };
    }

    pub unsafe fn retain_elem(&self, elem_ptr: *mut u8) {
        unsafe { retain_elem_if_needed(self.retain_fn, elem_ptr) };
    }
}

#[no_mangle]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub extern "C" fn __spark_value_desc_new(
    size: i64,
    align: i64,
    drop_fn: usize,
    retain_fn: usize,
    hash_fn: usize,
    eq_fn: usize,
) -> *mut u8 {
    value_desc_new(
        size as usize,
        align as usize,
        drop_fn,
        retain_fn,
        hash_fn,
        eq_fn,
    )
    .cast()
}

pub fn value_desc_new(
    size: usize,
    align: usize,
    drop_fn: usize,
    retain_fn: usize,
    hash_fn: usize,
    eq_fn: usize,
) -> *mut SparkValueDesc {
    let desc = Box::new(SparkValueDesc {
        size: size.max(1),
        align: normalize_align(align),
        drop_fn,
        retain_fn,
        hash_fn,
        eq_fn,
    });
    Box::into_raw(desc)
}

pub unsafe fn value_desc_clone(desc: *const SparkValueDesc) -> *mut SparkValueDesc {
    if desc.is_null() {
        eprintln!("spark: null value descriptor");
        std::process::abort();
    }
    Box::into_raw(Box::new(unsafe { *desc }))
}

pub unsafe fn value_desc_free(desc: *mut SparkValueDesc) {
    if !desc.is_null() {
        drop(unsafe { Box::from_raw(desc) });
    }
}

fn normalize_align(align: usize) -> usize {
    let align = if align == 0 {
        std::mem::align_of::<u64>()
    } else {
        align
    };
    if !align.is_power_of_two() {
        eprintln!("spark: invalid value descriptor alignment");
        std::process::abort();
    }
    align
}

pub fn buffer_align(align: usize) -> usize {
    normalize_align(align).max(std::mem::align_of::<u64>())
}

pub type ElemHashFn = unsafe extern "C" fn(*const u8) -> u64;
pub type ElemEqFn = unsafe extern "C" fn(*const u8, *const u8) -> i8;

pub unsafe fn libc_memcmp(a: *const u8, b: *const u8, len: usize) -> bool {
    let a_slice = unsafe { std::slice::from_raw_parts(a, len) };
    let b_slice = unsafe { std::slice::from_raw_parts(b, len) };
    a_slice == b_slice
}

pub unsafe fn fnv1a_hash(data: *const u8, len: usize) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for i in 0..len {
        hash ^= u64::from(unsafe { *data.add(i) });
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

unsafe fn hash_elem(elem_ptr: *const u8, elem_size: usize, hash_fn: usize) -> u64 {
    if hash_fn == 0 {
        fix_hash(unsafe { fnv1a_hash(elem_ptr, elem_size) })
    } else {
        let f: ElemHashFn = unsafe { std::mem::transmute(hash_fn) };
        fix_hash(unsafe { f(elem_ptr) })
    }
}

unsafe fn eq_elem(a: *const u8, b: *const u8, elem_size: usize, eq_fn: usize) -> bool {
    if eq_fn == 0 {
        unsafe { libc_memcmp(a, b, elem_size) }
    } else {
        let f: ElemEqFn = unsafe { std::mem::transmute(eq_fn) };
        unsafe { f(a, b) != 0 }
    }
}

unsafe fn drop_elem_if_needed(drop_fn: usize, elem_ptr: *mut u8) {
    if drop_fn != 0 {
        let f: unsafe extern "C" fn(*mut u8) = unsafe { std::mem::transmute(drop_fn) };
        unsafe { f(elem_ptr) };
    }
}

unsafe fn retain_elem_if_needed(retain_fn: usize, elem_ptr: *mut u8) {
    if retain_fn != 0 {
        let f: unsafe extern "C" fn(*mut u8) = unsafe { std::mem::transmute(retain_fn) };
        unsafe { f(elem_ptr) };
    }
}

#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "C" fn retain_rc_elem(elem_ptr: *mut u8) {
    let inner = unsafe { *(elem_ptr.cast::<*mut u8>()) };
    if !inner.is_null() {
        unsafe { crate::__spark_rc_inc(inner) };
    }
}

pub const fn fix_hash(h: u64) -> u64 {
    if h < 2 {
        2
    } else {
        h
    }
}
