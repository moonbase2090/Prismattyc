#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Generate the native animation workload independently of capture I/O."""
import argparse
import json
from pathlib import Path
import select
import socket
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fd', type=int, required=True)
    parser.add_argument('--images', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    records = []
    report = {'status': 'FAIL', 'frequency_hz': 30, 'requested': 150, 'records': records}
    try:
        with socket.socket(fileno=args.fd) as control:
            start = time.monotonic_ns()
            report['start_ns'] = start
            print(json.dumps({'event': 'pacing-start', 'start_ns': start}), flush=True)
            for generation in range(1, 151):
                scheduled = start + (generation - 1) * 1_000_000_000 // 30
                while (remaining := scheduled - time.monotonic_ns()) > 0:
                    # This timer defines the workload rate, not readiness or success.
                    try:
                        select.select([], [], [], remaining / 1_000_000_000)
                    except InterruptedError:
                        continue
                before = time.monotonic_ns()
                command = f'load 1 {generation} 64 64 1 1 {args.images / f"animation-{generation}.rgba"}\n'
                control.sendall(command.encode())
                records.append({'generation': generation, 'scheduled_ns': scheduled,
                                'before_send_ns': before, 'sent_ns': time.monotonic_ns()})
        report['missed_intervals'] = sum(row['before_send_ns'] - row['scheduled_ns'] >= 1_000_000_000 // 30 for row in records)
        gaps = [right['sent_ns'] - left['sent_ns'] for left, right in zip(records, records[1:])]
        report.update(generation_span_ns=records[-1]['sent_ns'] - records[0]['sent_ns'],
                      min_send_gap_ns=min(gaps), max_send_gap_ns=max(gaps),
                      max_lateness_ns=max(row['before_send_ns'] - row['scheduled_ns'] for row in records))
        report['status'] = 'PASS' if len(records) == 150 and not report['missed_intervals'] and 4_900_000_000 <= report['generation_span_ns'] <= 5_050_000_000 else 'FAIL'
    except BaseException as error:
        report['error'] = repr(error)
        raise
    finally:
        report['generated'] = len(records)
        args.out.write_text(json.dumps(report, indent=2) + '\n')
    return 0 if report['status'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
