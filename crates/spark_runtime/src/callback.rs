#![allow(clippy::cast_ptr_alignment)]

macro_rules! call_unary_callback {
    ($fn_addr:expr, $env:expr, $arg:expr, $fn_ty:ty, $pack:ident) => {{
        let f: $fn_ty = unsafe { callback_fn($fn_addr) };
        $pack(unsafe { f($env, $arg) })
    }};
}

macro_rules! call_binary_callback {
    ($fn_addr:expr, $env:expr, $lhs:expr, $rhs:expr, $fn_ty:ty, $pack:ident) => {{
        let f: $fn_ty = unsafe { callback_fn($fn_addr) };
        $pack(unsafe { f($env, $lhs, $rhs) })
    }};
}

#[derive(Clone, Copy)]
pub struct Callback {
    fn_addr: usize,
    env_addr: usize,
    signature: CallbackSignature,
}

impl Callback {
    pub fn new(fn_ptr: *mut u8, env_ptr: *mut u8, cb_kind: u8) -> Self {
        Self {
            fn_addr: fn_ptr as usize,
            env_addr: env_ptr as usize,
            signature: CallbackSignature::from_bits(cb_kind),
        }
    }

    pub unsafe fn call_foreach_elem(self, elem_ptr: *const u8) {
        let arg = unsafe { self.signature.input.read_elem(elem_ptr) };
        unsafe { self.call_foreach(arg) };
    }

    pub unsafe fn call_foreach_int(self, val: i64) {
        unsafe { self.call_foreach(ScalarArg::Int(val)) };
    }

    pub unsafe fn call_map_elem(self, elem_ptr: *const u8) -> u64 {
        let arg = unsafe { self.signature.input.read_elem(elem_ptr) };
        unsafe { self.signature.map.call_unary(self.fn_addr, self.env(), arg) }
    }

    pub unsafe fn call_map_int(self, val: i64) -> u64 {
        unsafe {
            self.signature
                .map
                .call_unary(self.fn_addr, self.env(), ScalarArg::Int(val))
        }
    }

    pub unsafe fn call_reduce(self, acc: u64, val: u64) -> u64 {
        let acc = self.signature.input.unpack(acc);
        let val = self.signature.input.unpack(val);
        unsafe {
            self.signature
                .map
                .call_binary(self.fn_addr, self.env(), acc, val)
        }
    }

    pub fn output_float(self) -> bool {
        self.signature.output == ScalarKind::Float
    }

    fn env(self) -> *mut u8 {
        self.env_addr as *mut u8
    }

    unsafe fn call_foreach(self, arg: ScalarArg) {
        match arg {
            ScalarArg::Int(value) => {
                let f: ForEachIntFn = unsafe { callback_fn(self.fn_addr) };
                unsafe { f(self.env(), value) };
            }
            ScalarArg::Float(value) => {
                let f: ForEachFloatFn = unsafe { callback_fn(self.fn_addr) };
                unsafe { f(self.env(), value) };
            }
        }
    }
}

#[derive(Clone, Copy)]
struct CallbackSignature {
    input: ScalarKind,
    output: ScalarKind,
    map: MapSignature,
}

impl CallbackSignature {
    fn from_bits(bits: u8) -> Self {
        let input = ScalarKind::from_input_bits(bits);
        let output = ScalarKind::from_output_bits(bits);
        Self {
            input,
            output,
            map: MapSignature::new(input, output),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScalarKind {
    Int,
    Float,
}

impl ScalarKind {
    fn from_input_bits(bits: u8) -> Self {
        if (bits & 1) != 0 {
            Self::Float
        } else {
            Self::Int
        }
    }

    fn from_output_bits(bits: u8) -> Self {
        if (bits & (1 << 2)) != 0 {
            Self::Float
        } else {
            Self::Int
        }
    }

    unsafe fn read_elem(self, elem_ptr: *const u8) -> ScalarArg {
        match self {
            Self::Int => ScalarArg::Int(unsafe { read_int(elem_ptr) }),
            Self::Float => ScalarArg::Float(unsafe { read_float(elem_ptr) }),
        }
    }

    fn unpack(self, bits: u64) -> ScalarArg {
        match self {
            Self::Int => ScalarArg::Int(bits.cast_signed()),
            Self::Float => ScalarArg::Float(f64::from_bits(bits)),
        }
    }
}

#[derive(Clone, Copy)]
enum ScalarArg {
    Int(i64),
    Float(f64),
}

#[derive(Clone, Copy)]
enum MapSignature {
    IntToInt,
    IntToFloat,
    FloatToInt,
    FloatToFloat,
}

impl MapSignature {
    fn new(input: ScalarKind, output: ScalarKind) -> Self {
        match (input, output) {
            (ScalarKind::Int, ScalarKind::Int) => Self::IntToInt,
            (ScalarKind::Int, ScalarKind::Float) => Self::IntToFloat,
            (ScalarKind::Float, ScalarKind::Int) => Self::FloatToInt,
            (ScalarKind::Float, ScalarKind::Float) => Self::FloatToFloat,
        }
    }

    unsafe fn call_unary(self, fn_addr: usize, env: *mut u8, arg: ScalarArg) -> u64 {
        match (self, arg) {
            (Self::IntToInt, ScalarArg::Int(value)) => {
                call_unary_callback!(fn_addr, env, value, MapIntToIntFn, pack_int)
            }
            (Self::IntToFloat, ScalarArg::Int(value)) => {
                call_unary_callback!(fn_addr, env, value, MapIntToFloatFn, pack_float)
            }
            (Self::FloatToInt, ScalarArg::Float(value)) => {
                call_unary_callback!(fn_addr, env, value, MapFloatToIntFn, pack_int)
            }
            (Self::FloatToFloat, ScalarArg::Float(value)) => {
                call_unary_callback!(fn_addr, env, value, MapFloatToFloatFn, pack_float)
            }
            _ => unreachable!("callback signature/input mismatch"),
        }
    }

    unsafe fn call_binary(
        self,
        fn_addr: usize,
        env: *mut u8,
        lhs: ScalarArg,
        rhs: ScalarArg,
    ) -> u64 {
        match (self, lhs, rhs) {
            (Self::IntToInt, ScalarArg::Int(lhs), ScalarArg::Int(rhs)) => {
                call_binary_callback!(fn_addr, env, lhs, rhs, ReduceIntToIntFn, pack_int)
            }
            (Self::IntToFloat, ScalarArg::Int(lhs), ScalarArg::Int(rhs)) => {
                call_binary_callback!(fn_addr, env, lhs, rhs, ReduceIntToFloatFn, pack_float)
            }
            (Self::FloatToInt, ScalarArg::Float(lhs), ScalarArg::Float(rhs)) => {
                call_binary_callback!(fn_addr, env, lhs, rhs, ReduceFloatToIntFn, pack_int)
            }
            (Self::FloatToFloat, ScalarArg::Float(lhs), ScalarArg::Float(rhs)) => {
                call_binary_callback!(fn_addr, env, lhs, rhs, ReduceFloatToFloatFn, pack_float)
            }
            _ => unreachable!("callback signature/input mismatch"),
        }
    }
}

type ForEachIntFn = unsafe extern "C" fn(*mut u8, i64);
type ForEachFloatFn = unsafe extern "C" fn(*mut u8, f64);
type MapIntToIntFn = unsafe extern "C" fn(*mut u8, i64) -> i64;
type MapIntToFloatFn = unsafe extern "C" fn(*mut u8, i64) -> f64;
type MapFloatToIntFn = unsafe extern "C" fn(*mut u8, f64) -> i64;
type MapFloatToFloatFn = unsafe extern "C" fn(*mut u8, f64) -> f64;
type ReduceIntToIntFn = unsafe extern "C" fn(*mut u8, i64, i64) -> i64;
type ReduceIntToFloatFn = unsafe extern "C" fn(*mut u8, i64, i64) -> f64;
type ReduceFloatToIntFn = unsafe extern "C" fn(*mut u8, f64, f64) -> i64;
type ReduceFloatToFloatFn = unsafe extern "C" fn(*mut u8, f64, f64) -> f64;

fn pack_int(value: i64) -> u64 {
    value.cast_unsigned()
}

fn pack_float(value: f64) -> u64 {
    value.to_bits()
}

unsafe fn callback_fn<F>(fn_addr: usize) -> F {
    unsafe { std::mem::transmute_copy(&fn_addr) }
}

unsafe fn read_int(elem_ptr: *const u8) -> i64 {
    unsafe { *(elem_ptr.cast::<i64>()) }
}

unsafe fn read_float(elem_ptr: *const u8) -> f64 {
    unsafe { *(elem_ptr.cast::<f64>()) }
}
