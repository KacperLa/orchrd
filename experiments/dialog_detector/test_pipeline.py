import copy
from dataclasses import replace
import unittest
from unittest.mock import Mock, patch

from detector import Snapshot
from evaluate_pipeline import score
from pipeline import presentation, run
from source_detector import Observation, classify, gate, payload


class SourcePipeline(unittest.TestCase):
    def setUp(self):
        self.snapshot = Snapshot("agent", "one", "blocked", 7,
                                 "Choose the target:\n> API\n  CLI")
        value = {
            "status": "active", "complete": True, "purpose": "clarification",
            "input_mode": "single_choice", "question": "Choose the target:",
            "options": [{"label": label, "effect": "other", "shortcut": None}
                        for label in ("API", "CLI")],
            "allows_custom_answer": None,
            "interaction": {"method": "unknown", "selected_index": 0, "instructions": None},
            "evidence": ["Choose the target:"],
        }
        self.extracted = {"extraction": value, "validation_error": None}

    def detect(self, cfg, snapshot, previous=None):
        result = gate(Observation("modal_dialog", "current_dialog"), snapshot)
        return {"classification": result.classification, "route": result.route,
                "snapshot_fingerprint": result.fingerprint}

    def test_source_detector_does_not_request_visibility(self):
        self.assertEqual(set(payload("decision", self.snapshot)["questions"]),
                         {"dialog", "request_origin"})
        response = {"answers": {
            "dialog": {"type": "choice", "choice": "modal_dialog"},
            "request_origin": {"type": "choice", "choice": "current_dialog"},
        }}
        with patch("source_detector.request", return_value=response):
            result = classify({"model": "decision"}, self.snapshot)
        self.assertEqual(result["route"], "extract_modal")

    def test_headerless_candidates_can_be_read_but_incomplete_extraction_is_not_available(self):
        partial = copy.deepcopy(self.extracted)
        partial["extraction"]["complete"] = False
        extract = Mock(return_value=partial)
        result = run({}, {}, self.snapshot, detect=self.detect, extract=extract)
        self.assertEqual(result["extractor_calls"], 1)
        self.assertEqual(result["outcome"], "inspect")

    def test_unknown_controls_do_not_prevent_reading_a_grounded_complete_question(self):
        result = run({}, {}, self.snapshot, detect=self.detect,
                     extract=Mock(return_value=self.extracted))
        self.assertEqual(result["outcome"], "question_available")
        self.assertNotIn("keys", result)
        self.assertNotIn("approve", result)

    def test_ignored_placeholder_or_user_question_never_reaches_extractor(self):
        snapshot = replace(self.snapshot, lifecycle="idle")
        for origin in ("none", "historical_or_user"):
            def detect(cfg, current, previous=None):
                result = gate(Observation("conversational_question", origin), current)
                return {"classification": result.classification, "route": result.route,
                        "snapshot_fingerprint": result.fingerprint}
            extract = Mock(side_effect=AssertionError("extractor must not run"))
            result = run({}, {}, snapshot, detect=detect, extract=extract)
            self.assertEqual(result["outcome"], "ignore")
            extract.assert_not_called()

    def test_stale_detection_is_rejected_before_and_after_extraction(self):
        changed = replace(self.snapshot, state_change_seq=8)
        extract = Mock(return_value=self.extracted)
        result = run({}, {}, self.snapshot, detect=self.detect, extract=extract,
                     latest=lambda: changed)
        self.assertEqual(result["reason"], "screen_changed_before_extraction")
        extract.assert_not_called()
        states = iter([self.snapshot, changed])
        result = run({}, {}, self.snapshot, detect=self.detect, extract=extract,
                     latest=lambda: next(states))
        self.assertEqual(result["reason"], "screen_changed_during_extraction")
        self.assertEqual(result["extractor_calls"], 1)

    def test_invalid_evidence_unknown_types_and_authentication_require_inspection(self):
        for mutate in (
            lambda value: value.update(question="Invented question"),
            lambda value: value.update(input_mode="unknown"),
            lambda value: value.update(purpose="authentication"),
            lambda value: value.update(complete=False),
            lambda value: value.update(question=""),
        ):
            extracted = copy.deepcopy(self.extracted)
            mutate(extracted["extraction"])
            self.assertEqual(presentation(extracted, self.snapshot, "extract_modal")[0], "inspect")
        permission = copy.deepcopy(self.extracted)
        permission["extraction"]["purpose"] = "permission"
        self.assertEqual(presentation(permission, self.snapshot, "extract_conversation")[0], "inspect")

    def test_provider_failure_never_creates_an_available_question(self):
        def fail(*args):
            raise TimeoutError()
        result = run({}, {}, self.snapshot, detect=self.detect, extract=fail)
        self.assertEqual(result["outcome"], "inspect")
        self.assertEqual(result["error"], "TimeoutError")

    def test_grader_checks_permission_scopes_and_handles_malformed_extraction(self):
        case = {"expected": {"option_count": 2, "effects": ["other", "other"]}}
        self.assertEqual(score(case, {"extraction": self.extracted}), [])
        changed = copy.deepcopy(self.extracted)
        changed["extraction"]["options"][0]["effect"] = "allow_persistent"
        self.assertTrue(score(case, {"extraction": changed}))
        for envelope in (None, {"extraction": None}, {"extraction": {"options": [None]}}):
            self.assertTrue(score(case, {"extraction": envelope}))


if __name__ == "__main__":
    unittest.main()
