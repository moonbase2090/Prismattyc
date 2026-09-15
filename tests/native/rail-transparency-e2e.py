#!/usr/bin/env python3
"""Check Space rail alpha and hot reload in an isolated native test container."""
import importlib.util
import json
import time
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    'host_ux', Path(__file__).with_name('host-ux-e2e.py'))
ux = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ux)


def alpha_at(frame, x, y):
    assert frame.bpp == 4, 'present capture must retain framebuffer alpha'
    return frame.rows[y][x * 4 + 3]


def main():
    ux.OUT.mkdir(parents=True, exist_ok=True)
    result = {'status': 'FAIL', 'cases': []}
    host = None
    try:
        ux.wait_for(lambda: 'status: running' in ux.run('pmux', 'status'), 'test daemon')
        for name in ('astra', 'kiro-rail', 'quota-1', 'quota-2'):
            ux.run('pmux', 'new', name, '--no-attach', '--', 'bash', '--noprofile', '--norc')
        for name, sessions in [('PRISMATTYC', ['astra']), ('Nexus', ['kiro-rail']),
                               ('QUOTA', ['quota-1', 'quota-2'])]:
            ux.run('pmux', 'space', 'save', name, *sessions)
        base = ('theme = "prismattyc-default"\nfont_px = 16.0\n'
                'space_rail = "left"\nspace_rail_width_cols = 28\n'
                'space_rail_pane_names = true\nspace_startup = "fresh"\n'
                'window_opacity = 0.65\nwindow_blur = true\n')
        host = ux.Host('rail', [], base + 'chrome_opacity = 0.4\n')
        ux.run('pmux', 'space', 'open', 'PRISMATTYC', '--no-attach')
        ux.wait_for(lambda: host.status()['space'] == 'PRISMATTYC', 'active Space')
        for opacity, expected in [(0.4, 102), (0.75, 191), (1.0, 255), (0.4, 102)]:
            (host.directory / 'config.toml').write_text(base + f'chrome_opacity = {opacity}\n')
            name = f'opacity-{len(result["cases"])}-{expected}'

            def check():
                pixels, meta = host.capture(name)
                frame = ux.PNG(pixels)
                chips = [c for c in host.status()['space_chips'] if c['name']]
                if len(chips) != 3:
                    return None
                samples = []
                for chip in chips:
                    x, y, w, h = (chip[k] for k in ('x', 'y', 'width', 'height'))
                    ground = alpha_at(frame, x + 1, y + 1)
                    # The second row holds session names. Measure its right edge,
                    # inside the text box and away from the close button.
                    name_ground = alpha_at(frame, x + w - 30, y + h // 2 + 3)
                    if (ground, name_ground) != (expected, expected):
                        return None
                    ink = [alpha_at(frame, xx, yy)
                           for yy in range(y + h // 2, y + h - 3)
                           for xx in range(x + 4, x + w - 30)]
                    assert 255 in ink, f'session-name ink missing: {chip["name"]}'
                    samples.append({'space': chip['name'], 'ground': ground,
                                    'name_ground': name_ground})
                current = next(c for c in chips if c['name'] == 'PRISMATTYC')
                x, y, h = current['x'], current['y'], current['height']
                top, bottom = frame.pixel(x + 1, y + 1), frame.pixel(x + 1, y + h - 4)
                if expected < 255 and top == bottom:
                    return None  # The open request can precede its first painted frame.
                return {'opacity': opacity, 'samples': samples, 'active_top': top,
                        'active_bottom': bottom, 'frame_seq': meta['seq']}

            result['cases'].append(ux.wait_for(check, f'rail opacity {opacity}', timeout=15))
        time.sleep(2)
        idle = ux.wait_for(check, 'stable idle rail', timeout=15)
        assert idle['active_top'] == result['cases'][-1]['active_top']
        assert idle['active_bottom'] == result['cases'][-1]['active_bottom']
        result['idle'] = idle
        host.display('rail')
        result['status'] = 'PASS'
        print('RAIL_TRANSPARENCY_E2E_COMPLETE: rail, session names, and opacity hot reload PASS')
    except Exception as error:
        result['error'] = str(error)
        raise
    finally:
        if host is not None:
            host.stop()
        (ux.OUT / 'result.json').write_text(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
