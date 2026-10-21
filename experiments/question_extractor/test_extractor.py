import copy
import json
from pathlib import Path
import tempfile
import unittest

from evaluate import grade
from extractor import settings, validate


class Validation(unittest.TestCase):
    def setUp(self):
        self.screen = "Choose a target:\n> API\n  CLI\nUse arrows then Enter."
        self.value = {
            "status": "active", "complete": True, "purpose": "clarification", "input_mode": "single_choice",
            "question": "Choose a target:",
            "options": [{"label": "API", "effect": "other", "shortcut": None},
                        {"label": "CLI", "effect": "other", "shortcut": None}],
            "allows_custom_answer": None,
            "interaction": {"method": "keyboard_menu", "selected_index": 0,
                            "instructions": "Use arrows then Enter."},
            "evidence": ["Choose a target:", "> API"],
        }

    def test_rejects_invented_labels_evidence_shortcuts_and_instructions(self):
        validate(self.value, self.screen)
        for key in ("label", "shortcut"):
            value = copy.deepcopy(self.value)
            value["options"][0][key] = "not in screen"
            with self.assertRaises(ValueError):
                validate(value, self.screen)
        value = copy.deepcopy(self.value)
        value["evidence"].append("different quote")
        with self.assertRaises(ValueError):
            validate(value, self.screen)
        value = copy.deepcopy(self.value)
        value["interaction"]["instructions"] = "Press 1 to submit"
        with self.assertRaises(ValueError):
            validate(value, self.screen)

    def test_rejects_unknown_fields_and_boolean_indices(self):
        for change in (lambda v: v.update(keys_to_press=["enter"]),
                       lambda v: v["interaction"].update(selected_index=True),
                       lambda v: v["interaction"].update(selected_index=3)):
            value = copy.deepcopy(self.value)
            change(value)
            with self.assertRaises(ValueError):
                validate(value, self.screen)

    def test_none_cannot_smuggle_question_or_controls(self):
        value = copy.deepcopy(self.value)
        value["status"] = "none"
        with self.assertRaises(ValueError):
            validate(value, self.screen)

    def test_fixture_ids_and_labels_are_consistent(self):
        cases = json.loads(Path(__file__).with_name("cases.json").read_text())
        self.assertEqual(len(cases), len({case["id"] for case in cases}))
        self.assertEqual({case["origin"] for case in cases}, {"synthetic"})
        for case in cases:
            for label in case["expected"]["labels"]:
                self.assertIn(label, case["screen"])

    def test_grader_separates_validation_and_semantic_failures(self):
        result = {"extraction": self.value, "validation_error": None}
        case = {"expected": {"labels": ["API", "CLI"], "interaction.selected_index": 0}}
        self.assertEqual(grade(case, result), [])
        result["validation_error"] = "bad evidence"
        self.assertEqual(grade(case, result), ["validation: bad evidence"])
        self.assertTrue(grade(case, {"extraction": [], "validation_error": "invalid type"}))

    def test_credentials_are_parsed_without_shell_execution(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "config.toml"
            env = Path(directory) / "secret.env"
            config.write_text('[intent]\nbase_url="https://example.invalid/v1"\nmodel="test"\napi_key_env="EXTRACTOR_TEST_UNIQUE_KEY"\n')
            env.write_text('EXTRACTOR_TEST_UNIQUE_KEY="literal-$(do-not-execute)"\n')
            self.assertEqual(settings(config, [env])["credential"], "literal-$(do-not-execute)")


if __name__ == "__main__":
    unittest.main()
