#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Boundary cases for paste-e2e.py's frame-gap measurement (`window_gaps`).

A stall before the first frame after the paste key, or one that runs past the
end of the observation window, must count. A window that no frame bounds on
either side must fail rather than report a short gap.

Run: python3 tests/native/paste-e2e_test.py
"""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "paste_e2e", Path(__file__).with_name("paste-e2e.py"))
paste_e2e = importlib.util.module_from_spec(spec)
spec.loader.exec_module(paste_e2e)
window_gaps = paste_e2e.window_gaps


def steady(start, end, step=0.01):
    """Frames every `step` seconds from `start` up to `end`."""
    count = round((end - start) / step)
    return [round(start + i * step, 6) for i in range(count + 1)]


class WindowGaps(unittest.TestCase):
    def test_stall_before_the_first_frame_after_the_key_counts(self):
        # Review of 432404a: key at 10.0, nothing rendered until 10.25.
        frames = [9.99, 10.25, 10.26, 10.27, 10.28] + steady(10.29, 12.05)
        self.assertAlmostEqual(max(window_gaps(frames, 10.0, 12.0)), 0.26, places=6)

    def test_stall_across_the_end_of_the_window_counts(self):
        frames = steady(9.99, 11.9) + [12.2, 12.21]
        self.assertAlmostEqual(max(window_gaps(frames, 10.0, 12.0)), 0.3, places=6)

    def test_frames_that_stop_inside_the_window_fail(self):
        # The review's exact frames: no frame bounds the end of the window.
        with self.assertRaisesRegex(AssertionError, "do not span"):
            window_gaps([9.99, 10.25, 10.26, 10.27, 10.28], 10.0, 12.0)

    def test_no_frame_before_the_window_fails(self):
        with self.assertRaisesRegex(AssertionError, "do not span"):
            window_gaps(steady(10.05, 12.5), 10.0, 12.0)

    def test_steady_frames_report_the_frame_interval(self):
        gaps = window_gaps(steady(9.0, 13.0), 10.0, 12.0)
        self.assertAlmostEqual(max(gaps), 0.01, places=6)
        self.assertAlmostEqual(sum(gaps), 2.0, places=6)

    def test_frames_on_the_boundaries_bound_the_window(self):
        self.assertEqual(window_gaps([10.0, 10.5, 12.0], 10.0, 12.0), [0.5, 1.5])

    def test_unsorted_frames_are_ordered_first(self):
        self.assertEqual(window_gaps([12.5, 9.5, 11.0], 10.0, 12.0), [1.5, 1.5])


if __name__ == "__main__":
    unittest.main()
