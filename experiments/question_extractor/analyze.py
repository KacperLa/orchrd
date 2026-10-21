"""Re-score saved outputs without making model calls."""

import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path
import statistics

from evaluate import grade


def analyze(cases, rows):
    by_id = {case["id"]: case for case in cases}
    fields = defaultdict(lambda: {"passed": 0, "total": 0})
    failures = Counter()
    repeats = defaultdict(list)
    errors = Counter()
    core_passes = false_active = false_controls = valid = exact = 0
    times = []
    for row in rows:
        case = by_id[row["id"]]
        current_failures = grade(case, row)
        repeats[row["id"]].append(not current_failures)
        exact += not current_failures
        for failure in current_failures:
            failures[failure.split(":", 1)[0]] += 1
        if "error" in row:
            errors[row["error"]] += 1
        if "seconds" in row:
            times.append(row["seconds"])
        valid += "extraction" in row and row.get("validation_error") is None
        for key, expected in case["expected"].items():
            field_case = {"expected": {key: expected}}
            # Semantic scoring is deliberately separate from quote validation.
            field_row = {**row, "validation_error": None}
            fields[key]["total"] += 1
            fields[key]["passed"] += not grade(field_case, field_row)
        core_case = {"expected": {key: case["expected"][key]
                                  for key in ("status", "purpose", "input_mode")}}
        core_passes += not grade(core_case, {**row, "validation_error": None})
        value = row.get("extraction")
        if isinstance(value, dict):
            false_active += value.get("status") == "active" and case["expected"]["status"] != "active"
            false_controls += (
                case["expected"].get("interaction.method") == "unknown"
                and value.get("interaction", {}).get("method") not in (None, "unknown")
            )
    return {
        "attempts": len(rows), "unique_cases": len(repeats), "exact_passes": exact,
        "core_passes": core_passes, "valid_outputs": valid,
        "cases_passing_all_repeats": sum(all(values) for values in repeats.values()),
        "false_active": false_active, "unsupported_control_inferences": false_controls,
        "errors": dict(errors),
        "median_response_seconds_excluding_errors": round(statistics.median(times), 3) if times else None,
        "max_response_seconds": max(times) if times else None,
        "fields": dict(fields), "failure_categories": dict(failures),
        "failed_cases": sorted(key for key, values in repeats.items() if not all(values)),
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", type=Path)
    parser.add_argument("--cases", type=Path, default=Path(__file__).with_name("cases.json"))
    args = parser.parse_args()
    rows = [json.loads(line) for line in args.results.read_text().splitlines()]
    print(json.dumps(analyze(json.loads(args.cases.read_text()), rows), indent=2))
