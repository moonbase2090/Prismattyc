#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Break one behavior at a time, require its real test to fail, then restore."""
import argparse
import hashlib
import json
from pathlib import Path
import signal
import subprocess

base = Path(__file__).parent
crate = base / 'native-client'
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--out', type=Path, required=True)
args = parser.parse_args()
out = args.out.resolve()
out.mkdir(parents=True, exist_ok=False)
signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(SystemExit(143)))
cases = [
    ('ring-full', 'ring.rs', 'self.published - consumed >= 4', 'self.published - consumed >= 5', 'interoperability', 'actual_lunatui_encoder_publishes_wide_clusters_into_host_owned_ring'),
    ('crop-origin', 'frame.rs', '(16, crop.origin.x)', '(16, 0)', 'interoperability', 'real_image_geometry_encodes_exact_cover_crop_depth_opacity_and_metrics'),
    ('absolute-deadline', 'control.rs', 'if now >= op.deadline {', 'if now >= op.deadline + Duration::from_millis(1) {', 'control', 'partial_response_never_renews_the_absolute_operation_deadline'),
    ('wrong-client', 'control.rs', 'return Err(crate::invalid("response identity"));', '/* negative control: ignore the wrong client identity */', 'control', 'a_reply_stolen_or_replaced_by_another_client_fails_identity_without_recovery_scan'),
    ('pending-suppression', 'images.rs', '(0, handle) if handle != 0 => Resource::Pending(handle)', '(0, handle) if handle != 0 => Resource::Ready(handle)', 'image_fallback', 'actual_widget_fallback_survives_pending_and_decline_and_disappears_only_after_ready'),
    ('source-digest', 'backing.rs', 'digest.update(chunk);', 'digest.update(&vec![0; chunk.len()]);', 'discovery_backing', 'a_real_prepared_platform_object_is_accepted_and_read_by_the_host'),
    ('buffered-poll', 'session.rs', None, None, 'discovery_backing', 'a_complete_capability_reply_is_read_without_waiting_for_already_buffered_bytes'),
    ('ended-rearm', 'control.rs', '            if opcode == 14 {', '            if opcode == 14 { return Err(crate::invalid("native session ended")); }\n            if opcode == 14 {', 'control', 'client_and_actual_host_activate_expire_release_and_rearm_at_a_new_epoch'),
    ('legacy-pong', 'control.rs', '7 if self.legacy => (8, 32),', '7 if self.legacy => (15, 48),', 'legacy', 'legacy_control_accepts_frozen_pong_and_rejects_image_active_flags'),
    ('legacy-active', 'control.rs', 'let active_flags = if legacy { 0 } else { 2 };', 'let active_flags = if legacy { 2 } else { 2 };', 'legacy', 'legacy_control_accepts_frozen_pong_and_rejects_image_active_flags'),
]
results = []
for name, filename, before, after, target, test in cases:
    path = crate / 'src' / filename
    original = path.read_bytes()
    (out / f'{name}-source.rs').write_bytes(original)
    source = original.decode()
    if name == 'buffered-poll':
        marker = '    let mut bytes = Vec::new();\n'
        assert source.count(marker) == 1
        source = source.replace(marker, marker + '    let mut buffered = std::io::BufReader::new(std::fs::File::from(descriptor.try_clone_to_owned()?));\n')
        start = source.index('        let n = unsafe {\n            libc::read(')
        end = source.index('        };', start) + len('        };')
        source = source[:start] + '        let n = std::io::Read::read(&mut buffered, &mut chunk[..1])? as isize;' + source[end:]
    else:
        assert source.count(before) == 1, name
        source = source.replace(before, after)
    try:
        path.write_text(source)
        completed = subprocess.run(['cargo', '+1.90.0', 'test', '--manifest-path', str(crate / 'Cargo.toml'), '--locked', '--target-dir', str(out / 'target'), '--test', target, test, '--', '--exact'], capture_output=True, text=True, timeout=90)
        log = completed.stdout + completed.stderr
        (out / f'{name}.log').write_text(log)
        detected = completed.returncode != 0 and 'test result: FAILED' in log
        results.append({'control': name, 'detected': detected, 'exit_code': completed.returncode})
        assert detected, name + ' did not fail its test'
    finally:
        path.write_bytes(original)
        assert hashlib.sha256(path.read_bytes()).digest() == hashlib.sha256(original).digest()
(out / 'result.json').write_text(json.dumps(results, indent=2))
print(json.dumps(results), flush=True)
