from dataclasses import replace
import json
from pathlib import Path
import unittest

from detector import CATEGORIES, Observation, Snapshot, choice, gate, observe, payload
from evaluate_detector import analyze, score


def observation(classification):
    origin = {"modal_dialog": "current_dialog", "conversational_question": "latest_assistant",
              "no_question": "none", "uncertain": "uncertain"}.get(classification, "uncertain")
    return Observation(classification, origin, "visible_request" if classification in ("modal_dialog", "conversational_question") else "uncertain")


class DetectorPolicy(unittest.TestCase):
    def setUp(self):
        self.current = Snapshot("agent", "instance-1", "blocked", 10,
                                "Choose a target:\n> API\n  CLI\nEnter to select")

    def test_modal_extraction_requires_blocked_and_modal_classification(self):
        self.assertEqual(gate(observation("modal_dialog"), self.current).route, "extract_modal")
        for lifecycle in ("idle", "done", "working", "unknown"):
            self.assertEqual(gate(observation("modal_dialog"), replace(self.current, lifecycle=lifecycle)).route, "uncertain")
        for classification in ("conversational_question", "no_question", "uncertain"):
            self.assertEqual(gate(observation(classification), self.current).route, "uncertain")

    def test_conversational_input_cannot_become_modal_input(self):
        for lifecycle in ("idle", "done"):
            snapshot = replace(self.current, lifecycle=lifecycle)
            self.assertEqual(gate(observation("conversational_question"), snapshot).route, "extract_conversation")
            self.assertEqual(gate(observation("no_question"), snapshot).route, "ignore")
        self.assertEqual(gate(observation("conversational_question"), replace(self.current, lifecycle="working")).route, "uncertain")

    def test_binding_invalidates_changed_screen_state_identity_and_instance(self):
        detected = gate(observation("modal_dialog"), self.current)
        self.assertTrue(detected.current(self.current))
        for changed in (replace(self.current, screen=self.current.screen + " "),
                        replace(self.current, lifecycle="idle"),
                        replace(self.current, state_change_seq=11),
                        replace(self.current, agent_id="other"),
                        replace(self.current, instance_id="instance-2")):
            self.assertFalse(detected.current(changed))

    def test_payload_keeps_current_and_previous_separate_and_omits_annotations(self):
        previous = replace(self.current, lifecycle="working", screen="Working...")
        value = payload("decision", self.current, previous)
        self.assertEqual(value["state"]["current_screen"], self.current.screen)
        self.assertEqual(value["state"]["previous_screen"], "Working...")
        self.assertEqual(set(value["questions"]["dialog"]["criteria"]), set(CATEGORIES))
        self.assertNotIn("expected", value)
        self.assertNotIn("expected", value["state"])
        with self.assertRaises(ValueError):
            payload("decision", self.current, replace(previous, instance_id="other"))

    def test_unknown_answers_never_fall_through_to_input_routes(self):
        for result in (None, [], {"answers": []}, {"answers": {"dialog": []}}, {},
                       {"answers": {"dialog": {"type": "choice", "choice": "approve"}}},
                       {"answers": {"dialog": {"type": "text", "choice": "modal_dialog"}}}):
            with self.assertRaises(ValueError):
                choice(result)
        with self.assertRaises(ValueError):
            gate(observation("approve"), self.current)
        with self.assertRaises(ValueError):
            observe({"answers": {"dialog": {"type": "choice", "choice": "modal_dialog"}}})

    def test_headerless_menu_and_wrong_author_cannot_route_to_extraction(self):
        for visibility in ("missing_request", "uncertain"):
            partial = Observation("modal_dialog", "current_dialog", visibility)
            self.assertEqual(gate(partial, self.current).route, "uncertain")
        historical = Observation("conversational_question", "historical_or_user", "missing_request")
        self.assertEqual(gate(historical, replace(self.current, lifecycle="idle")).route, "ignore")
        self.assertEqual(gate(historical, self.current).route, "uncertain")
        mismatch = Observation("modal_dialog", "latest_assistant", "visible_request")
        self.assertEqual(gate(mismatch, self.current).route, "uncertain")
        chat = Observation("conversational_question", "latest_assistant", "missing_request")
        self.assertEqual(gate(chat, replace(self.current, lifecycle="idle")).route, "extract_conversation")
        self.assertEqual(gate(chat, self.current).route, "uncertain")

    def test_corpus_is_consistent_with_controller_policy(self):
        cases = json.loads(Path(__file__).with_name("cases.json").read_text())
        self.assertEqual(len(cases), len({case["id"] for case in cases}))
        for case in cases:
            current = replace(self.current, lifecycle=case["current"]["lifecycle"])
            self.assertEqual(gate(observation(case["expected"]["classification"]), current).route,
                             case["expected"]["route"], case["id"])

    def test_metrics_distinguish_classification_from_policy_and_transport(self):
        cases = [{"id": "one", "expected": {"classification": "no_question", "route": "ignore"}}]
        rows = [{"id": "one", "classification": "modal_dialog", "route": "uncertain", "seconds": 1},
                {"id": "one", "error": "TimeoutError", "seconds": 30}]
        self.assertEqual(len(score(cases[0], rows[0])), 2)
        result = analyze(cases, rows)
        self.assertEqual(result["false_modal_routes"], 0)
        self.assertEqual(result["errors"], {"TimeoutError": 1})
        self.assertEqual(result["fields"]["classification"]["total"], 2)


if __name__ == "__main__":
    unittest.main()
