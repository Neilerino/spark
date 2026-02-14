//! `spark.math` runtime implementations.
//!
//! Provides `extern "C"` functions for the `spark.math` standard library module.
//! Each function delegates to the corresponding `std::f64` method.

/// Returns the square root of `x`.
#[no_mangle]
pub extern "C" fn __spark_math_sqrt(x: f64) -> f64 {
    x.sqrt()
}

/// Returns `base` raised to the power `exp`.
#[no_mangle]
pub extern "C" fn __spark_math_pow(base: f64, exp: f64) -> f64 {
    base.powf(exp)
}

/// Returns the absolute value of `x`.
#[no_mangle]
pub extern "C" fn __spark_math_abs(x: f64) -> f64 {
    x.abs()
}

/// Returns the largest integer less than or equal to `x`.
#[no_mangle]
#[allow(clippy::cast_possible_truncation)]
pub extern "C" fn __spark_math_floor(x: f64) -> i64 {
    x.floor() as i64
}

/// Returns the smallest integer greater than or equal to `x`.
#[no_mangle]
#[allow(clippy::cast_possible_truncation)]
pub extern "C" fn __spark_math_ceil(x: f64) -> i64 {
    x.ceil() as i64
}

/// Returns the minimum of `a` and `b`.
#[no_mangle]
pub extern "C" fn __spark_math_min(a: f64, b: f64) -> f64 {
    a.min(b)
}

/// Returns the maximum of `a` and `b`.
#[no_mangle]
pub extern "C" fn __spark_math_max(a: f64, b: f64) -> f64 {
    a.max(b)
}

/// Returns `x` rounded to the nearest integer.
#[no_mangle]
#[allow(clippy::cast_possible_truncation)]
pub extern "C" fn __spark_math_round(x: f64) -> i64 {
    x.round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqrt_basic() {
        assert!(((__spark_math_sqrt(16.0)) - 4.0).abs() < f64::EPSILON);
        assert!(((__spark_math_sqrt(2.0)) - std::f64::consts::SQRT_2).abs() < f64::EPSILON);
    }

    #[test]
    fn pow_basic() {
        assert!(((__spark_math_pow(2.0, 10.0)) - 1024.0).abs() < f64::EPSILON);
        assert!(((__spark_math_pow(3.0, 0.0)) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn abs_basic() {
        assert!(((__spark_math_abs(-5.0)) - 5.0).abs() < f64::EPSILON);
        assert!(((__spark_math_abs(5.0)) - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn floor_basic() {
        assert_eq!(__spark_math_floor(3.7), 3);
        assert_eq!(__spark_math_floor(-3.2), -4);
    }

    #[test]
    fn ceil_basic() {
        assert_eq!(__spark_math_ceil(3.2), 4);
        assert_eq!(__spark_math_ceil(-3.7), -3);
    }

    #[test]
    fn min_max_basic() {
        assert!(((__spark_math_min(1.0, 2.0)) - 1.0).abs() < f64::EPSILON);
        assert!(((__spark_math_max(1.0, 2.0)) - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn round_basic() {
        assert_eq!(__spark_math_round(3.5), 4);
        assert_eq!(__spark_math_round(3.4), 3);
        assert_eq!(__spark_math_round(-3.5), -4);
    }
}
