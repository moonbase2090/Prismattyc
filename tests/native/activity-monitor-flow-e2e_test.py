#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Replay the native activity flow's pane identity and coordinate boundaries."""
import importlib.util
import ctypes
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "activity_flow", Path(__file__).with_name("activity-monitor-flow-e2e.py"))
flow = importlib.util.module_from_spec(spec)
spec.loader.exec_module(flow)


class MonitorPointer(unittest.TestCase):
    def test_pane_chrome_is_excluded_from_terminal_cell_coordinates(self):
        status = {
            "current_panes": [{"pane_id": "PaneId(3)", "remote_pane_id": 2}],
            "visible_pane_slots": [[3, [228, 96, 747, 546]]],
            "visible_pane_content_rects": [[3, [241, 141, 720, 483]]],
        }
        pane = {"id": 2, "geometry": {"cols": 80, "rows": 23}}
        self.assertEqual(
            flow.monitor_cell_point(status, pane, (100, 200, 983, 650),
                                    (983, 650), 20, 6),
            (525, 477))

    def test_daemon_pane_id_is_resolved_through_the_host_attachment(self):
        status = {
            "current_panes": [{"pane_id": "PaneId(3)", "remote_pane_id": 2}],
            "visible_pane_content_rects": [[3, [228, 96, 747, 546]]],
        }
        pane = {"id": 2, "geometry": {"cols": 83, "rows": 26}}
        self.assertEqual(
            flow.monitor_cell_point(status, pane, (100, 200, 980, 630),
                                    (980, 630), 20, 5),
            (512, 411))

    def test_render_pixels_are_scaled_into_native_client_points(self):
        status = {
            "current_panes": [{"pane_id": "PaneId(3)", "remote_pane_id": 3}],
            "visible_pane_content_rects": [[3, [228, 96, 747, 546]]],
        }
        pane = {"id": 3, "geometry": {"cols": 83, "rows": 26}}
        self.assertEqual(
            flow.monitor_cell_point(status, pane, (100, 200, 980, 630),
                                    (1960, 1260), 20, 5),
            (306, 305))

    def test_an_unattached_remote_pane_cannot_borrow_a_local_id(self):
        status = {
            "current_panes": [{"pane_id": "PaneId(2)", "remote_pane_id": 7}],
            "visible_pane_content_rects": [[2, [228, 96, 747, 546]]],
        }
        pane = {"id": 2, "geometry": {"cols": 83, "rows": 26}}
        with self.assertRaisesRegex(flow.Failed, "no visible host attachment"):
            flow.monitor_cell_point(status, pane, (100, 200, 980, 630), (980, 630), 20, 5)


class InheritedFlagsQuartz:
    """An OS event sink whose newly allocated events inherit held Shift."""
    def __init__(self):
        self.posted = []

    def CGEventCreateMouseEvent(self, source, kind, point, button):
        return {"kind": kind, "point": point, "flags": flow.MAC_FLAGS["shift"]}

    def CGEventCreateScrollWheelEvent2(self, source, units, count, y, x, z):
        return {"kind": 22, "delta": y, "flags": flow.MAC_FLAGS["shift"]}

    def CGEventSetFlags(self, event, flags):
        event["flags"] = flags

    def CGEventGetFlags(self, event):
        return event["flags"]

    def CGEventGetIntegerValueField(self, event, field):
        return event["delta"]

    def CGEventSetLocation(self, event, point):
        event["point"] = point

    def CGEventPost(self, tap, event):
        self.posted.append(dict(event))

    def CFRelease(self, event):
        pass


class MacPointerModifiers(unittest.TestCase):
    def test_plain_wheel_after_shift_right_click_emits_unmodified_events(self):
        adapter = flow.MacInput.__new__(flow.MacInput)
        adapter.quartz = InheritedFlagsQuartz()
        adapter.Point = lambda x, y: (x, y)
        adapter.diagnostics = {"events": []}
        with patch.object(flow.time, "sleep"):
            adapter.click(100, 200, button="right", shift=True)
            adapter.wheel(100, 200, -1)
        self.assertEqual(adapter.quartz.posted, [
            {"kind": 5, "point": (100, 200), "flags": 0},
            {"kind": 3, "point": (100, 200), "flags": 131072},
            {"kind": 4, "point": (100, 200), "flags": 131072},
            {"kind": 5, "point": (100, 200), "flags": 0},
            {"kind": 22, "delta": 1, "point": (100, 200), "flags": 0},
        ])


class NativeX11Geometry:
    def XOpenDisplay(self, name):
        return 1

    def XGetGeometry(self, display, window, root, x, y, width, height, border, depth):
        for pointer, value in [(root, 100), (x, 1), (y, 20), (width, 1066),
                               (height, 598), (border, 1), (depth, 24)]:
            pointer._obj.value = value
        return 1

    def XTranslateCoordinates(self, display, window, root, source_x, source_y, x, y, child):
        x._obj.value = 600 + source_x
        y._obj.value = 331 + source_y
        return 1

    def XCloseDisplay(self, display):
        return 0


class X11PointerOrigin(unittest.TestCase):
    def test_client_origin_does_not_add_window_manager_decoration_twice(self):
        adapter = flow.XdotoolInput.__new__(flow.XdotoolInput)
        adapter.c, adapter.x11, adapter.window = ctypes, NativeX11Geometry(), "4194306"
        old_geometry = "WINDOW=4194306\nX=601\nY=351\nWIDTH=1066\nHEIGHT=598\nSCREEN=0\n"
        with patch.object(flow.subprocess, "check_output", return_value=old_geometry):
            self.assertEqual(adapter.frame(), (600, 331, 1066, 598))


if __name__ == "__main__":
    unittest.main()
