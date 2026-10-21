"""Source-aware detection: candidate inspection is separate from readiness."""

from dataclasses import dataclass
import time

from detector import CATEGORIES, ORIGINS, QUESTIONS as GUARDED_QUESTIONS
from detector import Detection, Snapshot, choice, payload as guarded_payload, request, settings


QUESTIONS = {key: GUARDED_QUESTIONS[key] for key in ("dialog", "request_origin")}


@dataclass(frozen=True)
class Observation:
    classification: str
    origin: str


def gate(observation, snapshot):
    """Admit read-only extraction, including incomplete candidate dialogs."""
    if observation.classification not in CATEGORIES or observation.origin not in ORIGINS:
        raise ValueError("unknown source-aware observation")
    if observation.origin in ("historical_or_user", "none"):
        route = "uncertain" if snapshot.lifecycle == "blocked" else "ignore"
    elif observation.classification == "modal_dialog" and observation.origin == "current_dialog":
        route = "extract_modal" if snapshot.lifecycle == "blocked" else "uncertain"
    elif observation.classification == "conversational_question" and observation.origin == "latest_assistant":
        route = "extract_conversation" if snapshot.lifecycle in ("idle", "done") else "uncertain"
    elif observation.classification == "no_question":
        route = "uncertain" if snapshot.lifecycle == "blocked" else "ignore"
    else:
        route = "uncertain"
    return Detection(observation.classification, route, snapshot.fingerprint())


def payload(model, snapshot, previous=None):
    result = guarded_payload(model, snapshot, previous)
    result["questions"] = QUESTIONS
    return result


def classify(cfg, snapshot, previous=None):
    started = time.monotonic()
    response = request(cfg, payload(cfg["model"], snapshot, previous))
    observation = Observation(choice(response), choice(response, "request_origin", ORIGINS))
    detection = gate(observation, snapshot)
    return {"classification": observation.classification, "request_origin": observation.origin,
            "route": detection.route, "snapshot_fingerprint": detection.fingerprint,
            "seconds": round(time.monotonic() - started, 3)}
