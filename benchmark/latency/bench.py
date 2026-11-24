#!/usr/bin/env python3
"""Latency suite: wall time of rivet commands on synthetic stamped trackers.

Runs prebuilt binaries only; it never builds or installs anything. Every
timing row carries the 1-minute load average and IO pressure
(/proc/pressure/io `some avg10`) seen while it was taken, because the numbers
are only meaningful beside the load they were measured at.

Single-writer rows interleave binaries and sizes inside each repetition, so
load drift hits every cell alike. Concurrent rows run K writer processes on
disjoint items of one file. See benchmark/latency/README.md for the method.
"""
import argparse
import concurrent.futures as cf
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import time

PATH = "tracking/TRACKING_SYNTH.md"
HERE = os.path.dirname(os.path.abspath(__file__))
EXAMPLE = os.path.join(HERE, "..", "..", "examples", "tracker")


def tracker(lines):
    """A synthetic tracker of about `lines` lines: half Action items, half Log
    sections. Same shape as crates/markrivet-cli/tests/scaling.rs."""
    head = ["---", "status: active", "entity: synth", "owner: owner", "---", "",
            "# Synth", "", "## Action", ""]
    body = []
    for i in range(max(3, lines // 2 // 4)):
        tag = "`agent` " if i % 3 == 0 else ""
        body += [f"- [ ] {tag}**Action {i} title. (2026-09-{1 + i % 28:02d})** Context line for item {i}",
                 f"      with a continuation about widget {i}.", f"      **Gate**: check {i} passes.", ""]
    body += ["## Log", ""]
    for j in range(max(1, lines // 2 // 6)):
        body += [f"### 2026-08-{1 + j % 28:02d}: event {j}", "",
                 f"Entry {j} paragraph one with some words about the change.",
                 "A second line in the same paragraph.", "", ""]
    return "\n".join(head + body) + "\n"


def load():
    with open("/proc/loadavg") as f:
        return float(f.read().split()[0])


def io_pressure():
    try:
        with open("/proc/pressure/io") as f:
            for line in f:
                if line.startswith("some"):
                    return float(line.split()[1].split("=")[1])
    except OSError:
        pass
    return None


def run(binary, root, cmd, req) -> "tuple[int, dict, float]":
    started = time.perf_counter()
    p = subprocess.run([binary, "--root", root, cmd], input=json.dumps(req).encode(),
                       capture_output=True)
    took = time.perf_counter() - started
    out: dict
    try:
        out = json.loads(p.stdout)
    except ValueError:
        out = {"raw": p.stdout[:400].decode(errors="replace"),
               "stderr": p.stderr[:400].decode(errors="replace")}
    return p.returncode, out, took


def ok(binary, root, cmd, req) -> "tuple[dict, float]":
    code, out, took = run(binary, root, cmd, req)
    if code != 0:
        sys.exit(f"{binary} {cmd} exited {code}: {json.dumps(out)[:600]}")
    return out, took


def project(work, name, lines, stamp=True, binary=None):
    root = os.path.join(work, name)
    if os.path.exists(root):
        shutil.rmtree(root)
    shutil.copytree(os.path.join(EXAMPLE, ".rivet"), os.path.join(root, ".rivet"))
    os.makedirs(os.path.join(root, "tracking"))
    with open(os.path.join(root, PATH), "w") as f:
        f.write(tracker(lines))
    if stamp:
        m, _ = ok(binary, root, "map", {"path": PATH})
        ok(binary, root, "stamp", {"path": PATH, "guard": m["revision"]})
    return root


def items(binary, root):
    """Every action as (id, revision), paging through query."""
    out, cursor = [], None
    while True:
        q, _ = ok(binary, root, "query", {"collection": "actions", "paths": [PATH],
                                          "limit": 1000, "cursor": cursor})
        out += [(i["id"], i["revision"]) for i in q["items"]]
        cursor = q.get("next_cursor")
        if not cursor:
            return out


def stats(xs):
    xs = sorted(xs)
    p90 = xs[max(0, -(-len(xs) * 9 // 10) - 1)]
    return {"n": len(xs), "min_ms": round(xs[0] * 1000, 1),
            "median_ms": round(statistics.median(xs) * 1000, 1),
            "p90_ms": round(p90 * 1000, 1), "max_ms": round(xs[-1] * 1000, 1)}


def span(xs):
    xs = [x for x in xs if x is not None]
    return [min(xs), max(xs)] if xs else None


class Cell:
    """One (binary, size): a stamped project for reads and writes, and a
    second project re-stamped from scratch for each `stamp` sample."""

    def __init__(self, work, label, binary, lines):
        self.label, self.binary, self.lines = label, binary, lines
        self.root = project(work, f"{label}-{lines}", lines, binary=binary)
        self.stamp_root = project(work, f"{label}-{lines}-stamp", lines, stamp=False)
        self.bytes = os.path.getsize(os.path.join(self.root, PATH))
        with open(os.path.join(self.root, PATH)) as f:
            self.stays = f.read().count("<!-- stay:")
        self.id, self.guard = items(binary, self.root)[-1]
        self.done = True
        self.samples = {op: [] for op in ("map", "query", "get", "update_item_preview",
                                          "update_item", "stamp")}
        self.loads, self.io = [], []

    def update(self, preview):
        req = {"path": PATH, "collection": "actions", "id": self.id, "guard": self.guard,
               "set": {"done": self.done}}
        if preview:
            req["preview"] = True
        out, took = ok(self.binary, self.root, "update-item", req)
        if not preview:
            self.guard = next(op["revision"] for op in out["ops"] if op.get("id") == self.id)
            self.done = not self.done
        return took

    def rep(self):
        self.loads.append(load())
        self.io.append(io_pressure())
        b, r = self.binary, self.root
        self.samples["map"].append(ok(b, r, "map", {"path": PATH})[1])
        self.samples["query"].append(ok(b, r, "query", {"collection": "actions", "paths": [PATH]})[1])
        self.samples["get"].append(ok(b, r, "get", {"path": PATH,
                                                    "select": {"kind": "id", "id": self.id}})[1])
        self.samples["update_item_preview"].append(self.update(True))
        self.samples["update_item"].append(self.update(False))
        with open(os.path.join(self.stamp_root, PATH), "w") as f:
            f.write(tracker(self.lines))
        m, _ = ok(b, self.stamp_root, "map", {"path": PATH})
        self.samples["stamp"].append(ok(b, self.stamp_root, "stamp",
                                        {"path": PATH, "guard": m["revision"]})[1])
        self.loads.append(load())
        self.io.append(io_pressure())

    def row(self):
        return {"binary": self.label, "lines": self.lines, "bytes": self.bytes,
                "stays": self.stays, "load_1m": span(self.loads), "io_some_avg10": span(self.io),
                **{op: stats(xs) for op, xs in self.samples.items()}}


def single(args, binaries, work):
    cells = [Cell(work, label, path, lines) for lines in args.sizes for label, path in binaries]
    for rep in range(args.reps):
        for cell in cells:
            cell.rep()
        print(f"single: rep {rep + 1}/{args.reps} done, load {load()}", file=sys.stderr, flush=True)
    rows = [c.row() for c in cells]
    return rows, ratios(rows)


def ratios(rows):
    """Each binary's cost at its largest size over each smaller size."""
    out = []
    for label in dict.fromkeys(r["binary"] for r in rows):
        mine = sorted((r for r in rows if r["binary"] == label), key=lambda r: r["lines"])
        z = mine[-1]
        for a in mine[:-1]:
            for op in ("map", "update_item", "stamp"):
                out.append({"binary": label, "op": op, "from_lines": a["lines"],
                            "to_lines": z["lines"],
                            "median_ratio": round(z[op]["median_ms"] / a[op]["median_ms"], 1),
                            "min_ratio": round(z[op]["min_ms"] / a[op]["min_ms"], 1)})
    return out


def concurrent(args, binaries, work):
    rows = []
    for label, binary in binaries:
        for lines in args.writer_sizes:
            for k in args.writers:
                root = project(work, f"{label}-{lines}-w{k}", lines, binary=binary)
                targets = items(binary, root)[:k]
                load_before, io_before = load(), io_pressure()

                def writer(target):
                    iid, guard = target
                    done, out = True, []
                    for _ in range(args.writes):
                        code, o, took = run(binary, root, "update-item",
                                            {"path": PATH, "collection": "actions", "id": iid,
                                             "guard": guard, "set": {"done": done}})
                        error = (o.get("error") or {}).get("code") if isinstance(o, dict) else None
                        out.append((code, took, error))
                        if code == 0:
                            guard = next(op["revision"] for op in o["ops"] if op.get("id") == iid)
                            done = not done
                        else:
                            g, _ = ok(binary, root, "get", {"path": PATH,
                                                            "select": {"kind": "id", "id": iid}})
                            guard = g["typed"]["actions"]["revision"]
                    return out

                started = time.perf_counter()
                with cf.ThreadPoolExecutor(k) as pool:
                    results = [x for r in pool.map(writer, targets) for x in r]
                wall = time.perf_counter() - started
                outcomes = {}
                for code, _, error in results:
                    key = "ok" if code == 0 else f"exit {code}: {error}"
                    outcomes[key] = outcomes.get(key, 0) + 1
                row = {"binary": label, "lines": lines, "writers": k, "writes": len(results),
                       "wall_s": round(wall, 2), "writes_per_s": round(len(results) / wall, 2),
                       "per_write": stats([t for _, t, _ in results]),
                       "per_write_ok": stats([t for c, t, _ in results if c == 0])
                       if any(c == 0 for c, _, _ in results) else None,
                       "outcomes": outcomes, "load_1m": [load_before, load()],
                       "io_some_avg10": span([io_before, io_pressure()])}
                rows.append(row)
                print(f"concurrent: {label} {lines} lines, {k} writers: {outcomes}, "
                      f"wall {row['wall_s']} s", file=sys.stderr, flush=True)
                shutil.rmtree(root)
    return rows


def ints(text):
    return [int(x) for x in text.split(",") if x]


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--bin", action="append", required=True, metavar="LABEL=PATH",
                    help="a prebuilt rivet binary to time; repeat to compare builds")
    ap.add_argument("--work", required=True, help="scratch directory for the synthetic projects")
    ap.add_argument("--out", required=True, help="results JSON to write")
    ap.add_argument("--reps", type=int, default=7, help="single-writer repetitions (default 7)")
    ap.add_argument("--sizes", type=ints, default=[1000, 3000, 10000, 30000],
                    help="tracker sizes in lines (default 1000,3000,10000,30000)")
    ap.add_argument("--writers", type=ints, default=[1, 4, 8],
                    help="concurrent writer counts (default 1,4,8)")
    ap.add_argument("--writer-sizes", type=ints, default=[3000, 30000],
                    help="tracker sizes for the concurrent rows (default 3000,30000)")
    ap.add_argument("--writes", type=int, default=10, help="writes per concurrent writer (default 10)")
    ap.add_argument("--concurrent-bin", action="append", default=[], metavar="LABEL",
                    help="run the concurrent rows for this --bin label (repeatable; none by default)")
    ap.add_argument("--note", default="", help="free text stored with the results")
    args = ap.parse_args()
    binaries = []
    for spec in args.bin:
        label, _, path = spec.partition("=")
        if not label or not os.access(path, os.X_OK):
            ap.error(f"--bin {spec!r}: need LABEL=PATH to an executable")
        binaries.append((label, os.path.abspath(path)))
    unknown = set(args.concurrent_bin) - {label for label, _ in binaries}
    if unknown:
        ap.error(f"--concurrent-bin names no --bin label: {sorted(unknown)}")
    os.makedirs(args.work, exist_ok=True)
    started = time.time()
    rows, ratios = single(args, binaries, args.work)
    conc = concurrent(args, [b for b in binaries if b[0] in args.concurrent_bin], args.work)
    result = {
        "suite": "benchmark/latency/bench.py",
        "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(started)),
        "finished_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "host": {"cpus": os.cpu_count(), "kernel": platform.release(), "shared": True},
        "shape": "synthetic tracker: frontmatter, ## Action with lines/8 three-line checkbox items, "
                 "## Log with lines/12 ### sections, every item and section stamped",
        "reps": args.reps, "note": args.note,
        "binaries": [{"label": label, "file": os.path.basename(path)} for label, path in binaries],
        "single": rows, "ratios": ratios, "concurrent": conc,
    }
    with open(args.out, "w") as f:
        json.dump(result, f, indent=1)
        f.write("\n")
    for r in ratios:
        print(f"{r['binary']} {r['op']} {r['from_lines']}->{r['to_lines']}: "
              f"median ratio {r['median_ratio']}, min ratio {r['min_ratio']}")


if __name__ == "__main__":
    main()
