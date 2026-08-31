#!/usr/bin/env python
"""Fail if the microbenchmark table in benchmark/RESULTS.md disagrees with the
committed criterion artifacts.

Why this guard exists. Every row of that table is a measured figure, and
criterion writes its estimates into `target/criterion/`, which is gitignored.
For as long as that was the only copy, not one published median had a producer
or an artifact committed anywhere in this repository: a reader could not check a
single number, and nothing would notice if one were edited. The estimates for
all 18 executing cases are now committed under `benchmark/criterion/`, and this
script asserts the table and those artifacts agree.

This script does NOT measure anything and does not run `cargo bench`. CI runners
are too noisy for meaningful timings -- that is why the CI benchmark step only
compiles the benches -- so prose-vs-artifact agreement is the one thing
checkable here. It makes no timing claim of its own.

Note the direction of the comparison: every figure is *parsed out of
RESULTS.md* and checked against a value *derived from the artifact*. It
deliberately does not compare an artifact value against a literal copied from
that same artifact, which would pass vacuously. Editing either side alone fails
this check; that is the point, not an inconvenience.

Three separate things are asserted, because each fails differently:

1. Every published row maps to a committed artifact directory. A row invented
   from nothing -- the original defect -- has no artifact and fails here.
2. Every published median, both 95% CI bounds, and the sample count agree with
   that artifact, to the precision the table actually prints. Perturbing any
   displayed digit fails.
3. Every committed artifact has a published row. This is the coverage
   direction: it keeps the table from silently reverting to a subset while the
   surrounding prose still claims the full set is published.

Tolerances are derived from the printed precision rather than fixed, so a figure
shown as `734 ns` is checked to +/-0.5 ns and one shown as `19.78 us` to
+/-5 ns. A tighter tolerance would fail on digits the prose never displayed.

Usage:
    python scripts/check_documented_metrics.py [repo_root]
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

# criterion records every duration in nanoseconds. Table rows may print either
# unit, so both sides are normalized to ns before comparison.
UNITS_TO_NS = {"ns": 1.0, "us": 1000.0}

# The table rows: | `bench/id` | 734 ns | [701-779] | 100 |
# The CI bracket uses an en dash in the prose; a plain hyphen is accepted too so
# that reformatting the dash does not read as a figure changing.
ROW_PATTERN = re.compile(
    r"^\|\s*`([^`]+)`\s*\|\s*(\d+(?:\.\d+)?)\s*(ns|us)\s*\|"
    r"\s*\[\s*(\d+(?:\.\d+)?)\s*[-–]\s*(\d+(?:\.\d+)?)\s*\]\s*\|"
    r"\s*(\d+)\s*\|",
    re.MULTILINE,
)


def _tolerance_ns(printed: str, unit: str) -> float:
    """Half of the last digit the table displays, expressed in nanoseconds.

    `734` prints whole ns, so a drift under 0.5 ns is invisible in the prose and
    must not fail. `19.78` in us prints hundredths of a microsecond, i.e. 10 ns
    steps, so the tolerance is 5 ns.
    """

    decimals = len(printed.split(".")[1]) if "." in printed else 0
    return 0.5 * (10.0**-decimals) * UNITS_TO_NS[unit]


def _load_cases(criterion_dir: Path) -> dict[str, dict]:
    """Read every committed estimate, keyed by criterion's own full benchmark id.

    The id is taken from the artifact's `benchmark.json` rather than inferred
    from the directory path, so a case filed under the wrong directory is caught
    instead of being silently renamed by this loader.
    """

    cases: dict[str, dict] = {}
    for estimates_path in sorted(criterion_dir.rglob("estimates.json")):
        case_dir = estimates_path.parent
        meta = json.loads((case_dir / "benchmark.json").read_text(encoding="utf-8"))
        estimates = json.loads(estimates_path.read_text(encoding="utf-8"))
        sample = json.loads((case_dir / "sample.json").read_text(encoding="utf-8"))
        full_id = meta["full_id"]
        relative = case_dir.relative_to(criterion_dir).as_posix()
        if full_id != relative:
            raise SystemExit(
                f"artifact layout: {relative}/benchmark.json declares full_id "
                f"{full_id!r}; the directory must match the benchmark id"
            )
        median = estimates["median"]
        cases[full_id] = {
            "median_ns": median["point_estimate"],
            "ci_lower_ns": median["confidence_interval"]["lower_bound"],
            "ci_upper_ns": median["confidence_interval"]["upper_bound"],
            # Trusted over any recorded count: this is the number of
            # measurements the estimate was actually bootstrapped from.
            "samples": len(sample["times"]),
        }
    return cases


def check_microbenchmark_table(results: str, cases: dict[str, dict]) -> list[str]:
    failures: list[str] = []
    published: set[str] = set()

    for row in ROW_PATTERN.finditer(results):
        case, median_text, unit, lower_text, upper_text, samples_text = row.groups()
        if case in published:
            failures.append(f"{case}: published twice")
            continue
        published.add(case)

        artifact = cases.get(case)
        if artifact is None:
            failures.append(
                f"{case}: published in the table but no artifact is committed at "
                f"benchmark/criterion/{case}/"
            )
            continue

        # Median and both CI bounds are printed in the median's unit, so all
        # three share it. Tolerance follows each figure's own precision.
        for label, printed, actual in (
            ("median", median_text, artifact["median_ns"]),
            ("95% CI lower bound", lower_text, artifact["ci_lower_ns"]),
            ("95% CI upper bound", upper_text, artifact["ci_upper_ns"]),
        ):
            documented_ns = float(printed) * UNITS_TO_NS[unit]
            tolerance = _tolerance_ns(printed, unit)
            if abs(documented_ns - actual) > tolerance:
                failures.append(
                    f"{case}: table says {label} {printed} {unit} "
                    f"({documented_ns:.4f} ns), artifact gives {actual:.4f} ns "
                    f"(tolerance {tolerance} ns)"
                )

        if int(samples_text) != artifact["samples"]:
            failures.append(
                f"{case}: table says {samples_text} samples, artifact holds "
                f"{artifact['samples']}"
            )

    # Coverage, the other direction. The table was a subset of the suite once;
    # this is what keeps it from becoming one again without the prose saying so.
    for case in sorted(set(cases) - published):
        failures.append(
            f"{case}: an estimate is committed but no row publishes it. Add the "
            f"row, or narrow the subset-disclosure note to name this case."
        )

    if not published:
        raise SystemExit(
            "no microbenchmark rows were parsed out of benchmark/RESULTS.md. The "
            "table format changed and this guard is no longer checking anything."
        )
    return failures


def main(argv: list[str]) -> int:
    root = Path(argv[0]) if argv else Path(__file__).resolve().parent.parent
    results_path = root / "benchmark" / "RESULTS.md"
    criterion_dir = root / "benchmark" / "criterion"

    if not results_path.exists():
        raise SystemExit(f"missing {results_path}")
    if not criterion_dir.is_dir():
        raise SystemExit(
            f"missing {criterion_dir}. The published medians have no committed "
            f"artifact to check against."
        )

    cases = _load_cases(criterion_dir)
    if not cases:
        raise SystemExit(f"no estimates.json found under {criterion_dir}")

    failures = check_microbenchmark_table(
        results_path.read_text(encoding="utf-8"), cases
    )

    if failures:
        print(
            f"benchmark/RESULTS.md disagrees with the committed criterion "
            f"artifacts ({len(failures)} problem(s)):",
            file=sys.stderr,
        )
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print(
        f"benchmark/RESULTS.md: {len(cases)} microbenchmark rows each match a "
        f"committed criterion artifact (median, both 95% CI bounds, sample count)."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
