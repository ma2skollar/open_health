"""Regression and parity checks for the AAD mobile export (requires local models)."""
import tempfile
import unittest
from pathlib import Path

import torch

from export_mobile import MODELS, repair_activity_empty_peaks


class MobileActivityTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        path = MODELS / "automatic_activity_detection_3_1_11.pt"
        if not path.exists():
            raise unittest.SkipTest("Local activity model is not installed")
        cls.original = torch.jit.load(str(path)).eval()
        cls.repaired = repair_activity_empty_peaks(torch.jit.load(str(path)).eval())
        cls.directory = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.directory.cleanup)
        mobile_path = Path(cls.directory.name) / "activity.ptl"
        cls.repaired._save_for_lite_interpreter(str(mobile_path))
        # Round-trip the archive with full TorchScript for numerical parity.
        # StabilityTests executes its bytecode in the actual iOS lite runtime.
        cls.exported = torch.jit.load(str(mobile_path)).eval()

    @staticmethod
    def inputs(sparse):
        nan = float("nan")
        rows = 1 if sparse else 720
        return (
            torch.tensor([2026, 9, 8, 1], dtype=torch.float32),
            torch.tensor([30, 1, 1.78, 75] + [nan] * 10),
            torch.tensor([[i, 5.0 if 300 <= i < 360 else 1.2] for i in range(720)]),
            torch.tensor([[0] + [nan] * 11, [719] + [nan] * 11]),
            torch.tensor([[i, 0, 30, 0, 0, 0, nan, 10, 1] for i in range(rows)], dtype=torch.float32),
            torch.tensor([[i, 33.0] for i in range(rows)]),
            torch.tensor([[i, 70.0] for i in range(rows)]),
            None, None, torch.tensor(0.5), torch.tensor(10.0), torch.tensor(0.0),
        )

    def test_sparse_day_no_longer_throws(self):
        with torch.no_grad():
            with self.assertRaisesRegex(RuntimeError, "Input must be of ints, floats, or bools"):
                self.original(*self.inputs(sparse=True))
            for result in (self.repaired(*self.inputs(sparse=True)),
                           self.exported(*self.inputs(sparse=True))):
                self.assertEqual(tuple(result[0].shape), (0, 9))

    def test_complete_day_keeps_original_results(self):
        with torch.no_grad():
            expected = self.original(*self.inputs(sparse=False))
            for actual in (self.repaired(*self.inputs(sparse=False)),
                           self.exported(*self.inputs(sparse=False))):
                torch.testing.assert_close(actual, expected, rtol=0, atol=0, equal_nan=True)


if __name__ == "__main__":
    unittest.main()
