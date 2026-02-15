# Spark Cross-Language Benchmark Suite ("Lab")

Empirical performance comparison of **Python**, **Go**, **Rust**, and **Spark** on identical workloads, measured with [hyperfine](https://github.com/sharkdp/hyperfine).

## Prerequisites

The following must be on your `PATH`:

| Tool       | Install                          |
|------------|----------------------------------|
| `python3`  | System or `brew install python`  |
| `go`       | `brew install go`                |
| `rustc`    | `rustup` / `brew install rust`   |
| `spark`    | Spark compiler (this project)    |
| `hyperfine`| `brew install hyperfine`         |

## Usage

```bash
cd lab

# Run all benchmarks
./run.sh

# Run a single benchmark
./run.sh fibonacci

# Verify outputs match across languages (no timing)
./run.sh --verify
```

Results are saved to `lab/results/<benchmark>.md` as hyperfine markdown tables.

## Benchmarks

| Benchmark        | What it tests                                   | Expected output |
|------------------|-------------------------------------------------|-----------------|
| `fibonacci`      | Recursive function call overhead (`fib(40)`)    | `102334155`     |
| `prime-check`    | Tight loops, modulo, branches (primes to 2M)    | `148933`        |
| `collatz`        | Iterative computation (Collatz lengths, 1-5M)   | `740438133`     |
| `list-transform` | Heap allocation, collection pipelines (1M elems)| `461500000`     |
| `parallel-sum`   | Thread spawning, parallel map (10M elements)    | `4615000000`    |

## Adding a New Benchmark

1. Create a directory under `lab/benchmarks/<name>/`
2. Add `main.py`, `main.go`, `main.rs`, and `main.spark`
3. Ensure all four produce **identical stdout output**
4. Run `./run.sh <name>` to test it

That's it. The runner auto-discovers benchmark directories.

### Conventions

- File names are always `main.<ext>`
- Each program is self-contained (no external dependencies)
- Programs should run for 0.5-5 seconds for stable measurements
- All implementations must print identical output (verified before benchmarking)

## Sample Results

After running `./run.sh`, check `lab/results/` for tables like:

| Command | Mean [s] | Min [s] | Max [s] | Relative |
|:---|---:|---:|---:|---:|
| `Python` | 25.123 +/- 0.456 | 24.567 | 25.789 | 100.00 |
| `Go` | 0.456 +/- 0.012 | 0.440 | 0.472 | 55.15x |
| `Rust` | 0.234 +/- 0.005 | 0.228 | 0.241 | 107.36x |
| `Spark` | TBD | TBD | TBD | TBD |

*(Sample values - run the suite to get real numbers for your machine.)*
