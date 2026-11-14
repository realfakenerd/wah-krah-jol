#!/usr/bin/env python3
"""Validate a paired, post-warmup cluster timing receipt."""

import argparse
import json
import math
from pathlib import Path


COUNTERS = ("selected_instances", "suppressed_instances", "retained_native_lod")


def validate_receipt(document):
    """Return validation errors for JSON values without trusting missing evidence."""
    errors = []
    selections = {}
    if not isinstance(document, dict):
        return {"valid": False, "errors": ["receipt: expected an object"]}

    for arm in ("baseline", "candidate"):
        run = document.get(arm)
        if not isinstance(run, dict):
            errors.append(f"{arm}: required object")
            continue
        frames = run.get("frames_after_ready")
        if type(frames) is not int or frames <= 0:
            errors.append(f"{arm}.frames_after_ready: required positive integer")
        seconds = run.get("ready_to_complete_seconds")
        if (type(seconds) not in (int, float) or seconds <= 0
                or (type(seconds) is float and not math.isfinite(seconds))):
            errors.append(f"{arm}.ready_to_complete_seconds: required positive finite number")
        if run.get("guard_verdict") != "pass":
            errors.append(f"{arm}.guard_verdict: required 'pass'")
        if run.get("warmup_discarded") is not True:
            errors.append(f"{arm}.warmup_discarded: required true")

        selection = run.get("selection")
        if not isinstance(selection, dict):
            errors.append(f"{arm}.selection: required object")
            continue
        selection_error_count = len(errors)
        for counter in COUNTERS:
            value = selection.get(counter)
            if type(value) is not int or value < 0:
                errors.append(f"{arm}.selection.{counter}: required nonnegative integer")
        flags = selection.get("lod_flags")
        if not isinstance(flags, dict) or not flags:
            errors.append(f"{arm}.selection.lod_flags: required nonempty object")
        else:
            for flag, value in flags.items():
                if not isinstance(flag, str) or not flag:
                    errors.append(f"{arm}.selection.lod_flags: flag names must be nonempty strings")
                elif type(value) is not bool:
                    errors.append(f"{arm}.selection.lod_flags.{flag}: required boolean")
        if len(errors) == selection_error_count:
            selections[arm] = selection

    if len(selections) == 2:
        for field in (*COUNTERS, "lod_flags"):
            if selections["baseline"][field] != selections["candidate"][field]:
                errors.append(f"candidate.selection.{field}: mismatch with baseline.selection.{field}")
    return {"valid": not errors, "errors": errors}


def reject_duplicate_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("receipt", type=Path)
    args = parser.parse_args()
    try:
        document = json.loads(args.receipt.read_text(encoding="utf-8"),
                              object_pairs_hook=reject_duplicate_keys)
        result = validate_receipt(document)
    except (OSError, UnicodeError, ValueError, RecursionError) as error:
        result = {"valid": False, "errors": [f"input: {error}"]}
    print(json.dumps(result))
    return 0 if result["valid"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
