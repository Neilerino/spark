# Known Issues

Codegen and parser issues discovered during end-to-end testing.

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

---

## 3. Multi-parameter generics don't parse in expression position

**Severity**: Medium
**Location**: `spark_parser` — expression parsing

The parser treats `[` in expression position as an index operator, so `Pair[A, B] { ... }` is parsed as indexing `Pair` with `A`, followed by unexpected `,`.

**Reproducer**:
```spark
struct Pair[A, B] {
    first: A,
    second: B
}

fn main(): Int {
    p: Pair[Int, String] = Pair[Int, String] { first: 1, second: "hi" }
    // parse error: expected ], found ,
    return 0
}
```

**Workaround**: Use single-parameter generics, or rely on type inference from the binding annotation:
```spark
    p: Pair[Int, String] = Pair { first: 1, second: "hi" }
```

**Note**: Multi-parameter generics work fine in type annotations (e.g., `p: Pair[Int, String]`), just not in expression position where the struct name is followed by `[`.

---

## 4. Struct passed as function argument reads wrong field values

**Severity**: High
**Location**: `spark_codegen` — struct parameter ABI

When a struct is passed as a function parameter, field accesses inside the called function read garbage values.

**Reproducer**:
```spark
struct Point {
    x: Int,
    y: Int
}

fn distance_squared(p: Point): Int {
    return p.x * p.x + p.y * p.y
}

fn main(): Int {
    origin: Point = Point { x: 3, y: 4 }
    print(distance_squared(origin))
    // prints: 6430625257292831808 (expected: 25)
    return 0
}
```

**Workaround**: Access struct fields in the same function where the struct is created, rather than passing it to another function.

**Likely cause**: Structs are aggregates passed by pointer. The callee may be reading from the wrong stack location, or the caller is passing a raw pointer while the callee expects a stack slot copy.

---

## 5. Generic function monomorphization crashes codegen

**Severity**: High
**Location**: `spark_codegen` — monomorphized function call emission

Calling a monomorphized generic function produces a Cranelift verifier error (same `call_indirect.i8` pattern as issue 1).

**Reproducer**:
```spark
fn identity[T](x: T): T {
    return x
}

fn main(): Int {
    print(identity(42))
    // codegen error: call_indirect.i8 has invalid controlling type
    return 0
}
```

**Workaround**: None currently.

**Likely cause**: The monomorphized function's signature may not be declared/imported correctly in the calling function's codegen context. The `call_indirect` controlling type defaults to `i8` instead of the actual monomorphized return type.

---

## 6. Loop with mutable accumulator crashes at runtime

**Severity**: Medium
**Location**: `spark_codegen` or `spark_runtime`

A loop that accumulates a sum with `+=` compiles but crashes at runtime (exit code 132 / SIGILL).

**Reproducer**:
```spark
fn main(): Int {
    sum: Int! = 0
    n: Int! = 1
    loop {
        if n > 10 {
            break
        }
        sum += n
        n += 1
    }
    print(sum)
    // crashes with exit code 132
    return 0
}
```

**Workaround**: None currently.

**Likely cause**: The `+=` compound assignment may be generating incorrect instructions, or the mutable variable update within the loop body is corrupting the stack frame.
