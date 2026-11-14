# Spark

Spark is a compiled, statically typed language. Bindings are immutable by default, and unions represent recoverable errors as values.

## Run from source

Use [Rustup](https://rustup.rs/) and a C compiler available as `cc`. On macOS, the Xcode Command Line Tools provide `cc`. On Linux, use GCC or Clang.

From the repository root, build the compiler and its matching runtime, then run the hello fixture.

```sh
cargo build -p spark_driver -p spark_runtime
SPARK_RUNTIME_DIR=target/debug target/debug/spark run --single-file crates/spark_driver/tests/run-pass/basics/hello.spark
```

The program prints `Hello, Spark!`. To build a native executable, replace `run` with `build` and add `-o <output>`.

## Install

From the repository root, run `./install.sh`. It builds a release compiler and runtime and installs them under `~/.local`. Pass a different prefix as the first argument to install elsewhere.

```sh
export PATH="$HOME/.local/bin:$PATH"
export SPARK_RUNTIME_DIR="$HOME/.local/lib"
```

From a directory containing your `day.spark`, run `spark run --single-file day.spark`. Omit `--single-file` when your program needs modules from the project. Add `--no-opt` to compare behavior with Spark's optional MIR optimizations disabled. Arguments after `--` are forwarded to your program.

For a local puzzle workflow, see the [Advent of Code quickstart](examples/advent-of-code/README.md).

See the [language reference](docs/lang-spec.md) for syntax and examples, and the [compiler guide](docs/compiler-architecture.md) for the pipeline and development checks.
