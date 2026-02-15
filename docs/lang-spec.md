# Language Specification

## Table of Contents

1. [Introduction](#introduction)
2. [Design Philosophy](#design-philosophy)
3. [Lexical Structure](#lexical-structure)
4. [Types](#types)
5. [Variables and Constants](#variables-and-constants)
6. [Operators](#operators)
7. [Control Flow](#control-flow)
8. [Functions](#functions)
9. [Structs](#structs)
10. [Enums](#enums)
11. [Interfaces](#interfaces)
12. [Type System Features](#type-system-features)
13. [Error Handling](#error-handling)
14. [Memory Model](#memory-model)
15. [Concurrency](#concurrency)
16. [Modules and Visibility](#modules-and-visibility)
17. [Standard Library Overview](#standard-library-overview)

---

## Introduction

This language is designed to be a "lower-level Python"—combining Python's developer ergonomics with static typing, compilation, and predictable performance. It emphasizes explicitness, readability, and progressive complexity, allowing developers to write simple code by default while opting into advanced features when needed.

### Key Characteristics

- **Statically typed** with explicit type annotations
- **Compiled** for performance
- **Immutable by default** with opt-in mutability
- **No null pointer exceptions** through union types and optionals
- **Explicit error handling** through type narrowing
- **Reference counted** with opt-in borrow checking for performance-critical code
- **Modern concurrency** with async/await and channels

---

## Design Philosophy

### Explicitness Over Implicitness

Every variable declaration includes its type. There is no type inference that hides information from the reader.

```
// Good: Type is clear
user: User = getUser(42)

// Not supported: Inferred types
user := getUser(42)
```

### Readability Over Writability

Code is read far more often than it is written. The language optimizes for the reader's experience.

```
// Explicit type narrowing shows exactly what's happening
result: User = fetchUser(id) to {
    NotFound -> panic("user not found"),
    DbError(e) -> panic("database error: {e}")
}
```

### Progressive Complexity

Simple things are simple. Complex things are possible. Developers don't need to learn advanced features until they need them.

```
// Simple: Copy semantics (default)
fn process(user: User): String {
    return user.name
}

// Advanced: Borrow semantics (opt-in)
fn processFast(user: Ref[User, A]): Ref[String, A] {
    return user.name
}
```

### Immutability by Default

Variables are immutable unless explicitly marked as mutable. This prevents accidental mutation and makes code easier to reason about.

```
x: Int = 5        // immutable
y: Int! = 10      // mutable
y += 1            // ok
x += 1            // compiler error
```

---

## Lexical Structure

### Comments

````
// Single line comment

/*
   Multi-line comment
   Can span multiple lines
*/

/* Nested /* comments */ are allowed */

/// Doc comment for the following item
/// Supports markdown formatting

/**
 * Multi-line doc comment
 *
 * Example:
 * ```
 * result: Int = add(2, 3)
 * ```
 */
````

### Identifiers

Identifiers begin with a letter or underscore, followed by letters, digits, or underscores.

```
validName
_privateConvention
camelCase
PascalCase
snake_case
name2
```

### Keywords

```
// Declarations
fn, struct, enum, interface, type, alias, const, pub, module, using, import

// Control flow
if, else, match, loop, break, continue, return

// Iteration
each, via, to

// Operators
and, or, not, is, as

// Concurrency
async, await, spawn, block

// Special
bind, impl, extends, Null, true, false
```

### Literals

#### Integers

```
42              // decimal
9_000_000       // with separators
0xFF            // hexadecimal
0b1010          // binary
0o755           // octal
```

#### Floats

```
19.99
1.5e10
3.14159
```

#### Booleans

```
true
false
```

#### Characters

```
'A'             // character
'\n'            // newline
'\t'            // tab
'\\'            // backslash
'\''            // single quote
'\u{2764}'      // unicode code point
```

#### Strings

```
"hello"                     // basic string
"Hello, {name}"             // interpolation
"line one\nline two"        // escape sequences
```

#### Multiline Strings

```
"""
    This is a multiline string.
    Indentation is preserved relative to closing quotes.
    Interpolation {works} here too.
    """
```

#### Raw Strings

```
r"C:\Users\Alice"           // no escape processing
r"\d+\.\d+"                 // regex without escaping

r"""
    Raw multiline string
    \n is literal backslash-n
    {this} is literal braces
    """
```

#### Collections

```
[1, 2, 3]                   // List
[0..10]                     // List from range (0-9)
[0..=10]                    // List from range (0-10 inclusive)
[0..10:2]                   // List with step (0, 2, 4, 6, 8)

{"key": "value"}            // Map
{"apple", "banana"}         // Set
(10, 20)                    // Tuple
```

---

## Types

### Primitive Types

#### Integers

| Type     | Size   | Range              |
| -------- | ------ | ------------------ |
| `Int8`   | 8-bit  | -128 to 127        |
| `Int16`  | 16-bit | -32,768 to 32,767  |
| `Int32`  | 32-bit | -2³¹ to 2³¹-1      |
| `Int64`  | 64-bit | -2⁶³ to 2⁶³-1      |
| `Int`    | 64-bit | Alias for `Int64`  |
| `UInt8`  | 8-bit  | 0 to 255           |
| `UInt16` | 16-bit | 0 to 65,535        |
| `UInt32` | 32-bit | 0 to 2³²-1         |
| `UInt64` | 64-bit | 0 to 2⁶⁴-1         |
| `UInt`   | 64-bit | Alias for `UInt64` |

#### Floating Point

| Type      | Size   | Precision           |
| --------- | ------ | ------------------- |
| `Float32` | 32-bit | Single precision    |
| `Float64` | 64-bit | Double precision    |
| `Float`   | 64-bit | Alias for `Float64` |

#### Other Primitives

| Type   | Description                 |
| ------ | --------------------------- |
| `Bool` | `true` or `false`           |
| `Char` | Single Unicode scalar value |
| `Byte` | Alias for `UInt8`           |

### Text Types

#### String

UTF-8 encoded text. Immutable.

```
name: String = "Alice"
greeting: String = "Hello, {name}!"
```

##### Escape Sequences

| Sequence   | Meaning            |
| ---------- | ------------------ |
| `\n`       | Newline            |
| `\t`       | Tab                |
| `\r`       | Carriage return    |
| `\\`       | Backslash          |
| `\"`       | Double quote       |
| `\{`       | Literal brace      |
| `\u{XXXX}` | Unicode code point |

##### String Methods

```
s.length              // Int
s.isEmpty()           // Bool
s.contains("sub")     // Bool
s.startsWith("pre")   // Bool
s.endsWith("suf")     // Bool
s.toUpper()           // String
s.toLower()           // String
s.trim()              // String
s.split(",")          // List[String]
s.replace("a", "b")   // String
s.substring(0, 5)     // String | RangeError
s.indexOf("x")        // Int?
s.chars()             // List[Char]
```

### Collection Types

#### List[T]

Ordered, indexed collection of elements.

```
numbers: List[Int] = [1, 2, 3, 4, 5]
empty: List[String] = []
ranged: List[Int] = [0..10]

// Access (returns optional)
first: Int? = numbers[0]

// Mutation (requires mutable list)
numbers: List[Int]! = [1, 2, 3]
numbers[0] = 10
numbers.append(4)
```

#### Map[K, V]

Key-value pairs.

```
ages: Map[String, Int] = {
    "alice": 30,
    "bob": 25
}

// Access (returns optional)
age: Int? = ages["alice"]

// Mutation (requires mutable map)
ages: Map[String, Int]! = {"alice": 30}
ages["charlie"] = 35
```

#### Set[T]

Unique values, unordered.

```
tags: Set[String] = {"rust", "python", "go"}
hasPython: Bool = tags.contains("python")

// Mutation (requires mutable set)
tags: Set[String]! = {"rust"}
tags.add("python")
```

#### Tup[A, B, ...]

Fixed-size, mixed-type collection.

```
point: Tup[Int, Int] = (10, 20)
x: Int = point[0]
y: Int = point[1]

// Destructuring
(x, y): Tup[Int, Int] = point

// Mixed types
record: Tup[String, Int, Bool] = ("alice", 30, true)
```

### Special Types

#### Void

Represents no return value. Cannot be captured.

```
fn log(message: String): Void {
    print(message)
}

log("hello")              // ok
x: Void = log("hello")    // compiler error
```

#### Null

Represents absence of value. Used in optional types.

```
// Type? is sugar for Type | Null
name: String? = findName()
name: String | Null = findName()  // equivalent
```

#### Never

Represents functions that never return.

```
fn panic(message: String): Never {
    // crashes, never returns
}

fn gameLoop(): Never {
    loop {
        update()
        render()
    }
}
```

### Optional Types

The `?` suffix creates an optional type (union with Null).

```
// These are equivalent
user: User? = findUser("alice")
user: User | Null = findUser("alice")

// Working with optionals
name: String = user?.name ?? "anonymous"
```

### Union Types

Multiple possible types separated by `|`.

```
result: Int | String = getValue()
response: User | NotFound | DbError = fetchUser(id)
```

---

## Variables and Constants

### Variable Declaration

```
name: Type = value
```

All variables require explicit type annotation.

```
count: Int = 42
name: String = "Alice"
active: Bool = true
user: User = User { name: "Alice", email: "alice@example.com" }
```

### Mutability

Variables are immutable by default. Add `!` after the type for mutability.

```
// Immutable
x: Int = 5
x = 10          // compiler error

// Mutable
y: Int! = 5
y = 10          // ok
y += 1          // ok
```

### Constants

Compile-time constants at module level.

```
const MAX_USERS: Int = 10_000
const API_URL: String = "https://api.example.com"
const TIMEOUT_MS: Int = 5_000

// Public constant
pub const VERSION: String = "1.0.0"

// Constants can reference other constants
const BASE_TIMEOUT: Int = 1_000
const EXTENDED_TIMEOUT: Int = BASE_TIMEOUT * 5
```

### Type Aliases

#### alias (Interchangeable)

Creates an alternate name for an existing type. The alias and original type are completely interchangeable.

```
alias UserId = Int
alias Handler = (Request): Response
alias UserMap = Map[String, User]

id: UserId = 42
num: Int = id    // ok, same underlying type
```

#### type (Distinct)

Creates a new type that is not interchangeable with the underlying type.

```
type UserId = Int
type PostId = Int

fn getUser(id: UserId): User { ... }

userId: UserId = UserId(42)
postId: PostId = PostId(5)

getUser(postId)     // compiler error: PostId is not UserId
getUser(userId)     // ok

raw: Int = userId.value  // explicit unwrap
```

---

## Operators

### Arithmetic Operators

| Operator | Description      | Example |
| -------- | ---------------- | ------- |
| `+`      | Addition         | `a + b` |
| `-`      | Subtraction      | `a - b` |
| `*`      | Multiplication   | `a * b` |
| `/`      | Division         | `a / b` |
| `%`      | Modulo           | `a % b` |
| `-`      | Negation (unary) | `-a`    |

Division behavior is determined by operand types:

- `Int / Int` → `Int` (integer division)
- `Float / Float` → `Float`

For exponentiation, use `pow()` from the math library.

### Comparison Operators

| Operator | Description           | Example  |
| -------- | --------------------- | -------- |
| `==`     | Equality              | `a == b` |
| `!=`     | Inequality            | `a != b` |
| `<`      | Less than             | `a < b`  |
| `>`      | Greater than          | `a > b`  |
| `<=`     | Less than or equal    | `a <= b` |
| `>=`     | Greater than or equal | `a >= b` |

All comparison operators return `Bool`.

### Logical Operators

| Operator | Description | Example   |
| -------- | ----------- | --------- |
| `and`    | Logical AND | `a and b` |
| `or`     | Logical OR  | `a or b`  |
| `not`    | Logical NOT | `not a`   |

```
if user.isAdmin and user.isActive {
    // ...
}

if not user.isVerified or user.isBanned {
    // ...
}
```

### Bitwise Operators

| Operator | Description | Example  |
| -------- | ----------- | -------- |
| `&`      | Bitwise AND | `a & b`  |
| `\|`     | Bitwise OR  | `a \| b` |
| `^`      | Bitwise XOR | `a ^ b`  |
| `~`      | Bitwise NOT | `~a`     |
| `<<`     | Left shift  | `a << n` |
| `>>`     | Right shift | `a >> n` |

### Assignment Operators

| Operator | Description         | Example  |
| -------- | ------------------- | -------- |
| `=`      | Assignment          | `x = 5`  |
| `+=`     | Add and assign      | `x += 1` |
| `-=`     | Subtract and assign | `x -= 1` |
| `*=`     | Multiply and assign | `x *= 2` |
| `/=`     | Divide and assign   | `x /= 2` |

Assignment operators only work on mutable variables (`Type!`).

### Null/Optional Operators

| Operator | Description       | Example        |
| -------- | ----------------- | -------------- |
| `??`     | Null coalescing   | `a ?? default` |
| `?.`     | Optional chaining | `a?.field`     |
| `!`      | Unwrap/propagate  | `a!`           |

```
// Null coalescing
name: String = user.name ?? "anonymous"

// Optional chaining
email: String? = user?.profile?.email

// Combined
email: String = user?.profile?.email ?? "no email"

// Unwrap (propagates Null or error)
name: String = user!.name    // if user is Null, propagates
```

### Type Operators

| Operator | Description | Example     |
| -------- | ----------- | ----------- |
| `is`     | Type check  | `x is Type` |
| `as`     | Type cast   | `x as Type` |

```
if value is String {
    print(value.length)  // compiler knows value is String
}

number: Int = someValue as Int
```

### Range Operators

| Operator | Description     | Example               |
| -------- | --------------- | --------------------- |
| `..`     | Exclusive range | `0..10` (0-9)         |
| `..=`    | Inclusive range | `0..=10` (0-10)       |
| `:`      | Step (in range) | `0..10:2` (0,2,4,6,8) |

```
[0..10]       // [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
[0..=10]      // [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
[0..10:2]     // [0, 2, 4, 6, 8]
[10..0:-1]    // [10, 9, 8, 7, 6, 5, 4, 3, 2, 1]
```

### String Operators

| Operator | Description   | Example                   |
| -------- | ------------- | ------------------------- |
| `+`      | Concatenation | `"hello" + " " + "world"` |

Interpolation is preferred for complex string building:

```
greeting: String = "Hello, {name}! You have {count} messages."
```

---

## Control Flow

### If/Else

Standard conditional branching. No parentheses required around conditions. Braces always required.

#### Statement Form

```
if condition {
    doSomething()
}

if condition {
    doSomething()
} else if otherCondition {
    doOtherThing()
} else {
    fallback()
}
```

#### Expression Form

When used as an expression, all branches must return the same type and `else` is required.

```
result: String = if x > 0 { "positive" } else { "non-positive" }

message: String = if count == 0 {
    "none"
} else if count == 1 {
    "one"
} else {
    "many"
}
```

### Match

Pattern matching with exhaustive checking.

#### Basic Matching

```
message: String = match count {
    0 -> "none",
    1 -> "one",
    _ -> "many"
}
```

#### Enum Variants

```
action: Action = match status {
    Status.Pending -> Action.Wait,
    Status.Active(since) -> Action.Monitor(since),
    Status.Complete(result) -> Action.Archive(result)
}
```

#### Union Type Narrowing

```
username: String = match result {
    user: User -> user.name,
    NotFound -> "anonymous",
    DbError(e) -> panic("db failed: {e}")
}
```

#### Guards

```
category: String = match age {
    n if n < 0 -> panic("invalid age"),
    n if n < 13 -> "child",
    n if n < 20 -> "teen",
    n if n < 65 -> "adult",
    _ -> "senior"
}
```

Match expressions must be exhaustive. The compiler enforces that all possible cases are handled.

### Loop

Infinite loop with explicit control flow.

```
loop {
    data: Data = poll()

    if data.isEmpty() {
        continue
    }

    if data.isEnd() {
        break
    }

    process(data)
}
```

### Each

Iteration for side effects.

```
items each (item: Item): Void {
    print(item)
}

// With enumeration
items.enumerate() each (pair: Tup[Int, Item]): Void {
    print("{pair[0]}: {pair[1]}")
}

// Range iteration
[0..5] each (i: Int): Void {
    print(i)
}
```

### Via

Applies a function to a value. When used with `each`, transforms each element.
When used standalone, applies to the whole value (pipeline composition).

```
// Transform each element (each via)
doubled: List[Int] = numbers each via (n: Int): Int { n * 2 }

// Chain per-element transformations
result: List[String] = numbers
    each via (n: Int): Int { n * 2 }
    each via (n: Int): String { n.toString() }

// Pipeline: apply whole-collection operations (standalone via)
result: List[Int] = numbers
    via sort
    via take{count: 5}

// Combined: pipeline + per-element transforms
result: List[String] = numbers
    via sort
    each via (n: Int): Int { n * 2 }
    via take{count: 5}
    each via (n: Int): String { n.toString() }
```

### To

Type narrowing for union types. Handles error/variant cases explicitly.

```
// Type narrowing
user: User = fetchUser(id) to {
    NotFound -> panic("not found"),
    DbError(e) -> panic("db: {e}")
}

// Narrowing a union return
content: String = readFile(path) to {
    IoError(e) -> return e.message
}
```

---

## Functions

### Basic Function Syntax

```
fn functionName(param: Type, param: Type): ReturnType {
    // body
    return value
}
```

### Examples

```
// Simple function
fn greet(name: String): String {
    return "Hello, {name}!"
}

// Multiple parameters
fn add(a: Int, b: Int): Int {
    return a + b
}

// No return value
fn log(message: String): Void {
    print(message)
}
```

### Return Semantics

Functions require an explicit `return` statement to return a value. A function without a `return` statement implicitly returns `Void`. If a function declares a non-`Void` return type, all code paths must contain a `return` statement; otherwise the compiler emits a "missing return" error.

```
// Correct: explicit return
fn double(x: Int): Int {
    return x * 2
}

// Correct: Void functions need no return
fn greet(): Void {
    print("hi")
}

// Error: missing `return` in function with return type `Int`
fn bad(): Int {
    42
}
```

> **Note:** Block expressions used in other contexts (if/match branches, variable bindings like `x: Int = { ... }`) still use their trailing expression as the block's value. The explicit-return rule applies only to function bodies.

### Union Return Types

Functions that can fail return union types.

```
fn divide(a: Int, b: Int): Int | DivisionError {
    if b == 0 {
        return DivisionError("division by zero")
    }
    return a / b
}

fn fetchUser(id: Int): User | NotFound | DbError {
    // ...
}
```

### Generic Functions

```
fn first[T](list: List[T]): T? {
    if list.isEmpty() {
        return Null
    }
    return list[0]
}

fn map[T, U](list: List[T], transform: (T): U): List[U] {
    // ...
}

// Multiple type parameters
fn zip[A, B](a: List[A], b: List[B]): List[Tup[A, B]] {
    // ...
}
```

### Function Types

Functions as parameters.

```
fn apply(value: Int, transform: (Int): Int): Int {
    return transform(value)
}

fn filter[T](list: List[T], predicate: (T): Bool): List[T] {
    // ...
}
```

### Inline Functions

Anonymous functions for callbacks and pipelines.

```
// Full syntax
numbers each via fn (x: Int): Int { x * 2 }

// Shorthand (omit fn keyword)
numbers each via (x: Int): Int { x * 2 }

// Multi-line
numbers each via (x: Int): Int {
    doubled: Int = x * 2
    return doubled + 1
}
```

### Partials

Create new functions by fixing some parameters.

```
fn greet(greeting: String, name: String): String {
    return "{greeting}, {name}!"
}

// Create partial by fixing greeting
fn sayHello = greet{greeting: "Hello"}
fn sayGoodbye = greet{greeting: "Goodbye"}

sayHello("Alice")    // "Hello, Alice!"
sayGoodbye("Bob")    // "Goodbye, Bob!"
```

Partials use keyword arguments for fixed values. Unfilled parameters become the partial's signature.

```
fn clamp(min: Int, value: Int, max: Int): Int {
    if value < min { return min }
    if value > max { return max }
    return value
}

fn clampPercent = clamp{min: 0, max: 100}

clampPercent(50)    // 50
clampPercent(150)   // 100
```

### Partials in Pipelines

```
data to validate{strict: true} to transform{config: appConfig}
```

The piped value fills the first unfilled parameter.

### Decorators

```
@logged
@cached{ttl: 60}
fn fetchUser(id: Int): User | NotFound {
    // ...
}
```

Decorators are higher-order functions that wrap the decorated function.

---

## Structs

Structs are pure data containers with no methods.

### Definition

```
struct User {
    id: Int
    name: String
    email: String
}
```

### Instantiation

```
user: User = User {
    id: 1,
    name: "Alice",
    email: "alice@example.com"
}
```

### Field Access

```
print(user.name)
```

### Mutation

Structs are immutable by default. For mutable structs:

```
user: User! = User { id: 1, name: "Alice", email: "alice@example.com" }
user.name = "Bob"
```

### Nested Structs

```
struct Address {
    street: String
    city: String
}

struct Person {
    name: String
    address: Address
}

person: Person = Person {
    name: "Alice",
    address: Address {
        street: "123 Main St",
        city: "Springfield"
    }
}
```

### Copying with Modification

```
original: User = User { id: 1, name: "Alice", email: "alice@example.com" }
modified: User = User { ...original, name: "Bob" }
```

---

## Enums

### Simple Enums

```
enum Direction {
    North
    South
    East
    West
}

dir: Direction = Direction.North

match dir {
    Direction.North -> print("going up"),
    Direction.South -> print("going down"),
    Direction.East -> print("going right"),
    Direction.West -> print("going left")
}
```

### Enums with Raw Values

```
enum HttpStatus: Int {
    Ok = 200
    Created = 201
    NotFound = 404
    ServerError = 500
}

status: HttpStatus = HttpStatus.Ok
code: Int = status.rawValue  // 200
```

### Enums with Associated Data

```
enum Result {
    Success(value: Int)
    Failure(message: String, code: Int)
}

result: Result = Result.Success(42)

match result {
    Result.Success(value) -> print("got {value}"),
    Result.Failure(message, code) -> print("error {code}: {message}")
}
```

### Mixed Variants

```
enum ApiResponse {
    Success(body: String)
    Redirect(url: String)
    NotFound
    ServerError(message: String, trace: String?)
}
```

### Enum Variants as Types

Individual variants can be used as types in unions.

```
fn handleResponse(r: ApiResponse): Page | ApiResponse.Error | ApiResponse.Loading {
    return match r {
        ApiResponse.Success(body) -> parsePage(body),
        e: ApiResponse.Error -> e,
        l: ApiResponse.Loading -> l
    }
}
```

### Enum Binding Interfaces

Enums can bind interfaces just like structs.

```
enum Color {
    Red
    Green
    Blue
    Custom(r: Int, g: Int, b: Int)
}

interface IToHex {
    fn toHex[T: Color](color: T): String
}

Color binds IToHex {
    fn toHex(color: Color): String {
        match color {
            Color.Red -> "#FF0000",
            Color.Green -> "#00FF00",
            Color.Blue -> "#0000FF",
            Color.Custom(r, g, b) -> "#{r.toHex()}{g.toHex()}{b.toHex()}"
        }
    }
}
```

---

## Interfaces

Interfaces define behavior contracts. They are collections of functions with generic constraints.

### Basic Interface

```
interface IPrintable {
    fn print[T: { name: String }](item: T): Void {
        print(item.name)
    }
}
```

### Interfaces with Required Functions

Functions without bodies must be implemented by binding types.

```
interface ISerializable {
    // Required (no default implementation)
    fn serialize[T: Self](item: T): String

    // Default implementation using required function
    fn serializePretty[T: Self](item: T): String {
        return "pretty: {item.serialize()}"
    }
}
```

### Structural Constraints

#### Named Constraint

```
fn getId[T: User](entity: T): Int {
    return entity.id
}
```

#### Anonymous Structural Constraint

```
fn getId[T: { id: Int }](entity: T): Int {
    return entity.id
}

fn sendEmail[T: { email: String, name: String }](target: T, subject: String): Void {
    // send to target.email
}
```

### Binding Interfaces to Types

#### Simple Bind

```
struct User {
    id: Int
    name: String
}

User binds IPrintable
```

#### Bind with Implementations

```
User binds ISerializable {
    fn serialize(user: User): String {
        return "User({user.id}, {user.name})"
    }
}
```

#### Multiple Bindings

```
User binds [IPrintable, ISerializable {
    fn serialize(user: User): String {
        return "User({user.id})"
    }
}]
```

### Interface Inheritance

```
interface IUser {
    fn printUser[T: User](user: T): Void {
        print("User[{user.id}]: {user.email}")
    }
}

interface IAdminUser extends IUser {
    fn printUser[T: AdminUser](user: T): Void {
        print("AdminUser[{user.id}]: {user.email} ({user.role})")
    }

    fn getPermissions[T: AdminUser](user: T): List[String] {
        return user.permissions
    }
}
```

### Generic Functions with Interface Constraints

```
fn displayAll[T: IDisplayable](items: List[T]): Void {
    items each (item: T): Void {
        item.display()
    }
}
```

---

## Type System Features

### Union Types

Multiple possible types.

```
result: Int | String = getValue()
response: User | NotFound | DbError = fetchUser(id)
```

### Type Narrowing with `to`

```
// Narrow union to single type
user: User = fetchUser(id) to {
    NotFound -> panic("not found"),
    DbError(e) -> panic("db: {e}")
}

// Narrow to subset union
result: User | NotFound = fetchUser(id) to {
    DbError(e) -> panic("db: {e}")
}
```

### The `!` Operator

Unwraps a union type, propagating non-target variants.

```
fn getUsername(id: Int): String | Null | NotFound | DbError {
    user: User = fetchUser(id)!    // propagates NotFound | DbError
    name: String = user.name!      // propagates Null
    return name
}
```

The function's return type must include all propagated variants.

### Optionals

`Type?` is sugar for `Type | Null`.

```
// Equivalent declarations
user: User? = findUser("alice")
user: User | Null = findUser("alice")

// Optional chaining
email: String? = user?.profile?.email

// Null coalescing
email: String = user?.profile?.email ?? "no email"
```

### Type Conversions

No implicit conversions. All conversions are explicit.

```
i: Int = 42
f: Float = i.toFloat()
s: String = i.toString()

s: String = "123"
i: Int = s.toInt()!  // returns Int | ParseError

c: Char = 'A'
code: Int = c.toInt()
back: Char = code.toChar()
```

---

## Error Handling

### Philosophy

Errors are values, not exceptions. Functions that can fail return union types. The caller must handle all cases.

### Returning Errors

```
enum ValidationError {
    EmptyName
    InvalidEmail(reason: String)
    TooYoung(age: Int)
}

fn validateUser(name: String, email: String, age: Int): User | ValidationError {
    if name.isEmpty() {
        return ValidationError.EmptyName
    }
    if not email.contains("@") {
        return ValidationError.InvalidEmail("missing @")
    }
    if age < 18 {
        return ValidationError.TooYoung(age)
    }
    return User { name: name, email: email, age: age }
}
```

### Handling Errors with `to`

```
user: User = validateUser(name, email, age) to {
    ValidationError.EmptyName -> panic("name required"),
    ValidationError.InvalidEmail(r) -> panic("bad email: {r}"),
    ValidationError.TooYoung(a) -> panic("must be 18, got {a}")
}
```

### Propagating Errors with `!`

```
fn createAccount(name: String, email: String, age: Int): Account | ValidationError | DbError {
    user: User = validateUser(name, email, age)!  // propagates ValidationError
    account: Account = saveUser(user)!            // propagates DbError
    return account
}
```

### Providing Defaults

```
// With null coalescing
config: Config = loadConfig() ?? Config.default()

// With to block
config: Config = loadConfig() to {
    NotFound -> Config.default(),
    ParseError(e) -> {
        log("parse error: {e}, using default")
        Config.default()
    }
}
```

### Handling Multiple Error Types

```
fn processUser(id: Int): ProcessedUser | NotFound | DbError | ValidationError {
    user: User = fetchUser(id)!                    // NotFound | DbError
    validated: ValidUser = validate(user)!         // ValidationError
    return ProcessedUser.from(validated)
}

// Caller handles all three
result: ProcessedUser = processUser(42) to {
    NotFound -> panic("user not found"),
    DbError(e) -> panic("database error: {e}"),
    ValidationError(e) -> panic("validation failed: {e}")
}
```

---

## Memory Model

### Overview

The language uses **reference counting** by default for automatic memory management, with **opt-in borrow checking** for performance-critical code.

### Default Behavior: Copy Semantics

By default, data is copied when passed to functions.

```
fn process(user: User): String {
    return user.name
}

user: User = User { name: "Alice" }
process(user)        // user is copied into function
print(user.name)     // still valid, we have our copy
```

For primitives and small structs, copying is cheap and requires no mental overhead.

### Explicit References with `Ref`

For performance-critical code, opt into borrowing with `Ref`.

```
// Immutable borrow
fn processFast(user: Ref[User, A]): String {
    return user.name
}

// Mutable borrow
fn updateName(user: Ref[User, A]!, name: String): Void {
    user.name = name
}
```

### Lifetime Identifiers

The second parameter in `Ref` is a lifetime identifier. Same letter = same lifetime.

```
// Returned reference lives as long as input reference
fn getFirstName(user: Ref[User, A]): Ref[String, A] {
    return user.name
}

// Multiple inputs, same lifetime
fn longest(a: Ref[String, A], b: Ref[String, A]): Ref[String, A] {
    if a.length > b.length { return a }
    return b
}

// Different lifetimes (independent)
fn independent(a: Ref[String, A], b: Ref[String, B]): Int {
    return a.length + b.length
}
```

### Creating Local References

```
fn process(): Void {
    user: User = User { name: "Alice" }

    // Immutable reference
    ref: Ref[User] = user
    print(ref.name)        // ok
    ref.name = "Bob"       // compiler error

    // Mutable reference
    mutRef: Ref[User]! = user
    mutRef.name = "Bob"    // ok
}
```

### Borrowing Rules

```
user: User! = User { name: "Alice" }

// Multiple immutable borrows: OK
ref1: Ref[User] = user
ref2: Ref[User] = user
print(ref1.name)
print(ref2.name)

// Multiple mutable borrows: ERROR
ref1: Ref[User]! = user
ref2: Ref[User]! = user  // compiler error: already mutably borrowed

// Mixed borrows: ERROR
ref1: Ref[User] = user
ref2: Ref[User]! = user  // compiler error: already immutably borrowed
```

### When to Use What

```
// Simple case: just copy (default, easy)
fn formatUser(user: User): String {
    return "{user.name} <{user.email}>"
}

// Performance critical: borrow to avoid copy
fn formatUserFast(user: Ref[User, A]): String {
    return "{user.name} <{user.email}>"
}

// Need to return part of input: explicit lifetime
fn getName(user: Ref[User, A]): Ref[String, A] {
    return user.name
}

// Need to mutate: mutable reference
fn updateName(user: Ref[User, A]!, name: String): Void {
    user.name = name
}
```

### Weak References

For breaking reference cycles:

```
cache: Weak[User] = Weak(user)
maybeUser: User? = cache.get()
```

### Explicit Copying

```
original: User = User { name: "Alice" }
copied: User = original.copy()
```

---

## Concurrency

### Async/Await

For I/O-bound concurrent tasks.

#### Async Functions

```
async fn fetchUser(id: Int): User | HttpError {
    response: Response = await http.get("/users/{id}")
    return response.json()!
}
```

#### Awaiting Results

```
async fn main(): Void {
    user: User = await fetchUser(42)!
    print(user.name)
}
```

#### Parallel Await

```
async fn fetchUserWithPosts(id: Int): UserData | HttpError {
    (user, posts) = await all(
        fetchUser(id),
        fetchPosts(id)
    )!
    return UserData { user: user, posts: posts }
}
```

#### Running Async from Sync Context

```
fn syncMain(): Void {
    user: User = block fetchUser(42)!
    print(user.name)
}
```

The `block` keyword runs an async function synchronously.

### Spawn for Parallelism

True multi-threaded parallelism.

```
handle: Future[Int] = spawn {
    return heavyComputation()
}

// Do other work...

// Wait for result
result: Int = handle.join()!
```

### Channels

Communication between threads.

```
ch: Channel[String] = Channel()

spawn {
    ch.send("hello")
    ch.send("world")
    ch.close()
}

// Receive single value
msg: String = ch.receive()!

// Iterate until closed
ch each (msg: String): Void {
    print(msg)
}

// Buffered channel
ch: Channel[Int] = Channel(size: 10)
```

### Select

Wait on multiple channels.

```
ch1: Channel[String] = Channel()
ch2: Channel[Int] = Channel()

match select(ch1, ch2) {
    Select.First(msg: String) -> print("ch1: {msg}"),
    Select.Second(num: Int) -> print("ch2: {num}"),
    Select.Closed -> print("a channel closed")
}

// With timeout
match select(ch1, ch2, timeout: 5000) {
    Select.First(msg) -> print(msg),
    Select.Second(num) -> print(num),
    Select.Timeout -> print("timed out")
}
```

### Mutex

For shared mutable state.

```
counter: Mutex[Int] = Mutex(0)

spawn {
    counter.update((n: Int): Int { n + 1 })
}

// Or with lock
counter.lock((value: Int!): Void {
    value += 1
})

finalCount: Int = counter.get()
```

### Thread Pools

Managed parallelism for many tasks.

```
pool: ThreadPool = ThreadPool(workers: 4)

results: List[Handle[Int]] = [0..100] each via (i: Int): Handle[Int] {
    pool.submit {
        return heavyWork(i)
    }
}

// Gather results
values: List[Int] = results each via (h: Handle[Int]): Int {
    h.join()!
}
```

---

## Modules and Visibility

### Module Declaration

The `module` keyword declares a module. The file must be named to match the module.

```
// user/user.lang
module user

struct User {
    id: Int
    name: String
}

fn createUser(name: String): User {
    return User { id: generateId(), name: name }
}
```

### Visibility

Everything is private by default. Use `pub` to export.

```
module user

// Private (only accessible within module)
fn generateId(): Int {
    // ...
}

// Public (accessible from outside)
pub struct User {
    id: Int
    name: String
}

pub fn createUser(name: String): User {
    return User { id: generateId(), name: name }
}
```

### Using (Joining a Module)

Files can join an existing module with `using`.

```
// user/validation.lang
using user

// Can access private items from user module
fn validateUser(user: User): Bool {
    return user.id > 0 and not user.name.isEmpty()
}
```

### Importing

Import from other modules.

```
// Import whole module
import utils

// Import submodule
import utils.helpers

// Import specific items
import user { User, createUser }
```

Import paths are relative to project root.

### Module Structure

```
project/
├── user/
│   ├── user.lang        // module user
│   ├── validation.lang  // using user
│   └── auth/
│       ├── auth.lang    // module auth
│       └── tokens.lang  // using auth
└── utils/
    └── utils.lang       // module utils
```

### Rules

1. `module` declares a new module (filename must match)
2. `using` joins an existing module from same directory
3. Child modules cannot depend on parent modules
4. All files in a module share visibility
5. `pub` exports items outside the module
6. Import paths are relative to project root

### Re-exports

Modules can re-export from children.

```
// user/user.lang
module user

pub import auth { authenticate }  // re-export from child
```

---

## Standard Library Overview

### Core Interfaces

#### ISequence

For iterable types.

```
interface ISequence {
    fn next[T: Self](seq: T): Tup[Item, T]?

    fn each[T: Self](seq: T, action: (Item): Void): Void { ... }
    fn enumerate[T: Self](seq: T): ISequence[Tup[Int, Item]] { ... }
    fn take[T: Self](seq: T, count: Int): List[Item] { ... }
    fn filter[T: Self](seq: T, predicate: (Item): Bool): ISequence[Item] { ... }
}

List binds ISequence
Map binds ISequence
Set binds ISequence
```

### Math

```
import math

result: Float = math.pow(2.0, 10.0)
root: Float = math.sqrt(16.0)
absolute: Int = math.abs(-5)
```

### I/O

```
import io

// Print
io.print("Hello, world!")
io.println("With newline")

// File operations
content: String = io.readFile("path.txt")!
io.writeFile("path.txt", content)!
```

### HTTP (Async)

```
import http

async fn fetchData(): Data | HttpError {
    response: Response = await http.get("https://api.example.com/data")
    return response.json()!
}
```

### Collections

Standard methods on collection types:

```
// List
list.length
list.isEmpty()
list.append(item)
list.prepend(item)
list.concat(other)
list.reverse()
list.sort()
list.contains(item)

// Map
map.keys()
map.values()
map.entries()
map.contains(key)
map.remove(key)

// Set
set.add(item)
set.remove(item)
set.contains(item)
set.union(other)
set.intersection(other)
```

### Concurrency Primitives

```
// Channels
Channel[T]
Channel[T](size: Int)  // buffered
ch.send(value)
ch.receive()
ch.close()

// Mutex
Mutex[T]
mutex.lock(fn)
mutex.get()
mutex.update(fn)

// Thread pools
ThreadPool(workers: Int)
pool.submit(fn)

// Handles
Handle[T]
handle.join()
```

---

## Appendix: Complete Grammar Summary

### Declarations

```
// Variables
name: Type = value
name: Type! = value              // mutable

// Constants
const NAME: Type = value
pub const NAME: Type = value

// Type alias
alias Name = Type
type Name = Type                 // distinct

// Functions
fn name(params): ReturnType { body }
async fn name(params): ReturnType { body }
pub fn name(params): ReturnType { body }

// Structs
struct Name { fields }
pub struct Name { fields }

// Enums
enum Name { variants }
enum Name: RawType { variants }
pub enum Name { variants }

// Interfaces
interface Name { functions }
interface Name extends Other { functions }

// Bindings
Type binds Interface
Type binds Interface { implementations }
Type binds [Interface1, Interface2 { ... }]

// Modules
module name
using name
import path
import path { items }
pub import path { items }
```

### Expressions

```
// Literals
42, 3.14, true, false, "string", 'c', [1,2,3], {"k":"v"}, (a,b)

// Operators
a + b, a - b, a * b, a / b, a % b
a == b, a != b, a < b, a > b, a <= b, a >= b
a and b, a or b, not a
a ?? b, a?.b, a!
a is Type, a as Type

// Control flow
if cond { } else { }
match value { patterns }
loop { }

// Iteration
collection each (item): Void { }
collection each via (item): T { }
value via function
value to { narrowing }

// Functions
fn (params): ReturnType { body }
(params): ReturnType { body }
fnName{partial: args}

// Async
await expr
block expr
spawn { }

// References
Ref[Type, Lifetime]
Ref[Type, Lifetime]!
```

### Types

```
// Primitives
Int, Int8, Int16, Int32, Int64
UInt, UInt8, UInt16, UInt32, UInt64
Float, Float32, Float64
Bool, Char, Byte, String

// Collections
List[T], Map[K,V], Set[T], Tup[A,B,...]

// Special
Void, Null, Never

// Modifiers
Type!           // mutable
Type?           // optional (sugar for Type | Null)
Type | Other    // union

// References
Ref[Type, A]    // immutable borrow
Ref[Type, A]!   // mutable borrow

// Function types
(ParamTypes): ReturnType

// Generics
Name[T], Name[T, U]
[T: Constraint]
[T: { field: Type }]
```
