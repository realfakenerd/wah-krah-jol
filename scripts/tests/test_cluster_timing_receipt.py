"""Synthetic receipts exercise the contract without claiming engine measurements."""

import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "validate-cluster-timing-receipt.py"
SPEC = importlib.util.spec_from_file_location("cluster_timing_receipt", SCRIPT)
VALIDATOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VALIDATOR)


def synthetic_receipt():
    run = {
        "frames_after_ready": 120,
        "ready_to_complete_seconds": 2.5,
        "guard_verdict": "pass",
        "warmup_discarded": True,
        "selection": {
            "selected_instances": 100,
            "suppressed_instances": 20,
            "retained_native_lod": 5,
            "lod_flags": {"object_lod": True, "terrain_lod": False},
        },
    }
    return {"baseline": copy.deepcopy(run), "candidate": copy.deepcopy(run)}


class ClusterTimingReceiptTests(unittest.TestCase):
    def assert_rejected(self, document, path):
        result = VALIDATOR.validate_receipt(document)
        self.assertFalse(result["valid"])
        self.assertTrue(any(path in error for error in result["errors"]), result)

    def mutations(self, field, values, selection=False):
        for arm in ("baseline", "candidate"):
            for value in values:
                with self.subTest(arm=arm, field=field, value=value):
                    document = synthetic_receipt()
                    target = document[arm]["selection"] if selection else document[arm]
                    target[field] = value
                    self.assert_rejected(document, f"{arm}." + ("selection." if selection else "") + field)

    def test_accepts_synthetic_matched_receipt(self):
        document = synthetic_receipt()
        document["candidate"]["frames_after_ready"] = 110
        document["candidate"]["ready_to_complete_seconds"] = 2
        document["candidate"]["annotation"] = "synthetic"
        document["candidate"]["selection"]["annotation"] = "unvalidated"
        self.assertEqual(VALIDATOR.validate_receipt(document), {"valid": True, "errors": []})

    def test_rejects_malformed_receipt_and_run_objects(self):
        for value in (None, [], False, 1, "run"):
            self.assert_rejected(value, "receipt")
            for arm in ("baseline", "candidate"):
                document = synthetic_receipt()
                document[arm] = value
                self.assert_rejected(document, arm)

    def test_rejects_missing_required_fields(self):
        for arm in ("baseline", "candidate"):
            document = synthetic_receipt()
            del document[arm]
            self.assert_rejected(document, arm)
            for selection in (False, True):
                fields = synthetic_receipt()[arm]["selection"] if selection else synthetic_receipt()[arm]
                for field in fields:
                    with self.subTest(arm=arm, field=field):
                        document = synthetic_receipt()
                        target = document[arm]["selection"] if selection else document[arm]
                        del target[field]
                        self.assert_rejected(document, f"{arm}." + ("selection." if selection else "") + field)

    def test_rejects_invalid_frame_count(self):
        self.mutations("frames_after_ready", [None, True, False, 0, -1, 1.5, "120", {}, []])

    def test_rejects_invalid_duration(self):
        self.mutations("ready_to_complete_seconds", [None, True, False, 0, -1, "2", {}, [], float("nan"), float("inf"), -float("inf")])

    def test_rejects_nonpassing_guard(self):
        self.mutations("guard_verdict", [None, False, True, "fail", "PASS", {}, []])

    def test_rejects_warmup_not_discarded(self):
        self.mutations("warmup_discarded", [None, False, 1, "true", {}, []])

    def test_rejects_invalid_selection_object(self):
        self.mutations("selection", [None, [], False, "selection"])

    def test_rejects_invalid_selection_counters(self):
        for counter in VALIDATOR.COUNTERS:
            self.mutations(counter, [None, True, False, -1, 1.5, "1", {}, []], selection=True)

    def test_accepts_zero_selection_counters(self):
        document = synthetic_receipt()
        for arm in document.values():
            for counter in VALIDATOR.COUNTERS:
                arm["selection"][counter] = 0
        self.assertTrue(VALIDATOR.validate_receipt(document)["valid"])

    def test_rejects_each_selection_counter_mismatch(self):
        for arm in ("baseline", "candidate"):
            for counter in VALIDATOR.COUNTERS:
                with self.subTest(arm=arm, counter=counter):
                    document = synthetic_receipt()
                    document[arm]["selection"][counter] += 1
                    self.assert_rejected(document, f"selection.{counter}")

    def test_rejects_invalid_lod_flags(self):
        self.mutations("lod_flags", [None, [], {}, True, {"": True}, {1: True}, {"object_lod": 1}, {"object_lod": "true"}, {"object_lod": None}], selection=True)

    def test_rejects_lod_flag_value_and_key_set_mismatch(self):
        for arm in ("baseline", "candidate"):
            for flags in ({"object_lod": False, "terrain_lod": False}, {"object_lod": True}, {"object_lod": True, "terrain_lod": False, "extra": True}):
                document = synthetic_receipt()
                document[arm]["selection"]["lod_flags"] = flags
                self.assert_rejected(document, "selection.lod_flags")

    def run_cli(self, content):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "receipt.json"
            if content is not None:
                path.write_bytes(content)
            process = subprocess.run([sys.executable, str(SCRIPT), str(path)], capture_output=True, text=True)
        self.assertEqual(process.stderr, "")
        return process.returncode, json.loads(process.stdout)

    def test_cli_accepts_valid_receipt(self):
        self.assertEqual(self.run_cli(json.dumps(synthetic_receipt()).encode()), (0, {"valid": True, "errors": []}))

    def test_cli_reports_named_validation_failure(self):
        document = synthetic_receipt()
        document["candidate"]["guard_verdict"] = "fail"
        code, result = self.run_cli(json.dumps(document).encode())
        self.assertEqual(code, 1)
        self.assertFalse(result["valid"])
        self.assertIn("candidate.guard_verdict", result["errors"][0])

    def test_cli_rejects_malformed_json(self):
        code, result = self.run_cli(b'{"baseline":')
        self.assertEqual(code, 1)
        self.assertFalse(result["valid"])
        self.assertIn("input:", result["errors"][0])

    def test_cli_rejects_duplicate_json_keys(self):
        code, result = self.run_cli(b'{"baseline": {}, "baseline": {}}')
        self.assertEqual(code, 1)
        self.assertIn("duplicate JSON key: baseline", result["errors"][0])

    def test_cli_rejects_read_and_encoding_failures(self):
        for content in (None, b'\xff'):
            code, result = self.run_cli(content)
            self.assertEqual(code, 1)
            self.assertFalse(result["valid"])
            self.assertIn("input:", result["errors"][0])

    def test_cli_usage_exits_two(self):
        process = subprocess.run([sys.executable, str(SCRIPT)], capture_output=True, text=True)
        self.assertEqual(process.returncode, 2)
        self.assertIn("usage:", process.stderr)


if __name__ == "__main__":
    unittest.main()
