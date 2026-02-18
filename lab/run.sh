#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BENCH_DIR="$SCRIPT_DIR/benchmarks"
RESULTS_DIR="$SCRIPT_DIR/results"

# Spark runtime — look in release first, then debug
if [[ -z "${SPARK_RUNTIME_DIR:-}" ]]; then
    if [[ -f "$PROJECT_ROOT/target/release/libspark_runtime.a" ]]; then
        export SPARK_RUNTIME_DIR="$PROJECT_ROOT/target/release"
    elif [[ -f "$PROJECT_ROOT/target/debug/libspark_runtime.a" ]]; then
        export SPARK_RUNTIME_DIR="$PROJECT_ROOT/target/debug"
    fi
fi

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

# Language filtering — all enabled by default
LANG_PYTHON=true
LANG_GO=true
LANG_RUST=true
LANG_SPARK=true

usage() {
    echo "Usage:"
    echo "  ./run.sh                          # Run all benchmarks, all languages"
    echo "  ./run.sh fibonacci                # Run one benchmark"
    echo "  ./run.sh --verify                 # Verify outputs match (no timing)"
    echo "  ./run.sh --only spark             # Run only Spark"
    echo "  ./run.sh --only rust,spark        # Run only Rust and Spark"
    echo "  ./run.sh --exclude python         # Skip Python"
    echo "  ./run.sh --exclude python,go      # Skip Python and Go"
    echo "  ./run.sh fibonacci --only spark   # Single benchmark, single language"
    echo ""
    echo "Languages: python, go, rust, spark"
    exit 1
}

log_info()  { echo -e "${BLUE}[info]${NC}  $*"; }
log_ok()    { echo -e "${GREEN}[ok]${NC}    $*"; }
log_warn()  { echo -e "${YELLOW}[warn]${NC}  $*"; }
log_error() { echo -e "${RED}[error]${NC} $*"; }
log_header(){ echo -e "\n${BOLD}=== $* ===${NC}\n"; }

# --- Parse a comma-separated language list into normalized names ---
parse_langs() {
    local input="$1"
    local -a langs=()
    IFS=',' read -ra parts <<< "$input"
    for part in "${parts[@]}"; do
        local lang
        lang="$(echo "$part" | tr '[:upper:]' '[:lower:]' | xargs)"
        case "$lang" in
            python|py) langs+=(python) ;;
            go|golang) langs+=(go) ;;
            rust|rs)   langs+=(rust) ;;
            spark)     langs+=(spark) ;;
            *)
                log_error "Unknown language: '$lang'"
                log_info  "Valid languages: python, go, rust, spark"
                exit 1
                ;;
        esac
    done
    echo "${langs[@]}"
}

# --- Check if a language is enabled ---
lang_enabled() {
    case "$1" in
        python) $LANG_PYTHON ;;
        go)     $LANG_GO ;;
        rust)   $LANG_RUST ;;
        spark)  $LANG_SPARK ;;
        *)      return 1 ;;
    esac
}

# --- Count enabled languages ---
count_enabled_langs() {
    local count=0
    $LANG_PYTHON && ((count++)) || true
    $LANG_GO     && ((count++)) || true
    $LANG_RUST   && ((count++)) || true
    $LANG_SPARK  && ((count++)) || true
    echo "$count"
}

# --- Show which languages are enabled ---
show_lang_filter() {
    local enabled=()
    $LANG_PYTHON && enabled+=(Python)
    $LANG_GO     && enabled+=(Go)
    $LANG_RUST   && enabled+=(Rust)
    $LANG_SPARK  && enabled+=(Spark)
    log_info "Languages: ${enabled[*]}"
}

# --- Prerequisite check ---
check_prereqs() {
    log_header "Checking prerequisites"
    local missing=()
    # Only check prereqs for enabled languages
    if $LANG_PYTHON; then
        if command -v python3 &>/dev/null; then
            log_ok "python3  $(command -v python3)"
        else
            log_error "python3  NOT FOUND"
            missing+=(python3)
        fi
    fi
    if $LANG_GO; then
        if command -v go &>/dev/null; then
            log_ok "go  $(command -v go)"
        else
            log_error "go  NOT FOUND"
            missing+=(go)
        fi
    fi
    if $LANG_RUST; then
        if command -v rustc &>/dev/null; then
            log_ok "rustc  $(command -v rustc)"
        else
            log_error "rustc  NOT FOUND"
            missing+=(rustc)
        fi
    fi
    if $LANG_SPARK; then
        if command -v spark &>/dev/null; then
            log_ok "spark  $(command -v spark)"
        else
            log_error "spark  NOT FOUND"
            missing+=(spark)
        fi
    fi
    if command -v hyperfine &>/dev/null; then
        log_ok "hyperfine  $(command -v hyperfine)"
    else
        log_error "hyperfine  NOT FOUND"
        missing+=(hyperfine)
    fi
    if [[ ${#missing[@]} -gt 0 ]]; then
        echo ""
        log_error "Missing tools: ${missing[*]}"
        log_info  "Install them and re-run."
        exit 1
    fi
}

# --- Compile a single benchmark ---
compile_benchmark() {
    local dir="$1"
    local name
    name="$(basename "$dir")"
    local build_dir="$dir/.build"
    mkdir -p "$build_dir"

    log_info "Compiling $name ..."

    # Rust
    if $LANG_RUST && [[ -f "$dir/main.rs" ]]; then
        rustc -O -o "$build_dir/main_rs" "$dir/main.rs" 2>&1 || {
            log_error "Rust compilation failed for $name"
            return 1
        }
    fi

    # Go
    if $LANG_GO && [[ -f "$dir/main.go" ]]; then
        go build -o "$build_dir/main_go" "$dir/main.go" 2>&1 || {
            log_error "Go compilation failed for $name"
            return 1
        }
    fi

    # Spark
    if $LANG_SPARK && [[ -f "$dir/main.spark" ]]; then
        spark build "$dir/main.spark" -o "$build_dir/main_spark" 2>&1 || {
            log_warn "Spark compilation failed for $name (skipping Spark)"
        }
    fi

    log_ok "Compiled $name"
}

# --- Verify outputs match ---
verify_benchmark() {
    local dir="$1"
    local name
    name="$(basename "$dir")"
    local build_dir="$dir/.build"
    local tmp_dir
    tmp_dir="$(mktemp -d)"

    log_info "Verifying $name ..."

    # Run each enabled implementation and capture output
    local has_spark=false
    $LANG_SPARK && [[ -f "$build_dir/main_spark" ]] && has_spark=true

    # Collect outputs for enabled languages
    local -a out_files=()
    local -a out_names=()

    if $LANG_PYTHON && [[ -f "$dir/main.py" ]]; then
        python3 "$dir/main.py" > "$tmp_dir/python.out" 2>&1
        out_files+=("$tmp_dir/python.out")
        out_names+=(Python)
    fi
    if $LANG_GO && [[ -f "$build_dir/main_go" ]]; then
        "$build_dir/main_go" > "$tmp_dir/go.out" 2>&1
        out_files+=("$tmp_dir/go.out")
        out_names+=(Go)
    fi
    if $LANG_RUST && [[ -f "$build_dir/main_rs" ]]; then
        "$build_dir/main_rs" > "$tmp_dir/rust.out" 2>&1
        out_files+=("$tmp_dir/rust.out")
        out_names+=(Rust)
    fi
    if $has_spark; then
        "$build_dir/main_spark" > "$tmp_dir/spark.out" 2>&1
        out_files+=("$tmp_dir/spark.out")
        out_names+=(Spark)
    fi

    # Compare all outputs against the first one as reference
    local failed=false
    if [[ ${#out_files[@]} -ge 2 ]]; then
        local ref_file="${out_files[0]}"
        local ref_name="${out_names[0]}"
        for i in $(seq 1 $(( ${#out_files[@]} - 1 ))); do
            if ! diff -q "$ref_file" "${out_files[$i]}" &>/dev/null; then
                log_error "$name: ${ref_name} vs ${out_names[$i]} output MISMATCH"
                diff "$ref_file" "${out_files[$i]}" || true
                failed=true
            fi
        done
    fi

    rm -rf "$tmp_dir"

    if $failed; then
        return 1
    fi
    if [[ ${#out_files[@]} -ge 2 ]]; then
        log_ok "Verified $name — all outputs match"
    else
        log_ok "Verified $name — ran successfully"
    fi
}

# --- Run benchmark with hyperfine ---
run_benchmark() {
    local dir="$1"
    local name
    name="$(basename "$dir")"
    local build_dir="$dir/.build"
    local has_spark=false
    $LANG_SPARK && [[ -f "$build_dir/main_spark" ]] && has_spark=true

    log_header "Benchmarking: $name"

    local cmds=()
    if $LANG_PYTHON && [[ -f "$dir/main.py" ]]; then
        cmds+=(-n "Python" "python3 $dir/main.py")
    fi
    if $LANG_GO && [[ -f "$build_dir/main_go" ]]; then
        cmds+=(-n "Go" "$build_dir/main_go")
    fi
    if $LANG_RUST && [[ -f "$build_dir/main_rs" ]]; then
        cmds+=(-n "Rust" "$build_dir/main_rs")
    fi
    if $has_spark; then
        cmds+=(-n "Spark" "$build_dir/main_spark")
    fi

    if [[ ${#cmds[@]} -eq 0 ]]; then
        log_warn "No implementations available for $name — skipping"
        return 0
    fi

    hyperfine --warmup 3 --runs 20 \
        "${cmds[@]}" \
        --export-markdown "$RESULTS_DIR/${name}.md" \
        --export-json "$RESULTS_DIR/${name}.json"

    log_ok "Results saved to results/${name}.md"
}

# --- Cleanup build artifacts ---
cleanup_benchmark() {
    local dir="$1"
    rm -rf "$dir/.build"
}

# --- Discover all benchmark directories ---
discover_benchmarks() {
    local filter="${1:-}"
    local dirs=()
    for d in "$BENCH_DIR"/*/; do
        [[ -d "$d" ]] || continue
        local name
        name="$(basename "$d")"
        if [[ -n "$filter" && "$name" != "$filter" ]]; then
            continue
        fi
        dirs+=("$d")
    done
    if [[ ${#dirs[@]} -eq 0 ]]; then
        log_error "No benchmarks found${filter:+ matching '$filter'}"
        exit 1
    fi
    echo "${dirs[@]}"
}

# --- Main ---
main() {
    local verify_only=false
    local filter=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --verify) verify_only=true; shift ;;
            --only|-l)
                # Disable all, then enable specified
                LANG_PYTHON=false; LANG_GO=false; LANG_RUST=false; LANG_SPARK=false
                local -a selected
                read -ra selected <<< "$(parse_langs "$2")"
                for lang in "${selected[@]}"; do
                    case "$lang" in
                        python) LANG_PYTHON=true ;;
                        go)     LANG_GO=true ;;
                        rust)   LANG_RUST=true ;;
                        spark)  LANG_SPARK=true ;;
                    esac
                done
                shift 2
                ;;
            --exclude|-x)
                local -a excluded
                read -ra excluded <<< "$(parse_langs "$2")"
                for lang in "${excluded[@]}"; do
                    case "$lang" in
                        python) LANG_PYTHON=false ;;
                        go)     LANG_GO=false ;;
                        rust)   LANG_RUST=false ;;
                        spark)  LANG_SPARK=false ;;
                    esac
                done
                shift 2
                ;;
            --help|-h) usage ;;
            -*) log_error "Unknown option: $1"; usage ;;
            *) filter="$1"; shift ;;
        esac
    done

    local enabled_count
    enabled_count="$(count_enabled_langs)"
    if [[ "$enabled_count" -eq 0 ]]; then
        log_error "No languages enabled — nothing to do"
        exit 1
    fi

    show_lang_filter
    check_prereqs

    local benchmarks
    read -ra benchmarks <<< "$(discover_benchmarks "$filter")"

    mkdir -p "$RESULTS_DIR"

    # Compile all
    log_header "Compilation"
    for dir in "${benchmarks[@]}"; do
        compile_benchmark "$dir"
    done

    # Verify all
    log_header "Verification"
    local verify_failed=false
    for dir in "${benchmarks[@]}"; do
        verify_benchmark "$dir" || verify_failed=true
    done

    if $verify_failed; then
        log_error "Verification failed — fix output mismatches before benchmarking"
        # Cleanup
        for dir in "${benchmarks[@]}"; do
            cleanup_benchmark "$dir"
        done
        exit 1
    fi

    if $verify_only; then
        log_ok "All verifications passed!"
        # Cleanup
        for dir in "${benchmarks[@]}"; do
            cleanup_benchmark "$dir"
        done
        exit 0
    fi

    # Benchmark all
    for dir in "${benchmarks[@]}"; do
        run_benchmark "$dir"
    done

    # Generate report
    log_header "Report"
    REPORT_DATE="$(date +%Y-%m-%d)"
    REPORT_DIR="$SCRIPT_DIR/reports/$REPORT_DATE"
    # If directory exists, append a counter
    if [[ -d "$REPORT_DIR" ]]; then
        counter=2
        while [[ -d "${REPORT_DIR}-${counter}" ]]; do
            ((counter++))
        done
        REPORT_DIR="${REPORT_DIR}-${counter}"
    fi
    if python3 "$SCRIPT_DIR/report.py" "$REPORT_DIR" "$RESULTS_DIR"/*.json 2>&1; then
        log_ok "Report generated: $REPORT_DIR/report.md"
    else
        log_warn "Report generation failed (results still saved as markdown)"
    fi

    # Cleanup
    log_header "Cleanup"
    for dir in "${benchmarks[@]}"; do
        cleanup_benchmark "$dir"
        log_ok "Cleaned $(basename "$dir")"
    done

    log_header "Done"
    log_ok "All results saved to lab/results/"
    log_ok "Report: $REPORT_DIR/report.md"
}

main "$@"
