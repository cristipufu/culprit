#!/usr/bin/env python3
"""Refresh the browser demo with today's analysis, without connecting to a database.

Run `python3 dev/refresh-demo.py` after changing findings or plan advice. The saved
snapshots stay the inputs; reports and their derived browser sidecars move together.
"""

import json
from pathlib import Path
import subprocess


ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "fixtures"


def run(*args):
    subprocess.run(args, cwd=ROOT, check=True)


def cli(*args):
    return json.loads(subprocess.check_output([str(ROOT / "target/debug/culprit"), "--json", *map(str, args)], cwd=ROOT))


def write(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n")


def main():
    run("cargo", "build", "-p", "culprit-cli")
    paths = sorted(FIXTURES.glob("*.report.json"))
    # Read every input before replacing any report.
    reports = [(path, cli("analyze", path)) for path in paths]
    for path, report in reports:
        # Match the desktop projection: replay evidence belongs in saved scan artifacts, not the browser bundle.
        report.pop("evidence", None)
        write(path, report)
    write(FIXTURES / "checks.sample.json", cli("checks"))
    for filename, command in [("dashboard.sample.json", "dashboard"), ("index-advice.sample.json", "indexes")]:
        write(FIXTURES / filename, {
            report["snapshot"]["connection"]: cli(command, path)
            for path, report in reports
        })
    # Retained activity is projected by Rust. Keep the older live captures, but refresh this derived view together
    # with its snapshot so the browser never substitutes durations or another period for native task samples.
    waits_path = FIXTURES / "waits.sample.json"
    waits = json.loads(waits_path.read_text())
    for path, report in reports:
        if report["snapshot"].get("operations", {}).get("activity_history") is not None:
            waits["reports"][report["snapshot"]["connection"]] = cli("waits", "--file", path)["waits"]
    # Preserve the captured sidecar's existing order and one-space indentation.
    waits_path.write_text(json.dumps(waits, indent=1, ensure_ascii=False) + "\n")
    run("cargo", "test", "-p", "culprit-engine", "--test", "all", "insights::write_demo_sidecar", "--", "--ignored")
    run("cargo", "run", "-p", "culprit-engine", "--example", "share_sample")
    run("cargo", "run", "-p", "culprit-engine", "--example", "dashboard_live_sample")
    run("cargo", "run", "-p", "culprit-engine", "--example", "plan_view_sample")


if __name__ == "__main__":
    main()
