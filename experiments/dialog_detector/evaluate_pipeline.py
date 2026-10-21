#!/usr/bin/env python3
"""Evaluate read-only question availability; never execute agent input."""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor, as_completed
import hashlib
import json
from pathlib import Path
import statistics

from detector import Snapshot, settings as routing_settings
from pipeline import extraction_key, extractor, run
from source_detector import QUESTIONS


def snapshot(case):
    current = case["current"]
    return Snapshot(case["id"], "pipeline-evaluation", current["lifecycle"],
                    current["state_change_seq"], current["screen"])


def score(case, row):
    failures = []
    envelope = row.get("extraction")
    value = envelope.get("extraction") if isinstance(envelope, dict) else None
    value = value if isinstance(value, dict) else {}
    options = value.get("options")
    options = options if isinstance(options, list) else []
    for field, expected in case["expected"].items():
        if field in ("labels", "effects"):
            key = "label" if field == "labels" else "effect"
            actual = [option.get(key) if isinstance(option, dict) else None for option in options]
        elif field == "option_count":
            actual = len(options)
        elif field in ("purpose", "input_mode"):
            actual = value.get(field)
        else:
            actual = row.get(field)
        if actual != expected:
            failures.append(f"{field}: expected {expected!r}, got {actual!r}")
    return failures


def replay_cache(case_file, results_files):
    originals = {case["id"]: case for case in json.loads(case_file.read_text())}
    cached = {}
    for file in results_files:
        for line in file.read_text().splitlines():
            row = json.loads(line)
            case = originals[row["id"]]
            key = extraction_key(Snapshot("unused", "unused", case["state"], None, case["screen"]))
            cached.setdefault(key, []).append(row)
    return cached


def analyze(cases, rows):
    by_id = {case["id"]: case for case in cases}
    ready_expected = sum(by_id[row["id"]]["expected"]["outcome"] == "question_available" for row in rows)
    expected_ignore = sum(by_id[row["id"]]["expected"]["outcome"] == "ignore" for row in rows)
    times = [row["seconds"] for row in rows]
    return {
        "attempts": len(rows), "unique_cases": len({row["id"] for row in rows}),
        "outcome_matches": sum(row["outcome"] == by_id[row["id"]]["expected"]["outcome"] for row in rows),
        "exact_passes": sum(not score(by_id[row["id"]], row) for row in rows),
        "expected_questions": ready_expected,
        "available_expected_questions": sum(row["outcome"] == "question_available"
            and by_id[row["id"]]["expected"]["outcome"] == "question_available" for row in rows),
        "false_available_questions": sum(row["outcome"] == "question_available"
            and by_id[row["id"]]["expected"]["outcome"] != "question_available" for row in rows),
        "expected_ignored": expected_ignore,
        "correctly_ignored": sum(row["outcome"] == "ignore"
            and by_id[row["id"]]["expected"]["outcome"] == "ignore" for row in rows),
        "extractor_invocations": sum(row["extractor_calls"] for row in rows),
        "outcomes": dict(Counter(row["outcome"] for row in rows)),
        "reasons": dict(Counter(row["reason"] for row in rows)),
        "errors": dict(Counter(row["error"] for row in rows if "error" in row)),
        "median_end_to_end_seconds": round(statistics.median(times), 3) if times else None,
        "failed_cases": sorted({row["id"] for row in rows if score(by_id[row["id"]], row)}),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", default="~/.config/orchrd/config.toml")
    parser.add_argument("--environment-file", action="append", default=[])
    parser.add_argument("--cases", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--workers", type=int, default=2)
    parser.add_argument("--replay-results", type=Path, action="append", default=[])
    parser.add_argument("--replay-cases", type=Path, default=Path(__file__).resolve().parents[1] / "question_extractor/cases.json")
    args = parser.parse_args()
    if args.repeat < 1 or not 1 <= args.workers <= 4:
        parser.error("repeat must be positive; workers must be between 1 and 4")
    cases_bytes = args.cases.read_bytes()
    cases = json.loads(cases_bytes)
    routing = routing_settings(args.config)
    intent = None if args.replay_results else extractor.settings(args.config, args.environment_file)
    cached = replay_cache(args.replay_cases, args.replay_results) if args.replay_results else None
    rows = []

    def evaluate(case, repetition):
        current = snapshot(case)
        previous = None
        if case.get("previous"):
            item = case["previous"]
            previous = Snapshot(current.agent_id, current.instance_id, item["lifecycle"],
                                item["state_change_seq"], item["screen"])
        def extract(cfg, screen, lifecycle):
            if cached is None:
                return extractor.extract(cfg, screen, lifecycle)
            values = cached.get(extraction_key(current))
            if not values:
                raise ValueError("no matching cached extraction; network fallback disabled")
            value = values[repetition % len(values)]
            if "error" in value:
                raise RuntimeError("replayed extractor error: " + value["error"])
            return {key: value[key] for key in ("extraction", "validation_error", "seconds", "usage", "screen_sha256")}
        row = {"id": case["id"], "repetition": repetition,
               **run(routing, intent, current, previous, extract=extract)}
        row["failures"] = score(case, row)
        return row

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with ThreadPoolExecutor(max_workers=args.workers) as pool, args.output.open("w") as output:
        futures = [pool.submit(evaluate, case, repetition)
                   for repetition in range(args.repeat) for case in cases]
        for future in as_completed(futures):
            row = future.result()
            rows.append(row)
            output.write(json.dumps(row, ensure_ascii=False) + "\n")
            output.flush()
            print(f"{len(rows)}/{len(futures)} {row['id']}: {row['outcome']} ({row['reason']})", flush=True)
    files = ["source_detector.py", "detector.py", "pipeline.py"]
    summary = {
        "detection_model": routing["model"], "extraction_model": intent["model"] if intent else "recorded bean outputs",
        "extraction_mode": "replay" if cached is not None else "fresh",
        "cases_sha256": hashlib.sha256(cases_bytes).hexdigest(),
        "source_sha256": {name: hashlib.sha256(Path(__file__).with_name(name).read_bytes()).hexdigest() for name in files},
        "extractor_sha256": hashlib.sha256(Path(extractor.__file__).read_bytes()).hexdigest(),
        "detector_questions": QUESTIONS,
        "origins": sorted({case["origin"] for case in cases}),
        **analyze(cases, rows),
    }
    summary["extractor_api_calls"] = 0 if cached is not None else summary["extractor_invocations"]
    args.output.with_suffix(".summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({key: value for key, value in summary.items() if key != "detector_questions"}, indent=2))


if __name__ == "__main__":
    main()
