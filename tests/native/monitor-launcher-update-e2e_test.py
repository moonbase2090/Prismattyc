#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Fetching the older release for monitor-launcher-update-e2e.py.

A cached release is reused with no request. Rate limits are retried, waiting
as long as GitHub asks, and other refusals are not. The token goes only to the
API request, never to the asset download.

Run: python3 tests/native/monitor-launcher-update-e2e_test.py
"""
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import urllib.error

spec = importlib.util.spec_from_file_location(
    "monitor_launcher_update_e2e", Path(__file__).with_name("monitor-launcher-update-e2e.py"))
e2e = importlib.util.module_from_spec(spec)
spec.loader.exec_module(e2e)

VERSION = '0.3.52'
TARGET = 'x86_64-unknown-linux-gnu'
NAME = f'prismattyc-v{VERSION}-{TARGET}-pmux'
BODY = b'old pmux bytes'


def metadata(body=BODY):
    return json.dumps({
        'tag_name': f'v{VERSION}',
        'immutable': True,
        'draft': False,
        'prerelease': False,
        'assets': [{
            'name': NAME,
            'size': len(body),
            'digest': 'sha256:' + hashlib.sha256(body).hexdigest(),
            'browser_download_url': f'https://github.com/{e2e.REPOSITORY}/releases/download/v{VERSION}/{NAME}',
        }],
    }).encode()


class Response(io.BytesIO):
    def __init__(self, data, url):
        super().__init__(data)
        self.url = url

    def geturl(self):
        return self.url

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def http_error(code, headers):
    return urllib.error.HTTPError('https://api.github.com/x', code, 'error', headers, io.BytesIO())


class FakeGitHub:
    """Serves the release API and asset download, recording each request."""

    def __init__(self, failures=()):
        self.failures = list(failures)
        self.requests = []

    def __call__(self, request, timeout):
        url = getattr(request, 'full_url', request)
        headers = dict(getattr(request, 'header_items', lambda: [])())
        self.requests.append((url, headers))
        if self.failures:
            raise self.failures.pop(0)
        if url.startswith('https://api.github.com/'):
            return Response(metadata(), url)
        return Response(BODY, url)


class FetchOld(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.sleeps = []
        patcher = mock.patch.object(e2e.time, 'sleep', self.sleeps.append)
        patcher.start()
        self.addCleanup(patcher.stop)

    def fetch(self, github, out, cache=None, env=None):
        out.mkdir()
        with mock.patch.object(e2e.urllib.request, 'urlopen', github), \
                mock.patch.dict(os.environ, env or {}, clear=True):
            return e2e.fetch_old(VERSION, TARGET, out, cache)

    def test_a_cached_release_is_reused_without_a_request(self):
        cache = self.root / 'cache'
        first = FakeGitHub()
        path = self.fetch(first, self.root / 'one', cache)
        self.assertEqual(path.read_bytes(), BODY)
        self.assertEqual(len(first.requests), 2)
        second = FakeGitHub()
        path = self.fetch(second, self.root / 'two', cache)
        self.assertEqual(path.read_bytes(), BODY)
        self.assertEqual(second.requests, [], 'a cache hit must not call GitHub')
        self.assertTrue((self.root / 'two' / 'old-release.json').is_file())

    def test_a_corrupt_cached_binary_is_fetched_again(self):
        cache = self.root / 'cache'
        self.fetch(FakeGitHub(), self.root / 'one', cache)
        (cache / f'v{VERSION}' / NAME).write_bytes(b'tampered')
        again = FakeGitHub()
        path = self.fetch(again, self.root / 'two', cache)
        self.assertEqual(path.read_bytes(), BODY)
        self.assertEqual(len(again.requests), 2)
        self.assertEqual((cache / f'v{VERSION}' / NAME).read_bytes(), BODY)

    def test_the_token_goes_only_to_the_api_request(self):
        github = FakeGitHub()
        self.fetch(github, self.root / 'out', env={'GITHUB_TOKEN': 'secret-token'})
        (api_url, api_headers), (asset_url, asset_headers) = github.requests
        self.assertTrue(api_url.startswith('https://api.github.com/'))
        self.assertEqual(api_headers.get('Authorization'), 'Bearer secret-token')
        self.assertTrue(asset_url.startswith('https://github.com/'))
        self.assertNotIn('Authorization', asset_headers)

    def test_a_rate_limited_api_call_is_retried_after_the_reset(self):
        limited = http_error(403, {'x-ratelimit-remaining': '0', 'x-ratelimit-reset': '1030'})
        github = FakeGitHub(failures=[limited])
        with mock.patch.object(e2e.time, 'time', return_value=1000):
            path = self.fetch(github, self.root / 'out')
        self.assertEqual(path.read_bytes(), BODY)
        self.assertEqual(self.sleeps, [31])

    def test_a_plain_refusal_is_not_retried(self):
        github = FakeGitHub(failures=[http_error(403, {'x-ratelimit-remaining': '42'})])
        with self.assertRaises(urllib.error.HTTPError):
            self.fetch(github, self.root / 'out')
        self.assertEqual(self.sleeps, [])
        self.assertEqual(len(github.requests), 1)


class RetryWait(unittest.TestCase):
    def test_retry_after_and_server_errors_wait_and_are_capped(self):
        self.assertEqual(e2e.retry_wait(http_error(429, {'Retry-After': '7'}), 0), 7)
        self.assertEqual(e2e.retry_wait(http_error(403, {'Retry-After': '900'}), 0), e2e.FETCH_MAX_WAIT_SECONDS)
        self.assertEqual(e2e.retry_wait(http_error(502, {}), 2), 4)
        self.assertEqual(e2e.retry_wait(urllib.error.URLError('reset'), 1), 2)

    def test_client_errors_other_than_rate_limits_give_up(self):
        self.assertIsNone(e2e.retry_wait(http_error(404, {}), 0))
        self.assertIsNone(e2e.retry_wait(http_error(403, {}), 0))


if __name__ == '__main__':
    unittest.main()
