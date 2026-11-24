"""Real fresh-process reproducibility and corpus-target checks."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from gen_docs import build_doc, ntok
import random


class CorpusTest(unittest.TestCase):
    def test_maximum_target_and_rejection_above_it(self):
        _, stats = build_doc("doc_0000", 100000, random.Random(42))
        self.assertLess(abs(stats["actual_tokens"] - 100000), 300)
        with tempfile.TemporaryDirectory() as tmp:
            result = subprocess.run(
                [sys.executable, str(Path(__file__).with_name("gen_docs.py")),
                 "--output-dir", tmp, "--target-tokens", "100001"],
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn("between 1 and 100000", result.stderr)

    def test_fresh_processes_and_measured_rows(self):
        with tempfile.TemporaryDirectory() as tmp:
            outputs = []
            for i, hashseed in enumerate(("1", "98765")):
                output = Path(tmp) / str(i)
                subprocess.run(
                    [sys.executable, str(Path(__file__).with_name("gen_docs.py")),
                     "--output-dir", str(output), "--count", "3", "--seed", "42"],
                    env={**os.environ, "PYTHONHASHSEED": hashseed},
                    check=True, capture_output=True,
                )
                outputs.append({p.name: p.read_bytes() for p in output.iterdir()})
            self.assertEqual(outputs[0], outputs[1])
            manifest = json.loads(outputs[0]["manifest.json"])
            self.assertEqual(len({m["seed"] for m in manifest}), 3)
            for stats in manifest:
                source = outputs[0][stats["doc_id"] + ".md"].decode()
                self.assertEqual(stats["actual_tokens"], ntok(source))
                table = source.split("## Component Status\n\n")[1].split("\n\n")[0]
                rows = table.splitlines()[2:]
                labels = [row.split("|")[1].strip() for row in rows]
                self.assertEqual(len(labels), len(set(labels)))
                self.assertEqual(labels.count(stats["row_target"]), 1)
                self.assertEqual(stats["one_table_row_tokens"], ntok(rows[0]))
                self.assertEqual(len({row.split("|")[3].strip() for row in rows}), 5)


if __name__ == "__main__":
    unittest.main()
