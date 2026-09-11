import copy
import unittest

from summarize import timing, valid_run


class SummaryTests(unittest.TestCase):
    def test_quantiles(self):
        self.assertEqual(timing([2.0, 1.0]), (1.5, 2.0, 2.0))
        self.assertEqual(timing(list(range(1, 21))), (10.5, 19, 20))

    def test_incomplete_or_invalid_results_are_not_successful(self):
        run = {"rounds": 2}
        rows = [{"kind": "holder"}, {"kind": "complete", "block_hash_unchanged": True,
                                    "invalid_samples": 0}]
        groups = {(1, variant): [{"round": index, "valid": True} for index in range(2)]
                  for variant in ("sequential", "concurrent")}
        self.assertTrue(valid_run(rows, run, groups))
        self.assertFalse(valid_run(rows[:-1], run, groups))
        self.assertFalse(valid_run(rows, run, {}))
        self.assertFalse(valid_run(rows, run, {(1, "sequential"): groups[(1, "sequential")]}))
        run_with_batching = {"rounds": 2, "variants": ["sequential", "concurrent", "batched"]}
        self.assertFalse(valid_run(rows, run_with_batching, groups))
        with_batching = {**groups, (1, "batched"): copy.deepcopy(groups[(1, "concurrent")])}
        self.assertTrue(valid_run(rows, run_with_batching, with_batching))
        for change in ("invalid", "duplicate_round", "missing_sample", "reorg", "no_holders",
                       "invalid_samples_recorded"):
            with self.subTest(change=change):
                altered_rows, altered_groups = copy.deepcopy((rows, groups))
                samples = altered_groups[(1, "concurrent")]
                if change == "invalid":
                    samples[0]["valid"] = False
                elif change == "duplicate_round":
                    samples[1]["round"] = 0
                elif change == "missing_sample":
                    samples.pop()
                elif change == "reorg":
                    altered_rows[-1]["block_hash_unchanged"] = False
                elif change == "no_holders":
                    altered_rows.pop(0)
                else:
                    altered_rows[-1]["invalid_samples"] = 1
                self.assertFalse(valid_run(altered_rows, run, altered_groups))


if __name__ == "__main__":
    unittest.main()
