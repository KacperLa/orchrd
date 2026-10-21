"""Read-only pipeline: source detection -> extraction -> presentation checks."""

import hashlib
import importlib.util
from pathlib import Path
import time

from detector import Detection
from source_detector import classify


# Reuse the frozen extractor without modifying its prompts or validation rules.
_extractor_path = Path(__file__).resolve().parents[1] / "question_extractor" / "extractor.py"
_spec = importlib.util.spec_from_file_location("orchrd_experiment_question_extractor", _extractor_path)
extractor = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(extractor)


def presentation(extracted, snapshot, route):
    """Question availability only: no permission or keyboard execution authority."""
    if extracted.get("validation_error"):
        return "inspect", "extractor_validation_failed"
    value = extracted.get("extraction")
    try:
        extractor.validate(value, snapshot.screen)
    except (ValueError, TypeError, KeyError):
        return "inspect", "invalid_extraction"
    if value["status"] != "active" or not value["complete"]:
        return "inspect", "request_not_active_or_complete"
    if value["input_mode"] == "unknown" or value["purpose"] == "unknown":
        return "inspect", "unknown_question_type"
    labels = [option["label"] for option in value["options"]]
    if value["input_mode"] == "single_choice" and len(labels) < 2:
        return "inspect", "missing_choices"
    if value["input_mode"] == "multiple_choice" and not labels:
        return "inspect", "missing_choices"
    if len(labels) != len(set(labels)):
        return "inspect", "ambiguous_choices"
    if route == "extract_conversation" and value["purpose"] != "clarification":
        return "inspect", "conversational_route_cannot_handle_permission"
    if value["purpose"] == "authentication":
        return "inspect", "authentication_requires_terminal"
    # Unknown controls do not prevent reading the question aloud. This never
    # claims that a choice is executable or that an approval was authorized.
    return "question_available", "grounded_complete_question"


def run(routing_cfg, intent_cfg, snapshot, previous=None, *, detect=classify,
        extract=extractor.extract, latest=None):
    started = time.monotonic()
    calls = 0
    detected = None
    try:
        detected = detect(routing_cfg, snapshot, previous)
        bound = Detection(detected["classification"], detected["route"],
                          detected["snapshot_fingerprint"])
        current = latest() if latest else snapshot
        if not bound.current(current):
            return {"outcome": "inspect", "reason": "screen_changed_before_extraction",
                    "detection": detected, "extractor_calls": 0,
                    "seconds": round(time.monotonic() - started, 3)}
        if detected["route"] == "ignore":
            outcome, reason, extracted = "ignore", "no_current_request", None
        elif detected["route"] == "uncertain":
            outcome, reason, extracted = "inspect", "detector_uncertain", None
        elif detected["route"] in ("extract_modal", "extract_conversation"):
            calls = 1
            extracted = extract(intent_cfg, snapshot.screen, snapshot.lifecycle)
            current = latest() if latest else snapshot
            if not bound.current(current):
                outcome, reason = "inspect", "screen_changed_during_extraction"
            else:
                outcome, reason = presentation(extracted, snapshot, detected["route"])
        else:
            raise ValueError("unknown detector route")
        return {"outcome": outcome, "reason": reason, "detection": detected,
                "extraction": extracted, "extractor_calls": calls,
                "seconds": round(time.monotonic() - started, 3)}
    except Exception as error:
        return {"outcome": "inspect", "reason": "provider_or_pipeline_error",
                "error": str(error) if isinstance(error, (ValueError, RuntimeError)) else type(error).__name__,
                "detection": detected, "extractor_calls": calls,
                "seconds": round(time.monotonic() - started, 3)}


def extraction_key(snapshot):
    return hashlib.sha256((snapshot.lifecycle + "\0" + snapshot.screen).encode()).hexdigest()
