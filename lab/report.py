#!/usr/bin/env python3
"""Generate benchmark reports with violin plots from hyperfine JSON data.

Usage:
    python3 report.py <output-dir> [--title "..."] <json-files...>
    python3 report.py <output-dir> --from-markdown <md-files...>
    python3 report.py --retro   # Generate retroactive reports from hardcoded historical data
"""

from __future__ import annotations

import argparse
import json
import os
import random
import sys
from pathlib import Path
from typing import Optional

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

# Consistent language colors across all plots
LANG_COLORS = {
    "Rust": "#E3722D",    # orange
    "Go": "#00ADD8",      # cyan
    "Spark": "#8B5CF6",   # purple
    "Python": "#3B8132",  # green
}
LANG_ORDER = ["Rust", "Go", "Spark", "Python"]


def parse_hyperfine_json(path: str) -> list[dict]:
    """Parse a hyperfine JSON export file. Returns list of benchmark entries."""
    with open(path) as f:
        data = json.load(f)
    return data["results"]


def parse_hyperfine_markdown(text: str) -> list[dict]:
    """Parse a hyperfine markdown table into synthetic benchmark entries.

    Since we only have mean/stddev/min/max, we generate 20 synthetic samples
    from a normal distribution clipped to [min, max].
    """
    entries = []
    for line in text.strip().splitlines():
        line = line.strip()
        if line.startswith("|:") or line.startswith("| Command") or line.startswith("|-"):
            continue
        if not line.startswith("|"):
            continue

        cols = [c.strip() for c in line.split("|")[1:-1]]
        if len(cols) < 5:
            continue

        command = cols[0].strip("`").strip()

        # Detect unit from header (parsed from context), but we can infer from values
        mean_str = cols[1]
        min_str = cols[2]
        max_str = cols[3]

        def parse_val(s):
            """Parse '292.6 +/- 15.7' or '0.868 +/- 0.003' style values."""
            s = s.strip()
            parts = s.split("±")
            val = float(parts[0].strip())
            stddev = float(parts[1].strip()) if len(parts) > 1 else 0.0
            return val, stddev

        mean, stddev = parse_val(mean_str)
        min_val = float(min_str)
        max_val = float(max_str)

        entries.append({
            "command": command,
            "mean": mean,
            "stddev": stddev,
            "min": min_val,
            "max": max_val,
        })

    return entries


def detect_unit_from_markdown(text: str) -> float:
    """Detect the time unit from a hyperfine markdown header. Returns multiplier to seconds."""
    for line in text.strip().splitlines():
        if "Mean [ms]" in line:
            return 0.001
        if "Mean [s]" in line:
            return 1.0
    return 1.0


def synthesize_times(mean: float, stddev: float, min_val: float, max_val: float,
                     n: int = 20) -> list[float]:
    """Generate synthetic run times from a normal distribution clipped to [min, max]."""
    rng = random.Random(hash((mean, stddev, min_val, max_val)))
    times = []
    for _ in range(n * 10):
        if len(times) >= n:
            break
        sample = rng.gauss(mean, stddev) if stddev > 0 else mean
        sample = max(min_val, min(max_val, sample))
        times.append(sample)
    # Fill any remaining with mean if we couldn't generate enough
    while len(times) < n:
        times.append(mean)
    return times[:n]


def format_time(seconds: float) -> str:
    """Format a time value in appropriate units."""
    if seconds < 0.001:
        return f"{seconds * 1_000_000:.0f}us"
    elif seconds < 1.0:
        return f"{seconds * 1000:.0f}ms"
    else:
        return f"{seconds:.3f}s"


def make_violin_plot(benchmark_name: str, lang_times: dict[str, list[float]],
                     output_path: str):
    """Generate a horizontal violin plot comparing languages for one benchmark."""
    # Order languages consistently
    ordered = [lang for lang in LANG_ORDER if lang in lang_times]

    fig, ax = plt.subplots(figsize=(10, max(2.5, len(ordered) * 0.9 + 1.0)))

    data = [lang_times[lang] for lang in ordered]
    colors = [LANG_COLORS.get(lang, "#888888") for lang in ordered]

    positions = list(range(1, len(ordered) + 1))

    if all(len(d) > 1 and np.std(d) > 0 for d in data):
        parts = ax.violinplot(data, positions=positions, vert=False, showmeans=True,
                              showmedians=False, showextrema=False)

        for i, pc in enumerate(parts["bodies"]):
            pc.set_facecolor(colors[i])
            pc.set_alpha(0.7)
            pc.set_edgecolor(colors[i])

        parts["cmeans"].set_color("black")
        parts["cmeans"].set_linewidth(1.5)
    else:
        # Fallback: just plot points
        for i, (lang, times) in enumerate(zip(ordered, data)):
            ax.scatter(times, [positions[i]] * len(times), color=colors[i],
                       alpha=0.6, s=20)
            ax.scatter([np.mean(times)], [positions[i]], color="black",
                       marker="|", s=100, linewidths=2)

    ax.set_yticks(positions)
    ax.set_yticklabels(ordered, fontsize=12)
    ax.set_xlabel("Time (seconds)", fontsize=11)
    ax.set_title(benchmark_name.replace("-", " ").title(), fontsize=14, fontweight="bold")
    ax.grid(axis="x", alpha=0.3)

    plt.tight_layout()
    plt.savefig(output_path, dpi=150, bbox_inches="tight")
    plt.close(fig)


def generate_report(output_dir: str, benchmarks: dict[str, dict[str, list[float]]],
                    title: Optional[str] = None):
    """Generate the full markdown report with violin plots.

    benchmarks: { benchmark_name: { language: [times_in_seconds] } }
    """
    os.makedirs(output_dir, exist_ok=True)

    if title is None:
        title = os.path.basename(output_dir)

    lines = [f"# Spark Benchmark Report — {title}\n"]

    # Summary table
    lines.append("## Summary\n")
    lines.append("| Benchmark | " + " | ".join(
        lang for lang in LANG_ORDER if any(lang in b for b in benchmarks.values())
    ) + " | Spark vs Rust |")

    active_langs = [lang for lang in LANG_ORDER
                    if any(lang in b for b in benchmarks.values())]
    lines.append("|" + "|".join(["---"] * (len(active_langs) + 2)) + "|")

    for bench_name, lang_times in benchmarks.items():
        row = [bench_name]
        rust_mean = np.mean(lang_times["Rust"]) if "Rust" in lang_times else None
        for lang in active_langs:
            if lang in lang_times:
                mean = np.mean(lang_times[lang])
                row.append(format_time(mean))
            else:
                row.append("—")
        # Spark vs Rust ratio
        if "Spark" in lang_times and rust_mean:
            spark_mean = np.mean(lang_times["Spark"])
            ratio = spark_mean / rust_mean
            row.append(f"{ratio:.2f}x")
        else:
            row.append("—")
        lines.append("| " + " | ".join(row) + " |")

    lines.append("")

    # Per-benchmark sections
    for bench_name, lang_times in benchmarks.items():
        png_name = f"{bench_name}.png"
        png_path = os.path.join(output_dir, png_name)

        make_violin_plot(bench_name, lang_times, png_path)

        lines.append(f"## {bench_name}\n")
        lines.append(f"![{bench_name}]({png_name})\n")

        # Data table
        rust_mean = np.mean(lang_times["Rust"]) if "Rust" in lang_times else None
        lines.append("| Language | Mean | Std Dev | Min | Max | vs Rust |")
        lines.append("|----------|------|---------|-----|-----|---------|")

        ordered = [lang for lang in LANG_ORDER if lang in lang_times]
        for lang in ordered:
            times = lang_times[lang]
            mean = np.mean(times)
            std = np.std(times)
            mn = np.min(times)
            mx = np.max(times)
            ratio = f"{mean / rust_mean:.2f}x" if rust_mean else "—"
            lines.append(
                f"| {lang} | {format_time(mean)} | ±{format_time(std)} "
                f"| {format_time(mn)} | {format_time(mx)} | {ratio} |"
            )
        lines.append("")

    report_path = os.path.join(output_dir, "report.md")
    with open(report_path, "w") as f:
        f.write("\n".join(lines) + "\n")

    print(f"Report: {report_path}")
    print(f"  {len(benchmarks)} benchmarks, {sum(1 for b in benchmarks.values() for _ in b)} plots")


def load_from_json_files(json_files: list[str]) -> dict[str, dict[str, list[float]]]:
    """Load benchmark data from hyperfine JSON export files."""
    benchmarks = {}
    for path in json_files:
        name = Path(path).stem
        results = parse_hyperfine_json(path)
        lang_times = {}
        for r in results:
            lang = r["command"]
            lang_times[lang] = r["times"]
        benchmarks[name] = lang_times
    return benchmarks


def load_from_markdown_files(md_files: list[str]) -> dict[str, dict[str, list[float]]]:
    """Load benchmark data from hyperfine markdown files, synthesizing run times."""
    benchmarks = {}
    for path in md_files:
        name = Path(path).stem
        with open(path) as f:
            text = f.read()
        multiplier = detect_unit_from_markdown(text)
        entries = parse_hyperfine_markdown(text)
        lang_times = {}
        for e in entries:
            mean_s = e["mean"] * multiplier
            std_s = e["stddev"] * multiplier
            min_s = e["min"] * multiplier
            max_s = e["max"] * multiplier
            lang_times[e["command"]] = synthesize_times(mean_s, std_s, min_s, max_s)
        benchmarks[name] = lang_times
    return benchmarks


# --- Retroactive report data (extracted from git history) ---

RETRO_REPORTS = {
    "2026-02-15-initial": {
        "title": "Initial Benchmarks",
        "benchmarks": {
            "fibonacci": {
                "unit": "s",
                "data": {
                    "Python": (43.143, 0.904, 42.357, 44.939),
                    "Go":     (0.574, 0.012, 0.557, 0.600),
                    "Rust":   (0.315, 0.017, 0.304, 0.362),
                    "Spark":  (2.498, 0.247, 2.299, 2.907),
                },
            },
            "collatz": {
                "unit": "s",
                "data": {
                    "Python": (158.463, 4.654, 150.486, 165.142),
                    "Go":     (1.864, 0.024, 1.832, 1.893),
                    "Rust":   (0.887, 0.033, 0.869, 0.980),
                    "Spark":  (12.382, 0.691, 12.031, 14.335),
                },
            },
            "parallel-sum": {
                "unit": "s",
                "data": {
                    "Python": (1.062, 0.046, 0.995, 1.141),
                    "Go":     (0.011, 0.002, 0.008, 0.020),
                    "Rust":   (0.007, 0.001, 0.006, 0.017),
                    "Spark":  (2.605, 0.060, 2.535, 2.742),
                },
            },
            "prime-check": {
                "unit": "s",
                "data": {
                    "Python": (21.506, 0.851, 20.903, 23.872),
                    "Go":     (1.670, 0.362, 1.535, 2.699),
                    "Rust":   (1.547, 0.046, 1.507, 1.644),
                    "Spark":  (1.585, 0.042, 1.536, 1.676),
                },
            },
            "list-transform": {
                "unit": "ms",
                "data": {
                    "Python": (431.7, 58.9, 375.7, 527.6),
                    "Go":     (19.2, 2.0, 16.2, 27.6),
                    "Rust":   (16.6, 1.7, 13.1, 21.0),
                    "Spark":  (900.9, 24.6, 862.2, 946.9),
                },
            },
        },
    },
    "2026-02-16-func-call-opts": {
        "title": "Function Calling Optimizations",
        "benchmarks": {
            "fibonacci": {
                "unit": "ms",
                "data": {
                    "Rust":  (295.7, 4.9, 285.9, 302.6),
                    "Spark": (967.7, 47.8, 908.5, 1074.6),
                },
            },
            "collatz": {
                "unit": "s",
                "data": {
                    "Rust":  (1.024, 0.079, 0.932, 1.152),
                    "Spark": (12.851, 1.048, 12.209, 14.925),
                },
            },
            "parallel-sum": {
                "unit": "ms",
                "data": {
                    "Rust":  (7.1, 2.1, 5.0, 19.7),
                    "Spark": (269.0, 8.8, 257.1, 285.7),
                },
            },
            "prime-check": {
                "unit": "s",
                "data": {
                    "Rust":  (1.467, 0.020, 1.446, 1.510),
                    "Spark": (1.546, 0.039, 1.511, 1.644),
                },
            },
            "list-transform": {
                "unit": "ms",
                "data": {
                    "Rust":  (21.3, 2.9, 18.2, 36.8),
                    "Spark": (57.1, 4.6, 50.2, 77.4),
                },
            },
        },
    },
    "2026-02-16-pipeline-overhaul": {
        "title": "Pipeline Overhaul",
        "benchmarks": {
            "fibonacci": {
                "unit": "ms",
                "data": {
                    "Rust":  (284.5, 1.3, 283.2, 287.5),
                    "Spark": (875.0, 15.4, 866.4, 917.0),
                },
            },
            "collatz": {
                "unit": "ms",
                "data": {
                    "Rust":  (867.1, 1.9, 864.8, 870.8),
                    "Spark": (12597.7, 52.1, 12536.2, 12707.3),
                },
            },
            "parallel-sum": {
                "unit": "ms",
                "data": {
                    "Rust":  (6.1, 0.8, 4.9, 9.9),
                    "Spark": (75.4, 3.7, 70.1, 85.9),
                },
            },
            "prime-check": {
                "unit": "s",
                "data": {
                    "Rust":  (1.449, 0.036, 1.429, 1.548),
                    "Spark": (1.448, 0.021, 1.433, 1.493),
                },
            },
            "list-transform": {
                "unit": "ms",
                "data": {
                    "Rust":  (18.4, 1.0, 17.3, 23.3),
                    "Spark": (54.5, 3.1, 49.5, 60.9),
                },
            },
        },
    },
    "2026-02-16-opt-passes": {
        "title": "Optimization Passes",
        "benchmarks": {
            "fibonacci": {
                "unit": "ms",
                "data": {
                    "Go":    (522.6, 8.7, 514.5, 542.5),
                    "Rust":  (292.6, 15.7, 284.3, 336.3),
                    "Spark": (892.7, 26.6, 875.0, 955.2),
                },
            },
            "collatz": {
                "unit": "s",
                "data": {
                    "Go":    (1.812, 0.009, 1.801, 1.827),
                    "Rust":  (0.868, 0.003, 0.864, 0.874),
                    "Spark": (1.841, 0.032, 1.822, 1.927),
                },
            },
            "parallel-sum": {
                "unit": "ms",
                "data": {
                    "Go":    (8.8, 1.1, 6.9, 13.5),
                    "Rust":  (7.4, 1.1, 5.7, 12.0),
                    "Spark": (81.6, 3.5, 76.4, 90.7),
                },
            },
            "prime-check": {
                "unit": "s",
                "data": {
                    "Go":    (1.562, 0.037, 1.536, 1.660),
                    "Rust":  (1.445, 0.015, 1.431, 1.482),
                    "Spark": (1.440, 0.005, 1.433, 1.448),
                },
            },
            "list-transform": {
                "unit": "ms",
                "data": {
                    "Go":    (20.7, 1.4, 19.5, 32.4),
                    "Rust":  (18.3, 1.5, 16.7, 25.3),
                    "Spark": (56.7, 4.6, 50.0, 74.3),
                },
            },
        },
    },
}


def generate_retro_reports(base_dir: str):
    """Generate retroactive reports from hardcoded historical data."""
    for dir_name, report_data in RETRO_REPORTS.items():
        out_dir = os.path.join(base_dir, dir_name)
        benchmarks = {}
        for bench_name, bench_info in report_data["benchmarks"].items():
            multiplier = 0.001 if bench_info["unit"] == "ms" else 1.0
            lang_times = {}
            for lang, (mean, std, mn, mx) in bench_info["data"].items():
                mean_s = mean * multiplier
                std_s = std * multiplier
                min_s = mn * multiplier
                max_s = mx * multiplier
                lang_times[lang] = synthesize_times(mean_s, std_s, min_s, max_s)
            benchmarks[bench_name] = lang_times
        generate_report(out_dir, benchmarks, title=report_data["title"])
        print(f"  -> {out_dir}")


def main():
    parser = argparse.ArgumentParser(description="Generate benchmark reports with violin plots")
    parser.add_argument("output_dir", nargs="?", help="Output directory for report")
    parser.add_argument("--title", "-t", help="Report title")
    parser.add_argument("--from-markdown", action="store_true",
                        help="Parse hyperfine markdown files instead of JSON")
    parser.add_argument("--retro", action="store_true",
                        help="Generate retroactive reports from historical data")
    parser.add_argument("files", nargs="*", help="JSON or markdown files to process")

    args = parser.parse_args()

    if args.retro:
        base_dir = args.output_dir or os.path.join(os.path.dirname(__file__), "reports")
        print(f"Generating retroactive reports in {base_dir}")
        generate_retro_reports(base_dir)
        return

    if not args.output_dir:
        parser.error("output_dir is required (unless using --retro)")
    if not args.files:
        parser.error("at least one input file is required")

    if args.from_markdown:
        benchmarks = load_from_markdown_files(args.files)
    else:
        benchmarks = load_from_json_files(args.files)

    generate_report(args.output_dir, benchmarks, title=args.title)


if __name__ == "__main__":
    main()
