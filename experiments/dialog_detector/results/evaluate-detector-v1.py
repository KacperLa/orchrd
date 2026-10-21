#!/usr/bin/env python3
"""Evaluate dialog detection and controller gates without sending agent input."""

import argparse
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor, as_completed
import hashlib
import json
from pathlib import Path
import statistics
import time

from detector import CRITERIA, INSTRUCTIONS, Snapshot, classify, settings


def snapshot(case, key="current"):
    value = case[key]
    return Snapshot(case["id"], "evaluation-instance", value["lifecycle"],
                    value["state_change_seq"], value["screen"])


def score(case, result):
    if "error" in result:
        return [result["error"]]
    return [f"{key}: expected {expected}, got {result.get(key)}"
            for key, expected in case["expected"].items()
            if result.get(key) != expected]


def analyze(cases, rows):
    by_id = {case["id"]: case for case in cases}
    fields = {key: {"passed": 0, "total": len(rows)} for key in ("classification", "route")}
    confusion = defaultdict(Counter)
    per_case = defaultdict(list)
    false_modal = false_conversation = 0
    for row in rows:
        expected = by_id[row["id"]]["expected"]
        per_case[row["id"]].append(not score(by_id[row["id"]], row))
        for key in fields:
            fields[key]["passed"] += row.get(key) == expected[key]
        confusion[expected["classification"]][row.get("classification", "error")] += 1
        false_modal += row.get("route") == "extract_modal" and expected["route"] != "extract_modal"
        false_conversation += row.get("route") == "extract_conversation" and expected["route"] != "extract_conversation"
    times = [row["seconds"] for row in rows if "classification" in row]
    return {
        "attempts": len(rows), "unique_cases": len(per_case), "fields": fields,
        "exact_passes": sum(not score(by_id[row["id"]], row) for row in rows),
        "cases_passing_all_repeats": sum(all(passes) for passes in per_case.values()),
        "false_modal_routes": false_modal, "false_conversation_routes": false_conversation,
        "errors": dict(Counter(row["error"] for row in rows if "error" in row)),
        "median_response_seconds_excluding_errors": round(statistics.median(times), 3) if times else None,
        "max_response_seconds": max(times) if times else None,
        "confusion": {key: dict(value) for key, value in confusion.items()},
        "failed_cases": sorted(key for key, values in per_case.items() if not all(values)),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", default="~/.config/orchrd/config.toml")
    parser.add_argument("--cases", type=Path, default=Path(__file__).with_name("cases.json"))
    parser.add_argument("--split", choices=["development", "regression", "holdout"])
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--workers", type=int, default=2)
    parser.add_argument("--limit", type=int)
    args = parser.parse_args()
    if args.repeat < 1 or not 1 <= args.workers <= 4:
        parser.error("repeat must be positive and workers between 1 and 4")
    cfg = settings(args.config)
    cases_bytes = args.cases.read_bytes()
    cases = json.loads(cases_bytes)
    if args.split:
        cases = [case for case in cases if case["split"] == args.split]
    if args.limit:
        cases = cases[:args.limit]
    if not cases:
        parser.error("no matching cases")
    rows = []
    args.output.parent.mkdir(parents=True, exist_ok=True)

    def run(case, repetition):
        started = time.monotonic()
        try:
            previous = snapshot(case, "previous") if case.get("previous") else None
            result = classify(cfg, snapshot(case), previous)
        except Exception as error:
            result = {"error": str(error) if isinstance(error, (ValueError, RuntimeError)) else type(error).__name__,
                      "seconds": round(time.monotonic() - started, 3)}
        return {"id": case["id"], "split": case["split"], "repetition": repetition,
                **result, "failures": score(case, result)}

    with ThreadPoolExecutor(max_workers=args.workers) as pool, args.output.open("w") as output:
        futures = [pool.submit(run, case, repetition)
                   for repetition in range(args.repeat) for case in cases]
        for future in as_completed(futures):
            row = future.result()
            rows.append(row)
            output.write(json.dumps(row) + "\n")
            output.flush()
            print(f"{len(rows)}/{len(futures)} {row['id']}: {'FAIL' if row['failures'] else 'PASS'}", flush=True)
    summary = {"model": cfg["model"], "workers": args.workers,
               "implementation_sha256": hashlib.sha256(Path(__file__).with_name("detector.py").read_bytes()).hexdigest(),
               "cases_sha256": hashlib.sha256(cases_bytes).hexdigest(),
               "classifier_sha256": hashlib.sha256((INSTRUCTIONS + json.dumps(CRITERIA)).encode()).hexdigest(),
               "instructions": INSTRUCTIONS, "criteria": CRITERIA,
               "origins": sorted({case["origin"] for case in cases}),
               **analyze(cases, rows)}
    args.output.with_suffix(".summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({key: value for key, value in summary.items()
                      if key not in ("instructions", "criteria")}, indent=2))


if __name__ == "__main__":
    main()
