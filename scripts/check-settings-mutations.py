#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Prove settings coverage rejects four real source omissions at a commit."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


def run(args, cwd, **kwargs):
    return subprocess.run(args, cwd=cwd, text=True, check=True, **kwargs)


def replace_one(path, pattern, replacement):
    before = path.read_text()
    after, count = re.subn(pattern, replacement, before, count=0)
    if count != 1:
        raise RuntimeError(f"mutation expected one source match, found {count}: {path.name}")
    path.write_text(after)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--repo', type=Path, default=Path.cwd())
    parser.add_argument('--revision', default='HEAD')
    parser.add_argument('--target', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    repo = args.repo.resolve()
    revision = run(['git', 'rev-parse', args.revision], repo, capture_output=True).stdout.strip()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ, CARGO_TARGET_DIR=str(args.target.resolve()))
    host = 'crates/prismattyc-host/src/'
    trials = [
        ('missing-control', host + 'settings/controls.rs',
         r'\s*row\(\s*Section::Spaces,\s*"space_rail_pane_names",\s*Kind::Bool,?\s*\),', '',
         'settings::tests::schema_matches_controls_reached_through_real_rows_and_editors'),
        ('missing-writer', host + 'config_document.rs',
         r'(pub\(crate\) fn set_value\([\s\S]*?\) -> Result<\(\)> \{)',
         r'\1\n    if path.len() == 1 && path[0] == "font_px" { return Ok(()); }',
         'settings::tests::every_setting_saves_literal_values_through_input_and_loads_them'),
        ('missing-choice', host + 'settings/controls.rs',
         r'(Kind::Choice\(values\))',
         'Kind::Choice(values.into_iter().filter(|value| value != "light-cycle").collect())',
         'settings::tests::plain_arrows_reach_every_finite_choice'),
        ('new-unmapped-field', host + 'config.rs',
         r'(pub settings_panel: Option<bool>,)',
         r'\1\n    pub coverage_probe: Option<bool>,',
         'settings::tests::schema_matches_controls_reached_through_real_rows_and_editors'),
    ]
    results = []
    with tempfile.TemporaryDirectory(prefix='prism-settings-mutations-') as temp:
        worktree = Path(temp) / 'source'
        run(['git', 'worktree', 'add', '--detach', str(worktree), revision], repo, capture_output=True)
        try:
            baseline = ['cargo', '+1.90.0', 'test', '-p', 'prismattyc-host', '--locked', 'settings::tests', '--', '--test-threads=1']
            with (out / 'baseline.log').open('w') as log:
                run(baseline, worktree, env=env, stdout=log, stderr=subprocess.STDOUT)
            for name, relative, pattern, replacement, selector in trials:
                source = worktree / relative
                original = source.read_bytes()
                try:
                    replace_one(source, pattern, replacement)
                    command = ['cargo', '+1.90.0', 'test', '-p', 'prismattyc-host', '--locked', selector, '--', '--exact', '--test-threads=1']
                    result = subprocess.run(command, cwd=worktree, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
                    (out / f'{name}.log').write_text(result.stdout)
                    killed = result.returncode != 0 and 'test result: FAILED. 0 passed; 1 failed;' in result.stdout
                    results.append({'mutation': name, 'test': selector, 'exit_code': result.returncode, 'caught_by_assertion': killed})
                    (out / 'result.json').write_text(json.dumps({'revision': revision, 'trials': results}, indent=2) + '\n')
                    if not killed:
                        raise RuntimeError(f'{name} did not fail its behavioral assertion; inspect its log')
                    print(f'{name}: caught by failing behavioral assertion', flush=True)
                finally:
                    source.write_bytes(original)
        finally:
            run(['git', 'worktree', 'remove', '--force', str(worktree)], repo, capture_output=True)
    print(json.dumps({'status': 'PASS', 'revision': revision, 'mutations_caught': len(results)}))


if __name__ == '__main__':
    main()
