# Known Issues

Codegen and runtime issues discovered during end-to-end testing.

---

## 1. Function calls inside string interpolation crash codegen

**Severity**: High
**Location**: `spark_codegen` — string interpolation emission

Embedding a function call directly inside `{...}` interpolation braces causes a Cranelift verifier error:

```
error: codegen error in function 'main': Verifier — call_indirect.i8 ... has an invalid controlling type i8
```

**Reproducer**:
```spark
fn double(n: Int): Int {
    return n * 2
}

fn main(): Int {
    print("result = {double(5)}")  // crashes
    return 0
}
```

**Workaround**: Assign the function result to a variable first:
```spark
    d: Int = double(5)
    print("result = {d}")  // works
```

**Likely cause**: The interpolation codegen emits the inner expression with the wrong return type (`i8` instead of the actual function return type). The `call_indirect` instruction's controlling type must be `i32` or `i64`, not `i8`.

---

## 2. String interpolation with Int/String variables produces empty output at runtime

**Severity**: High
**Location**: `spark_runtime` — `__spark_int_to_string` / string concat

Int and String variables interpolated via `"text {variable}"` produce empty strings at runtime (the surrounding text prints but the interpolated value is blank).

**Reproducer**:
```spark
fn main(): Int {
    name: String = "Spark"
    version: Int = 1
    print("Welcome to {name} v{version}!")
    // prints: "Welcome to  v!"
    return 0
}
```

**Workaround**: Use separate `print()` calls:
```spark
    print("Welcome to")
    print(name)
```

**Likely cause**: The runtime's `__spark_int_to_string` or `__spark_string_concat` functions may not be returning/linking correctly, or the codegen is passing arguments in the wrong order/format.


