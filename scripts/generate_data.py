#!/usr/bin/env python3

from __future__ import annotations

import argparse
import json
import os
import subprocess
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
TEMPLATE = ROOT / "data.template.md"
OUTPUT = ROOT / "data.md"
CRITERION = ROOT / "target" / "criterion"

BENCH_COMMAND = "cargo bench --bench compare"
WORKLOADS = [
    "turn_ring",
    "bounded_queue",
    "hash_map_queue",
    "btree_map_queue",
    "signal_stress",
    "broadcast_stress",
]
THREADS = [2, 8, 32, 64, 128]
IMPLEMENTATIONS = ["musl", "glibc", "musl_wake", "std", "llvm_old", "llvm_new"]


@dataclass(frozen=True)
class Result:
    workload: str
    implementation: str
    threads: int
    mean_ns: float


def run_command(cmd: list[str]) -> str:
    env = dict(os.environ)
    env.pop("RUSTC_WRAPPER", None)
    completed = subprocess.run(
        cmd,
        cwd=ROOT,
        env=env,
        check=True,
        text=True,
        capture_output=True,
    )
    return completed.stdout


def format_ns(value_ns: float) -> str:
    if value_ns >= 1_000_000_000:
        return f"{value_ns / 1_000_000_000:.3f} s"
    if value_ns >= 1_000_000:
        return f"{value_ns / 1_000_000:.3f} ms"
    if value_ns >= 1_000:
        return f"{value_ns / 1_000:.3f} us"
    return f"{value_ns:.0f} ns"


def format_cell(value_ns: float, best_ns: float) -> str:
    delta = (value_ns / best_ns - 1.0) * 100.0
    if abs(delta) < 0.05:
        return f"{format_ns(value_ns)} (best)"
    return f"{format_ns(value_ns)} (+{delta:.1f}%)"


def read_results() -> list[Result]:
    results: list[Result] = []
    for workload in WORKLOADS:
        for implementation in IMPLEMENTATIONS:
            for threads in THREADS:
                path = (
                    CRITERION
                    / workload
                    / implementation
                    / str(threads)
                    / "new"
                    / "estimates.json"
                )
                if not path.exists():
                    raise FileNotFoundError(f"missing benchmark output: {path}")
                data = json.loads(path.read_text())
                results.append(
                    Result(
                        workload=workload,
                        implementation=implementation,
                        threads=threads,
                        mean_ns=float(data["mean"]["point_estimate"]),
                    )
                )
    return results


def build_tables(results: list[Result]) -> str:
    lookup = {
        (result.workload, result.threads, result.implementation): result.mean_ns
        for result in results
    }

    sections: list[str] = []
    for workload in WORKLOADS:
        sections.append(f"### `{workload}`")
        sections.append("")
        sections.append(
            "| threads | musl | glibc | musl_wake | std | llvm_old | llvm_new |"
        )
        sections.append("| --- | --- | --- | --- | --- | --- | --- |")

        for threads in THREADS:
            row_values = [
                lookup[(workload, threads, implementation)]
                for implementation in IMPLEMENTATIONS
            ]
            best = min(row_values)
            cells = [str(threads)]
            cells.extend(format_cell(value, best) for value in row_values)
            sections.append("| " + " | ".join(cells) + " |")
        sections.append("")

    return "\n".join(sections).rstrip()


def render(results: list[Result], lscpu_output: str) -> str:
    template = TEMPLATE.read_text()
    return (
        template.replace("{{BENCH_COMMAND}}", BENCH_COMMAND)
        .replace("{{TABLES}}", build_tables(results))
        .replace("{{LSCPU}}", lscpu_output.rstrip())
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--skip-bench",
        action="store_true",
        help="reuse existing target/criterion output instead of running cargo bench",
    )
    args = parser.parse_args()

    if not args.skip_bench:
        subprocess.run(
            ["cargo", "bench", "--bench", "compare"],
            cwd=ROOT,
            env={k: v for k, v in os.environ.items() if k != "RUSTC_WRAPPER"},
            check=True,
        )

    lscpu_output = run_command(["lscpu"])
    results = read_results()
    OUTPUT.write_text(render(results, lscpu_output))
    print(OUTPUT)


if __name__ == "__main__":
    main()
