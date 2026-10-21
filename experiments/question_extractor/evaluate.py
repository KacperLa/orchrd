#!/usr/bin/env python3
"""Run synthetic, non-interactive extractor evaluations against intent config."""

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import hashlib
import json
from pathlib import Path
import statistics

from extractor import PROMPT, SCHEMA, extract, settings


def field(value, dotted):
    for key in dotted.split("."):
        if not isinstance(value, dict):
            return "<missing>"
        value = value.get(key, "<missing>")
    return value


def grade(case, result):
    if "error" in result:
        return [result["error"]]
    value = result["extraction"]
    failures = []
    if result["validation_error"]:
        failures.append("validation: " + result["validation_error"])
    if not isinstance(value, dict) or not isinstance(value.get("options"), list):
        return failures + ["extraction is not a question object with options"]
    for key, expected in case["expected"].items():
        if key in ("labels", "effects", "shortcuts"):
            item_key = {"labels": "label", "effects": "effect", "shortcuts": "shortcut"}[key]
            actual = [item.get(item_key) for item in value.get("options", []) if isinstance(item, dict)]
        else:
            actual = field(value, key)
        if actual != expected:
            failures.append(f"{key}: expected {expected!r}, got {actual!r}")
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", default="~/.config/orchrd/config.toml")
    parser.add_argument("--environment-file", action="append", default=[])
    parser.add_argument("--cases", type=Path, default=Path(__file__).with_name("cases.json"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--workers", type=int, default=3)
    parser.add_argument("--limit", type=int)
    parser.add_argument("--split", choices=["development", "holdout"])
    parser.add_argument("--response-format", choices=["json_schema", "json_object", "prompt"], default="json_schema")
    args = parser.parse_args()
    if args.repeat < 1 or not 1 <= args.workers <= 8:
        parser.error("repeat must be positive and workers between 1 and 8")
    cfg = settings(args.config, args.environment_file)
    cases_bytes = args.cases.read_bytes()
    cases = json.loads(cases_bytes)
    if args.split:
        cases = [case for case in cases if case["split"] == args.split]
    if args.limit:
        cases = cases[:args.limit]
    results = []
    args.output.parent.mkdir(parents=True, exist_ok=True)

    def run(case, repetition):
        try:
            result = extract(cfg, case["screen"], case["state"], args.response_format)
        except Exception as error:
            # HTTP error bodies, URLs, credentials and terminal data are not logged.
            result = {"error": str(error) if isinstance(error, (RuntimeError, ValueError)) else type(error).__name__}
        return {"id": case["id"], "split": case["split"], "repetition": repetition,
                **result, "failures": grade(case, result)}

    with ThreadPoolExecutor(max_workers=args.workers) as pool, args.output.open("w") as output:
        pending = [pool.submit(run, case, repetition) for repetition in range(args.repeat) for case in cases]
        for future in as_completed(pending):
            result = future.result()
            results.append(result)
            output.write(json.dumps(result, ensure_ascii=False) + "\n")
            output.flush()
            print(f"{len(results)}/{len(pending)} {result['id']}: {'FAIL' if result['failures'] else 'PASS'}", flush=True)
    successes = sum(not row["failures"] for row in results)
    timed = [row["seconds"] for row in results if "seconds" in row]
    summary = {
        "model": cfg["model"], "response_format": args.response_format,
        "prompt_sha256": hashlib.sha256((PROMPT + json.dumps(SCHEMA)).encode()).hexdigest(),
        "prompt": PROMPT,
        "schema": SCHEMA,
        "cases_sha256": hashlib.sha256(cases_bytes).hexdigest(),
        "origins": sorted({case["origin"] for case in cases}),
        "attempts": len(results), "exact_passes": successes,
        "valid_outputs": sum("extraction" in row and row.get("validation_error") is None for row in results),
        "false_active": sum(isinstance(row.get("extraction"), dict) and row["extraction"].get("status") == "active"
                            and next(case for case in cases if case["id"] == row["id"])["expected"].get("status") != "active"
                            for row in results),
        "median_seconds": round(statistics.median(timed), 3) if timed else None,
        "max_seconds": max(timed) if timed else None,
        "failed_cases": sorted({row["id"] for row in results if row["failures"]}),
    }
    args.output.with_suffix(".summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({key: value for key, value in summary.items()
                      if key not in ("prompt", "schema")}, indent=2))


if __name__ == "__main__":
    main()
