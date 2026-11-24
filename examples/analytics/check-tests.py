#!/usr/bin/env python3
"""Require named tests on their actual Cargo target, including an executed ok result."""
import argparse
import json
import re
import subprocess
from pathlib import Path


class GateError(RuntimeError):
    pass


def verify_listing(required, output):
    listed = re.findall(r"^(.+): test$", output, re.MULTILINE)
    if len(listed) != len(set(listed)):
        raise GateError("duplicate test names within a target")
    missing = set(required) - set(listed)
    if missing:
        raise GateError(f"missing required tests: {sorted(missing)}")


def verify_execution(required, output):
    results = re.findall(r"^test (.+) \.\.\. (ok|ignored|FAILED)(?: .*)?$", output, re.MULTILINE)
    for name in required:
        outcomes = [status for test, status in results if test == name]
        if outcomes != ["ok"]:
            raise GateError(f"required test {name} did not execute exactly once with ok: {outcomes}")


def check(targets, phase, runner):
    total = 0
    for target in targets:
        required = [name for p, names in target["phases"].items()
                    if phase == "all" or p == phase for name in names]
        if not required:
            continue
        args = ["cargo", "test", "-p", target["package"], *target["target"]]
        code, output = runner([*args, "--", "--list", "--format", "terse"])
        if code:
            raise GateError(f"listing failed on {target['package']} {target['target']}: {code}\n{output}")
        verify_listing(required, output)
        code, output = runner([*args, "analytics_", "--", "--color", "never"])
        if code:
            raise GateError(f"execution failed on {target['package']} {target['target']}: {code}\n{output}")
        verify_execution(required, output)
        total += len(required)
    if not total:
        raise GateError("empty required case set")
    return total


def run(args):
    result = subprocess.run(args, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False)
    return result.returncode, result.stdout


def self_test():
    case = {"package": "fixture", "target": ["--test", "one"], "phases": {"1": ["analytics_a"]}}
    count = 0

    def attempt(outputs, targets, fails):
        nonlocal count
        count += 1
        outputs = iter(outputs)
        try:
            assert check(targets, "1", lambda _: next(outputs)) > 0
        except GateError:
            assert fails
        else:
            assert not fails

    good = [(0, "analytics_a: test\n"), (0, "test analytics_a ... ok\n")]
    attempt(good, [case], False)
    attempt([(0, "unrelated: test\n")], [case], True)
    attempt([(0, "analytics_a: test\n"), (0, "test analytics_a ... ignored\n")], [case], True)
    attempt([(0, "analytics_a: test\n"), (0, "test unrelated ... ok\n")], [case], True)
    attempt([(0, "analytics_a: test\n"), (1, "test analytics_a ... ok\n")], [case], True)
    attempt([(1, "analytics_a: test\n")], [case], True)
    attempt([(0, "analytics_a: test\nanalytics_a: test\n")], [case], True)
    attempt([(0, "analytics_a: test\n"), (0, "test analytics_a ... ok\ntest analytics_a ... ok\n")], [case], True)
    attempt([], [], True)
    other = {**case, "target": ["--lib"]}
    attempt([*good, (0, "other: test\n")], [case, other], True)
    attempt([*good, *good], [case, other], False)
    assert count == 11
    print(f"checker self-test: {count} cases passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--phase", choices=["1", "2", "3", "all"], default="all")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    targets = json.loads(Path(__file__).with_name("required-tests.json").read_text())
    try:
        count = check(targets, args.phase, run)
    except GateError as error:
        raise SystemExit(str(error)) from error
    print(f"phase {args.phase}: {count} required tests executed successfully")


if __name__ == "__main__":
    main()
