#!/usr/bin/env python3
"""Judge one frozen PageIndex adapter prediction cohort without running PageIndex.

The original PageIndex OSS results.json is aggregate-only. This tool binds the
separate live adapter's retained per-task responses and runs only a new judge.
It never indexes, searches, reads pages, or asks PageIndex to answer again.
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import sys
from types import SimpleNamespace

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from bridge import LocalCodex
import locks
from role_hosts import RoleHost
from run import checkpoint_attempt, judge_case, judge_constants, private_directory, run_case_pool, write_json


def sha(path: Path) -> str:
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def encoded(value) -> bytes:
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(',', ':'), allow_nan=False).encode()


def within(root: Path, relative: str) -> Path:
    if not isinstance(relative, str) or not relative or Path(relative).is_absolute() or '..' in Path(relative).parts:
        raise ValueError('frozen_case_path_invalid')
    path = root / relative
    if path.is_symlink() or not path.is_file() or not path.resolve().is_relative_to(root.resolve()):
        raise ValueError('frozen_case_path_unavailable')
    return path


def frozen_cases(summary_path: Path, questions_path: Path, *, expected_count: int = 62) -> list[dict]:
    summary = locks.read_json(summary_path)
    questions = locks.read_json(questions_path)
    answers = summary.get('answers')
    if (not isinstance(answers, list) or not isinstance(questions, list)
            or len(answers) != expected_count or len(questions) != expected_count
            or summary.get('summary', {}).get('full', {}).get('question_denominator') != expected_count):
        raise ValueError('frozen_cohort_incomplete')
    origin = Path(summary['index_origin']['run_dir']).resolve()
    baseline_root = summary_path.parent.resolve()
    rows = []
    seen = set()
    for item in answers:
        ordinal = item.get('source_row')
        if type(ordinal) is not int or not 0 <= ordinal < expected_count or ordinal in seen:
            raise ValueError('frozen_row_identity_invalid')
        seen.add(ordinal)
        imported = item.get('outcome_import')
        if imported is not None:
            path = within(origin, imported['origin_case_path'])
            if sha(path) != imported.get('origin_case_sha256'):
                raise ValueError('frozen_imported_case_changed')
            source_kind = 'imported_retained'
        else:
            attempt = item.get('attempt_record_count')
            if type(attempt) is not int or attempt < 1 or attempt > 10000:
                raise ValueError('frozen_local_attempt_invalid')
            path = within(baseline_root, f'full/attempts/question-{ordinal:03d}/{attempt:04d}.json')
            source_kind = 'local_retained'
        case = locks.read_json(path)
        row = questions[ordinal]
        if (case.get('source_row') != ordinal or case.get('identity') != item.get('identity')
                or case.get('status') != 'completed' or item.get('status') != 'completed'
                or case.get('judge', {}).get('status') != 'completed'
                or not isinstance(case.get('response'), str) or not case['response'].strip()
                or any(case.get(key) != row.get(key) for key in ('question', 'answer', 'answer_format'))):
            raise ValueError('frozen_prediction_or_reference_changed')
        rows.append({'source_row': ordinal, 'case_path': str(path), 'case_sha256': sha(path),
                     'source_kind': source_kind, 'reader_identity': case['identity'],
                     'response_sha256': hashlib.sha256(case['response'].encode()).hexdigest()})
    if seen != set(range(expected_count)):
        raise ValueError('frozen_cohort_rows_missing')
    return sorted(rows, key=lambda row: row['source_row'])


def expected_plan(args, *, expected_count: int = 62) -> dict:
    summary, questions, judge_source, binary, price_card = (path.expanduser().resolve() for path in
        (args.baseline_summary, args.questions, args.judge_source, args.binary, args.price_card))
    original_results = args.original_results.expanduser().resolve()
    if (args.judge_model != 'gpt-6-luna' or args.judge_effort not in ('xhigh', 'max')
            or args.service_tier != 'fast'):
        raise ValueError('frozen_rejudge_profile_invalid')
    launcher = args.codex_bin
    if os.sep in launcher:
        launcher = str(Path(launcher).expanduser().resolve())
        if not Path(launcher).is_file() or not os.access(launcher, os.X_OK):
            raise ValueError('frozen_rejudge_launcher_unavailable')
    constants = judge_constants(judge_source)
    rows = frozen_cases(summary, questions, expected_count=expected_count)
    original = [row for row in locks.read_json(original_results)
                if row.get('chat_model') == 'gpt-5.6-luna' and row.get('reasoning_effort') == 'high']
    if len(original) != 1 or original[0].get('total') != expected_count:
        raise ValueError('original_oss_reference_unavailable')
    plan = {'schema_version': 'gptgrep.frozen-rejudge-plan.v1',
            'scope': 'judge_only_frozen_adapter_predictions_no_pageindex_calls',
            'source': {'baseline_summary_sha256': sha(summary), 'questions_sha256': sha(questions),
                       'judge_source_sha256': sha(judge_source / 'eval/judge.py'), 'binary_sha256': sha(binary),
                       'script_sha256': sha(Path(__file__)), 'original_oss_results_sha256': sha(original_results),
                       'price_card_sha256': sha(price_card),
                       'original_oss_reference_role': 'separate_aggregate'},
            'original_oss_aggregate': {'chat_model': 'gpt-5.6-luna', 'effort': 'high',
                                       'correct': original[0]['correct'], 'total': original[0]['total'],
                                       'avg_answer_cost_usd': original[0]['avg_cost_usd'],
                                       'predictions_available_for_rejudge': False},
            'judge': {'model': args.judge_model, 'effort': args.judge_effort, 'tier': args.service_tier,
                      'rubric_sha256': hashlib.sha256(constants['PROMPT'].encode()).hexdigest(),
                      'schema_sha256': hashlib.sha256(encoded(constants['SCHEMA'])).hexdigest()},
            'launcher': launcher, 'codex_home': str(args.codex_home.expanduser().resolve()),
            'max_calls': args.max_calls, 'concurrency': args.concurrency, 'timeout': args.timeout,
            'question_denominator': expected_count, 'rows': rows}
    plan['plan_sha256'] = hashlib.sha256(encoded(plan)).hexdigest()
    return plan


def run(args, plan: dict) -> dict:
    run_dir = args.run_dir.expanduser().resolve()
    cases_dir = run_dir / 'cases'
    cases_dir.mkdir(parents=True, exist_ok=True)
    if (run_dir / 'host-calls.jsonl').exists() and not args.resume:
        raise ValueError('frozen_rejudge_existing_calls_require_resume')
    source_cases = []
    for item in plan['rows']:
        source = Path(item['case_path'])
        if sha(source) != item['case_sha256']:
            raise ValueError('frozen_source_case_changed')
        case = copy.deepcopy(locks.read_json(source))
        case.pop('judge', None)
        target = cases_dir / f"question-{item['source_row']:03d}.json"
        if target.exists():
            saved = locks.read_json(target)
            if any(saved.get(key) != case.get(key) for key in ('source_row', 'identity', 'response', 'question', 'answer', 'answer_format')):
                raise ValueError('frozen_saved_case_changed')
            case = saved
        source_cases.append(case)
    profile = {'roles': {'judge': {'model': args.judge_model, 'reasoning_effort': args.judge_effort,
                                   'service_tier': args.service_tier}}}
    host = LocalCodex(args.binary.expanduser().resolve(), plan['launcher'], args.codex_home.expanduser().resolve(),
                      run_dir, args.max_calls, timeout=args.timeout, model=args.judge_model,
                      effort=args.judge_effort, host_concurrency=args.concurrency,
                      judge_concurrency=args.concurrency, service_tier=args.service_tier)
    intervals = []
    try:
        options = SimpleNamespace(stage='judge', retry_failed=args.retry_failed)
        constants = judge_constants(args.judge_source.expanduser().resolve())
        results = run_case_pool(source_cases,
                                lambda answer: judge_case(answer, args=options, profile=profile,
                                                          constants=constants, host=host, variant_dir=cases_dir),
                                args.concurrency, host, 'judge', intervals)
    finally:
        host.close(cancel=True)
    completed = sum(item.get('judge', {}).get('status') == 'completed' for item in results)
    correct = sum(item.get('judge', {}).get('equivalent') is True for item in results)
    return {'schema_version': 'gptgrep.frozen-rejudge-result.v1', 'plan_sha256': plan['plan_sha256'],
            'status': 'completed' if completed == len(results) else 'incomplete',
            'source': plan['source'], 'judge': plan['judge'], 'question_denominator': len(results),
            'original_oss_aggregate': plan['original_oss_aggregate'],
            'judged': completed, 'correct': correct, 'unavailable': len(results) - completed,
            'host_invocations': len(host.calls), 'host_calls_by_status': {
                state: sum(call.get('status') == state for call in host.calls)
                for state in ('completed', 'failed', 'interrupted')},
            'per_row': [{'source_row': item['source_row'], 'status': item.get('judge', {}).get('status'),
                         'equivalent': item.get('judge', {}).get('equivalent') if item.get('judge', {}).get('status') == 'completed' else None,
                         'case_sha256': plan['rows'][item['source_row']]['case_sha256']}
                        for item in results],
            'limitations': {'original_oss_predictions_rejudged': False,
                            'pageindex_index_or_reader_rerun': False,
                            'source': 'separate R8 live adapted PageIndex predictions',
                            'actual_account_billing_usd': None}}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--stage', choices=('plan', 'run'), required=True)
    parser.add_argument('--baseline-summary', type=Path, required=True)
    parser.add_argument('--original-results', type=Path, required=True)
    parser.add_argument('--questions', type=Path, required=True)
    parser.add_argument('--judge-source', type=Path, required=True)
    parser.add_argument('--run-dir', type=Path, required=True)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--price-card', type=Path, required=True)
    parser.add_argument('--codex-bin', required=True)
    parser.add_argument('--codex-home', type=Path, required=True)
    parser.add_argument('--judge-model', default='gpt-6-luna')
    parser.add_argument('--judge-effort', choices=('xhigh', 'max'), default='max')
    parser.add_argument('--service-tier', default='fast')
    parser.add_argument('--max-calls', type=int, default=80)
    parser.add_argument('--concurrency', type=int, default=5)
    parser.add_argument('--timeout', type=int, default=180)
    parser.add_argument('--resume', action='store_true')
    parser.add_argument('--retry-failed', action='store_true')
    args = parser.parse_args()
    if not 62 <= args.max_calls <= 256 or not 1 <= args.concurrency <= 16 or not 1 <= args.timeout <= 900:
        parser.error('invalid bounded judge call settings')
    run_dir = args.run_dir.expanduser().resolve()
    private_directory(run_dir)
    plan = expected_plan(args)
    path = run_dir / 'plan.json'
    if args.stage == 'plan':
        if path.exists():
            if locks.read_json(path) != plan:
                raise ValueError('frozen_rejudge_plan_changed')
        else:
            write_json(path, plan)
        print(json.dumps({'status': 'plan_prepared', 'question_count': plan['question_denominator'],
                          'plan_sha256': plan['plan_sha256'], 'model_calls': 0}))
        return 0
    if not path.is_file() or locks.read_json(path) != plan:
        raise ValueError('frozen_rejudge_plan_missing_or_changed')
    result = run(args, plan)
    checkpoint_attempt(run_dir / 'result.json', result)
    print(json.dumps({key: result[key] for key in ('status', 'question_denominator', 'judged', 'correct', 'unavailable', 'host_invocations')}))
    return 0 if result['status'] == 'completed' else 2


if __name__ == '__main__':
    raise SystemExit(main())
