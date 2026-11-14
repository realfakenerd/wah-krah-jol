#!/usr/bin/env python3
"""Compare two streamed placement-probe directories and check their physics steps."""
import argparse
import csv
import json
from pathlib import Path


def check(off_dir, on_dir):
    off = json.loads((off_dir / "scene.json").read_text())
    on = json.loads((on_dir / "scene.json").read_text())
    assert off["passed"] and on["passed"] and off["policy_off"] and not on["policy_off"]
    assert off["form_id"] == on["form_id"] and off["assets_ready"] == on["assets_ready"]
    assert on["matched_rays"] >= 4 and on["mismatched_rays"] == 0
    assert on["transform_max_error"] < 0.05
    assert len(off["ray_hits"]) == len(on["ray_hits"])
    counts = {"unchanged_existing": 0, "new_target": 0, "unchanged_clear": 0}
    for before, after in zip(off["ray_hits"], on["ray_hits"]):
        assert (before["ix"], before["iz"]) == (after["ix"], after["iz"])
        if before["identity"] is not None:
            assert after["identity"] == before["identity"]
            assert abs(after["distance"] - before["distance"]) < 0.05
            counts["unchanged_existing"] += 1
        elif after["is_target"]:
            assert abs(after["distance"] - after["expected_distance"]) < 0.05
            counts["new_target"] += 1
        else:
            assert after["identity"] is None
            counts["unchanged_clear"] += 1
    for directory in (off_dir, on_dir):
        with (directory / "physics.csv").open(newline="") as stream:
            assert sum(int(row["steps"]) for row in csv.DictReader(stream)) > 0
    return {"form_id": on["form_id"], "matched_authored_rays": on["matched_rays"],
            "ray_comparison": counts, "passed": True}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("off_dir", type=Path)
    parser.add_argument("on_dir", type=Path)
    args = parser.parse_args()
    print(json.dumps(check(args.off_dir, args.on_dir), indent=2))
