#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Non-window regressions for the native fixture's evidence boundaries."""
import importlib.util
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('settings_native_fixture',Path(__file__).with_name('macos-settings-e2e.py'))
fixture=importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class FixtureBoundaries(unittest.TestCase):
    def test_doctor_requires_recent_owned_successful_post_action_frame(self):
        value={'host_pid':77,'windows':[{'focused':True,'last_raster':{'unix_ms':9500,'present_succeeded':True}}]}
        self.assertEqual(fixture.checked_window_status(value,77,10000,9000)['last_raster']['unix_ms'],9500)
        for pid,now,after in [(88,10000,9000),(77,20000,9000),(77,10000,9500),(77,5000,0)]:
            with self.subTest(pid=pid,now=now,after=after),self.assertRaises(AssertionError):
                fixture.checked_window_status(value,pid,now,after)
        stale={'host_pid':77,'windows':[{'last_raster':{'unix_ms':1,'present_succeeded':True}}]}
        with self.assertRaisesRegex(AssertionError,'stale'):
            fixture.checked_window_status(stale,77,10000)
        failed={'host_pid':77,'windows':[{'last_raster':{'unix_ms':9500,'present_succeeded':False}}]}
        with self.assertRaisesRegex(AssertionError,'did not present'):
            fixture.checked_window_status(failed,77,10000)

    def test_clipboard_restore_failure_preserves_failure_and_command_evidence(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp)
            with patch.object(fixture.subprocess,'run',side_effect=OSError('clipboard unavailable')):
                with self.assertRaisesRegex(RuntimeError,'clipboard restore: clipboard unavailable'):
                    fixture.finish_evidence(root,{'status':'FAIL','error':'original drive failed'},[{'args':['render-status'],'exit':0}],[{'file':'owned.png','owner_pid':77}],None,None,b'previous clipboard')
            result=json.loads((root/'result.json').read_text())
            self.assertEqual(result['status'],'FAIL');self.assertEqual(result['error'],'original drive failed')
            self.assertEqual(result['cleanup_errors'],['clipboard restore: clipboard unavailable'])
            self.assertTrue(result['host_reaped']);self.assertTrue(result['daemon_reaped'])
            self.assertEqual(json.loads((root/'commands.json').read_text()),[{'args':['render-status'],'exit':0}])
            self.assertEqual(json.loads((root/'captures.json').read_text()),[{'file':'owned.png','owner_pid':77}])

    def test_accessibility_action_rechecks_permission_and_owned_focus(self):
        for missing in ['permission','focus']:
            with self.subTest(missing=missing):
                desktop=object.__new__(fixture.Desktop)
                trusted=[True];mutations=[]
                def attribute(node,key):
                    if key=='AXTitle':
                        if missing=='permission':trusted[0]=False
                        return 'Settings: Save Validate the complete edit'
                    return {'AXFrontmost':False,'AXFocusedWindow':4}.get(key)
                desktop.attr=attribute;desktop.text=lambda value:value;desktop.string=lambda text:text
                desktop.cls=lambda value:value;desktop.sel=lambda value:value
                desktop.msg=lambda *args:1;desktop.utf8=lambda *args:b'fixture'
                desktop.ax=SimpleNamespace(AXUIElementCreateApplication=lambda pid:1,AXIsProcessTrusted=lambda:trusted[0],AXUIElementPerformAction=lambda *args:mutations.append(args) or 0)
                desktop.cf=SimpleNamespace(CFRetain=lambda node:node,CFRelease=lambda value:None,CFBooleanGetValue=lambda value:value)
                expected=RuntimeError if missing=='permission' else AssertionError
                message='Accessibility permission' if missing=='permission' else 'not frontmost'
                with self.assertRaisesRegex(expected,message):desktop.press_row(77,': Save ')
                self.assertEqual(mutations,[])

    def test_cleanup_success_retains_pass_and_all_records(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp)
            with patch.object(fixture.subprocess,'run') as clipboard:
                fixture.finish_evidence(root,{'status':'PASS'},[],[],None,None,b'prior bytes')
            clipboard.assert_called_once_with(['/usr/bin/pbcopy'],input=b'prior bytes',check=True)
            self.assertEqual(json.loads((root/'result.json').read_text()),{'status':'PASS','host_reaped':True,'daemon_reaped':True})
            self.assertEqual(json.loads((root/'commands.json').read_text()),[])
            self.assertEqual(json.loads((root/'captures.json').read_text()),[])


if __name__=='__main__':unittest.main()
