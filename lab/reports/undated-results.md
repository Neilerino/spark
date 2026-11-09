# Undated Spark and Rust benchmark snapshots

Historical Hyperfine tables from the repository. These runs have no recorded compiler revision or machine configuration. See the [benchmark lab](../README.md) to reproduce measurements on a current build.

## collatz

| Command | Mean [s] | Min [s] | Max [s] | Relative |
|:---|---:|---:|---:|---:|
| `Rust` | 1.101 ± 0.042 | 1.026 | 1.179 | 1.00 |
| `Spark` | 2.665 ± 0.129 | 2.465 | 2.957 | 2.42 ± 0.15 |

## fibonacci

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `Rust` | 433.9 ± 100.2 | 357.3 | 706.9 | 1.00 |
| `Spark` | 1214.4 ± 134.2 | 1123.0 | 1528.3 | 2.80 ± 0.72 |

## list-transform

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `Rust` | 23.7 ± 5.2 | 19.5 | 47.7 | 1.00 |
| `Spark` | 58.7 ± 8.2 | 49.0 | 92.2 | 2.47 ± 0.64 |

## parallel-sum

| Command | Mean [ms] | Min [ms] | Max [ms] | Relative |
|:---|---:|---:|---:|---:|
| `Rust` | 35.6 ± 137.6 | 5.7 | 975.4 | 1.00 |
| `Spark` | 42.4 ± 5.4 | 35.0 | 56.4 | 1.19 ± 4.61 |

## prime-check

| Command | Mean [s] | Min [s] | Max [s] | Relative |
|:---|---:|---:|---:|---:|
| `Rust` | 2.365 ± 0.492 | 1.799 | 3.230 | 1.00 |
| `Spark` | 2.972 ± 0.658 | 2.436 | 4.561 | 1.26 ± 0.38 |
