"""Synthetic frozen-prediction admission; no benchmark task or model call."""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts/pageindex_baseline'))
import frozen_rejudge as subject


class FrozenRejudgeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.origin = self.root / 'origin'
        self.baseline = self.root / 'baseline'
        self.benchmark = self.root / 'questions.json'
        self.judge_source = self.root / 'judge-source'
        (self.judge_source / 'eval').mkdir(parents=True)
        (self.judge_source / 'eval/judge.py').write_text(
            "MODEL='synthetic-default'\nEFFORT='high'\nMAX_RESPONSE_CHARS=12000\n"
            "SCHEMA={'type':'object','properties':{'equivalent':{'type':'boolean'},'abstained':{'type':'boolean'},'reason':{'type':'string'}},'required':['equivalent','abstained','reason'],'additionalProperties':False}\n"
            "PROMPT='Question: {question}\\nReference: {answer}\\nFormat: {answer_format}\\nResponse: {response}'\n")
        self.binary = self.root / 'synthetic-binary'
        self.binary.write_text('#!/bin/sh\nexit 0\n')
        self.binary.chmod(0o700)
        self.price_card = self.root / 'price-card.json'
        self.price_card.write_text('{"synthetic_price_card":true}')
        self.codex_bin = self.root / 'synthetic-codex'
        self.codex_bin.write_text('#!/bin/sh\nexit 0\n')
        self.codex_bin.chmod(0o700)
        self.codex_home = self.root / 'account'
        self.codex_home.mkdir()
        questions = [{'question': 'synthetic question A', 'answer': 'synthetic answer A', 'answer_format': 'Str'},
                     {'question': 'synthetic question B', 'answer': 'synthetic answer B', 'answer_format': 'Str'}]
        self.benchmark.write_text(json.dumps(questions))
        self.original_results = self.root / 'results.json'
        self.original_results.write_text(json.dumps([{'chat_model': 'gpt-5.6-luna',
            'reasoning_effort': 'high', 'correct': 1, 'total': 2, 'avg_cost_usd': 0.003607}]))
        answers = []
        for number, source in enumerate((self.origin, self.baseline)):
            path = source / f'full/attempts/question-{number:03d}/0001.json'
            path.parent.mkdir(parents=True)
            case = {**questions[number], 'source_row': number, 'status': 'completed',
                    'identity': f'synthetic-identity-{number}', 'variant': 'full', 'doc_id': f'synthetic-doc-{number}',
                    'response': f'synthetic response {number}', 'host_call_ordinals': [number + 1],
                    'judge': {'status': 'completed', 'equivalent': True}}
            path.write_text(json.dumps(case))
            entry = {'source_row': number, 'status': 'completed', 'identity': case['identity'], 'attempt_record_count': 1}
            if number == 0:
                entry['outcome_import'] = {'origin_case_path': f'full/attempts/question-{number:03d}/0001.json',
                                           'origin_case_sha256': subject.sha(path)}
            answers.append(entry)
        self.summary = self.baseline / 'summary.json'
        self.summary.write_text(json.dumps({'index_origin': {'run_dir': str(self.origin)},
                                            'summary': {'full': {'question_denominator': 2}}, 'answers': answers}))
        self.args = argparse.Namespace(baseline_summary=self.summary, original_results=self.original_results,
            questions=self.benchmark,
            judge_source=self.judge_source, binary=self.binary, price_card=self.price_card,
            codex_bin=str(self.codex_bin),
            codex_home=self.codex_home, judge_model='gpt-6-luna', judge_effort='max', service_tier='fast',
            max_calls=80, concurrency=5, timeout=180)

    def test_complete_frozen_predictions_bind_hashes_without_text_in_plan(self):
        plan = subject.expected_plan(self.args, expected_count=2)
        self.assertEqual(plan['question_denominator'], 2)
        self.assertEqual([row['source_kind'] for row in plan['rows']], ['imported_retained', 'local_retained'])
        self.assertEqual(len({row['response_sha256'] for row in plan['rows']}), 2)
        raw = subject.encoded(plan)
        self.assertNotIn(b'synthetic question', raw)
        self.assertNotIn(b'synthetic answer', raw)
        self.assertNotIn(b'synthetic response', raw)
        self.assertEqual(plan['plan_sha256'], hashlib.sha256(subject.encoded({k: v for k, v in plan.items()
                                                                                if k != 'plan_sha256'})).hexdigest())

    def test_tampered_imported_response_and_missing_row_fail_closed(self):
        imported = self.origin / 'full/attempts/question-000/0001.json'
        case = json.loads(imported.read_text())
        case['response'] = 'changed response'
        imported.write_text(json.dumps(case))
        with self.assertRaisesRegex(ValueError, 'frozen_imported_case_changed'):
            subject.expected_plan(self.args, expected_count=2)
        self.summary.write_text(json.dumps({'index_origin': {'run_dir': str(self.origin)},
                                            'summary': {'full': {'question_denominator': 2}}, 'answers': []}))
        with self.assertRaisesRegex(ValueError, 'frozen_cohort_incomplete'):
            subject.expected_plan(self.args, expected_count=2)

    def test_path_traversal_and_wrong_profile_reject(self):
        with self.assertRaisesRegex(ValueError, 'frozen_case_path_invalid'):
            subject.within(self.origin, '../escape.json')
        self.args.judge_model = 'gpt-5.6-luna'
        with self.assertRaisesRegex(ValueError, 'frozen_rejudge_profile_invalid'):
            subject.expected_plan(self.args, expected_count=2)

    def test_judge_only_run_retains_completed_verdicts_on_resume(self):
        self.args.run_dir = self.root / 'rejudge'
        self.args.run_dir.mkdir()
        self.args.retry_failed = False
        self.args.resume = False
        plan = subject.expected_plan(self.args, expected_count=2)
        seen = []

        def fake_process(arguments, cwd, timeout, *, cancelled=None, on_start=None):
            self.assertEqual(arguments[1], 'host-complete')
            self.assertEqual(arguments[arguments.index('--model') + 1], 'gpt-6-luna')
            request_path = Path(arguments[arguments.index('--input') + 1])
            request = json.loads(request_path.read_text())
            self.assertEqual(request['state'], {})
            self.assertIn('Response:', request['instructions'])
            ordinal = int(request_path.name.split('.')[0])
            if on_start:
                on_start(800000 + ordinal)
            seen.append(ordinal)
            report = {'status': 'completed', 'model': 'gpt-6-luna', 'requested_reasoning_effort': 'max',
                      'effective_reasoning_effort': 'max', 'requested_service_tier': 'fast',
                      'effective_service_tier': 'priority', 'auth_mode': 'chatgpt', 'model_provider': 'openai',
                      'thread_id': f'synthetic-judge-{ordinal}', 'turn_id': f'synthetic-turn-{ordinal}',
                      'usage': {'total_tokens': 11},
                      'value': {'equivalent': ordinal == 1, 'abstained': False, 'reason': 'synthetic'}}
            return subprocess.CompletedProcess(arguments, 0, subject.encoded(report), b'')

        with patch('bridge.owned_process', side_effect=fake_process):
            result = subject.run(self.args, plan)
        self.assertEqual((result['status'], result['judged'], result['correct']), ('completed', 2, 1))
        self.assertEqual(result['host_invocations'], 2)
        self.assertEqual(len(seen), 2)
        self.args.resume = True
        with patch('bridge.owned_process', side_effect=fake_process):
            resumed = subject.run(self.args, plan)
        self.assertEqual((resumed['judged'], resumed['correct'], resumed['host_invocations']), (2, 1, 2))
        self.assertEqual(len(seen), 2, 'completed verdicts must not be resampled')

    def test_cli_plan_repeat_and_run_keep_exact_immutable_plan(self):
        self.args.run_dir = self.root / 'cli-run'
        plan = subject.expected_plan(self.args, expected_count=2)
        arguments = ['frozen_rejudge.py', '--stage', 'plan', '--baseline-summary', str(self.summary),
                     '--original-results', str(self.original_results), '--questions', str(self.benchmark),
                     '--judge-source', str(self.judge_source), '--run-dir', str(self.args.run_dir),
                     '--binary', str(self.binary), '--price-card', str(self.price_card),
                     '--codex-bin', str(self.codex_bin), '--codex-home', str(self.codex_home),
                     '--judge-model', 'gpt-6-luna', '--judge-effort', 'max', '--service-tier', 'fast']
        actual_plan = subject.expected_plan
        fresh_plan = lambda parsed: actual_plan(parsed, expected_count=2)
        with patch.object(sys, 'argv', arguments), patch.object(subject, 'expected_plan', side_effect=fresh_plan):
            self.assertEqual(subject.main(), 0)
            self.assertEqual(subject.main(), 0)
        self.assertEqual(json.loads((self.args.run_dir / 'plan.json').read_text()), plan)
        result = {'status': 'completed', 'question_denominator': 2, 'judged': 2, 'correct': 1,
                  'unavailable': 0, 'host_invocations': 2}
        arguments[arguments.index('plan')] = 'run'
        with patch.object(sys, 'argv', arguments), patch.object(subject, 'expected_plan', side_effect=fresh_plan), \
                patch.object(subject, 'run', return_value=result) as execute:
            self.assertEqual(subject.main(), 0)
            execute.assert_called_once()


if __name__ == '__main__':
    unittest.main()
