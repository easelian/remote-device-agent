#!/usr/bin/env python3
"""Summarize every D1 alternating batch; no sample filtering or retry selection."""
import argparse
import json
import math
from pathlib import Path


def percentile(values, fraction):
    return sorted(values)[math.ceil(len(values) * fraction) - 1]


def report(root):
    batches = {}
    failures = []
    for path in sorted(root.glob('*-performance.jsonl')):
        rows = [json.loads(line) for line in path.read_text().splitlines()]
        samples = [row for row in rows if row['kind'] == 'sample']
        failures.extend([{'batch': path.stem, **row} for row in samples if 'error_code' in row])
        operations = {}
        for operation in ['observe', 'click', 'screenshot']:
            warmup = [row for row in samples if row['operation'] == operation and row['phase'] == 'warmup']
            measured = [row for row in samples if row['operation'] == operation and row['phase'] == 'measured']
            assert len(warmup) == 20 and len(measured) == 100, (path, operation)
            values = [row['elapsed_ms'] for row in measured]
            operations[operation] = {'count': len(measured), 'p50_ms': percentile(values, .5), 'p95_ms': percentile(values, .95),
                'commands_p95': percentile([row['commands'] for row in measured], .95)}
        batches[path.name.removesuffix('-performance.jsonl')] = operations
    pairs = []
    for number in range(1, 4):
        for operation in ['observe', 'click', 'screenshot']:
            base, candidate = (batches[f'{build}-{number}'][operation] for build in ['baseline', 'candidate'])
            p50 = candidate['p50_ms'] - base['p50_ms']
            p95 = candidate['p95_ms'] - base['p95_ms']
            commands = candidate['commands_p95'] / base['commands_p95']
            passed = p50 <= max(base['p50_ms'] * .1, 5) and p95 <= max(base['p95_ms'] * .15, 10) and commands <= 1.2
            pairs.append({'batch': number, 'operation': operation, 'p50_delta_ms': p50, 'p95_delta_ms': p95, 'command_ratio': commands, 'passed': passed})
    variance = []
    for operation in ['observe', 'click', 'screenshot']:
        for metric, ratio, floor in [('p50_ms', .1, 5), ('p95_ms', .15, 10)]:
            values = [batches[f'baseline-{number}'][operation][metric] for number in range(1, 4)]
            span = max(values) - min(values)
            variance.append({'operation': operation, 'metric': metric, 'span_ms': span, 'allowed_ms': max(min(values) * ratio, floor), 'passed': span <= max(min(values) * ratio, floor)})
    environment = json.loads((root/'environment.json').read_text())
    size = environment['binary_bytes']
    delta = size['candidate'] - size['baseline']
    passed = not failures and all(p['passed'] for p in pairs) and all(v['passed'] for v in variance) and delta <= max(size['baseline']*.15, 2*1024**2)
    return {'performance_status': 'passed' if passed else 'failed_or_insufficient_measurement_conditions', 'batches': batches, 'pairs': pairs,
        'baseline_variance': variance, 'failed_samples': failures, 'binary': {'bytes': size, 'delta_bytes': delta, 'allowed_delta_bytes': max(size['baseline']*.15, 2*1024**2)}}


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('evidence', type=Path)
    args = parser.parse_args()
    print(json.dumps(report(args.evidence), indent=2))
