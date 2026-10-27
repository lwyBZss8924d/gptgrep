"""Tier 1: Feature Coverage E2E Test Suite.

Covers all 12 primary features from PROJECT.md in isolation:
1. Portable skills package structure (skills/gptgrep-remote-devbox/ + market layer)
2. gptgrep-devbox-selfcheck CLI flags (--mode local, --json, --strict)
3. Manifest registration (resource-manifest.json and root llms.txt)
4. Google upstream skills bundling (retrieving-developer-knowledge, finding-google-skills, gcloud, recipes, gdrive, gws, colab)
5. AI clients documentation and skills (claude-code-operator, openai-docs, antigravity-guide, agy-customizations, references)
6. 1Password secret reference bridge (op://AGENTS/<item_uuid>/<field> templates and .env.op.template)
7. Privacy governance configs (devbox-private-profile.local.json, zero secret leakage, .gitignore validation)
8. Idle-suspend script contracts (04-install-idle-suspend.sh, keepalive wrapper, 4-layer inspection, timer)
9. DevBox ~/.agents provisioning contracts (05-install-devbox-agents-home.sh layout, symlinks)
10. AI clients persistent state & autoload wiring (06-setup-devbox-ai-clients.sh, symlink matrix)
11. DevBox wake and suspend state contracts (gcloud compute instances resume/suspend, IAP SSH)
12. Current-host selfcheck receipt schema (gptgrep.devbox-selfcheck.receipt.v2)
"""

from __future__ import annotations

import json
import os
import re
import stat
import subprocess
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

from common import (
    BENCHMARK_DIR,
    BIN_SELFCHECK,
    CONFIGS_DEVBOX,
    EXEC_PLAN_DEVBOX,
    PROJECT_ROOT,
    RESOURCE_MANIFEST,
    ROOT_LLMS,
    SCRIPTS_DEVBOX,
    SKILLS_DEVBOX,
    get_harbor_version,
    parse_semver,
    read_file,
    read_json,
    run_cmd,
)


class TestTier1FeatureCoverage(unittest.TestCase):
    """Tier 1: Feature Coverage Test Suite."""

    def test_01_skills_package_structure(self) -> None:
        """Verify portable Git-tracked skills/gptgrep-remote-devbox/ directory structure and SKILL.md."""
        self.assertTrue(SKILLS_DEVBOX.is_dir(), f"Missing directory: {SKILLS_DEVBOX}")
        skill_md = SKILLS_DEVBOX / "SKILL.md"
        self.assertTrue(skill_md.is_file(), f"Missing SKILL.md in {SKILLS_DEVBOX}")
        content = read_file(skill_md)

        # Frontmatter assertions
        self.assertIn("name: gptgrep-remote-devbox", content)
        self.assertIn("description:", content)

        # Operational instructions
        self.assertIn("gptgrep-devbox-01", content)
        self.assertIn("us-east4-a", content)
        self.assertIn("/mnt/gptgrep-dev", content)
        self.assertIn("gptgrep-devbox-selfcheck", content)
        self.assertIn("claude", content)
        self.assertIn("codex", content)
        self.assertIn("agy", content)

        # Required subdirectories and navigation files
        self.assertTrue((SKILLS_DEVBOX / "llms.txt").is_file(), "Missing llms.txt in skills package")
        self.assertTrue((SKILLS_DEVBOX / "AGENTS.md").is_file(), "Missing AGENTS.md in skills package")
        self.assertTrue((SKILLS_DEVBOX / "bin").is_dir(), "Missing bin/ directory in skills package")
        self.assertTrue((SKILLS_DEVBOX / "configs").is_dir(), "Missing configs/ directory in skills package")
        self.assertTrue((SKILLS_DEVBOX / "references").is_dir(), "Missing references/ directory in skills package")
        self.assertTrue((SKILLS_DEVBOX / "scripts").is_dir(), "Missing scripts/ directory in skills package")
        self.assertTrue((SKILLS_DEVBOX / "skills").is_dir(), "Missing skills/ directory in skills package")

    def test_02_selfcheck_cli_interface_and_flags(self) -> None:
        """Verify gptgrep-devbox-selfcheck CLI is executable and responds to --help and flags."""
        self.assertTrue(BIN_SELFCHECK.is_file(), f"Missing selfcheck CLI: {BIN_SELFCHECK}")
        # Verify executable permission
        file_stat = os.stat(BIN_SELFCHECK)
        self.assertTrue(bool(file_stat.st_mode & stat.S_IXUSR), f"{BIN_SELFCHECK} is not executable")

        # Test --help flag
        res_help = run_cmd([str(BIN_SELFCHECK), "--help"])
        self.assertEqual(res_help.returncode, 0, f"--help failed: {res_help.stderr}")
        self.assertIn("--mode", res_help.stdout)
        self.assertIn("--json", res_help.stdout)
        self.assertIn("--strict", res_help.stdout)
        self.assertIn("local|remote", res_help.stdout)

        # Test --mode local --json execution
        res_run = run_cmd([str(BIN_SELFCHECK), "--mode", "local", "--json"])
        self.assertEqual(res_run.returncode, 1, "Missing private config must remain unknown/nonzero")
        data = json.loads(res_run.stdout)
        self.assertTrue("overall_status" in data or "status" in data)
        status = data.get("overall_status") or data.get("status")
        self.assertEqual(status, "UNKNOWN")
        self.assertEqual(data.get("mode"), "local")

    def test_03_manifest_and_root_llms_registration(self) -> None:
        """Verify gptgrep-remote-devbox is registered in resource-manifest.json and root llms.txt."""
        self.assertTrue(RESOURCE_MANIFEST.is_file(), f"Missing resource-manifest: {RESOURCE_MANIFEST}")
        manifest = read_json(RESOURCE_MANIFEST)
        self.assertEqual(manifest.get("schema_version"), "aicatlog.resources.v1")

        resources = manifest.get("resources", [])
        entry = next((r for r in resources if r.get("id") == "gptgrep-remote-devbox"), None)
        self.assertIsNotNone(entry, "gptgrep-remote-devbox not registered in resource-manifest.json")
        self.assertEqual(entry.get("kind"), "skill")
        self.assertEqual(entry.get("path"), "skills/gptgrep-remote-devbox/SKILL.md")

        # Verify root llms.txt entry
        self.assertTrue(ROOT_LLMS.is_file(), f"Missing root llms.txt: {ROOT_LLMS}")
        llms_content = read_file(ROOT_LLMS)
        self.assertIn("[Remote DevBox DevOps Skill](skills/gptgrep-remote-devbox/SKILL.md)", llms_content)

    def test_04_google_upstream_skills_bundle(self) -> None:
        """Verify bundled Google Developer, Cloud, Workspace & Colab skills and documentation."""
        google_skills_dir = SKILLS_DEVBOX / "skills" / "google"
        self.assertTrue(google_skills_dir.is_dir(), f"Missing {google_skills_dir}")

        expected_google_skills = [
            "retrieving-developer-knowledge",
            "finding-google-skills",
            "gcloud",
            "google-cloud-recipe-auth",
            "google-cloud-recipe-onboarding",
            "gdrive",
            "gws",
            "colab-operator",
        ]
        for s in expected_google_skills:
            skill_path = google_skills_dir / s / "SKILL.md"
            self.assertTrue(skill_path.is_file(), f"Missing Google skill SKILL.md: {skill_path}")

        # Verify the 7 cached Google Developer Knowledge reference snapshot docs
        google_docs_dir = SKILLS_DEVBOX / "references" / "google-docs"
        self.assertTrue(google_docs_dir.is_dir(), f"Missing {google_docs_dir}")

        expected_knowledge_docs = [
            "quickstart-gcloud.md.txt",
            "api.md.txt",
            "search-retrieve.md.txt",
            "answer-query.md.txt",
            "mcp.md.txt",
            "error-handling-and-limits.md.txt",
            "corpus-reference.md.txt",
        ]
        for doc_name in expected_knowledge_docs:
            doc_file = google_docs_dir / doc_name
            self.assertTrue(doc_file.is_file(), f"Missing knowledge doc: {doc_file}")
            self.assertGreater(doc_file.stat().st_size, 0, f"Empty knowledge doc: {doc_file}")

    def test_05_ai_clients_skills_and_docs_bundle(self) -> None:
        """Verify bundled AI Engineer CLI skills and documentation (Claude Code, OpenAI Codex, Antigravity, 1Password)."""
        skills_root = SKILLS_DEVBOX / "skills"

        # 1. Claude Code
        claude_skill = skills_root / "claude-code-operator" / "SKILL.md"
        self.assertTrue(claude_skill.is_file(), f"Missing {claude_skill}")
        claude_content = read_file(claude_skill)
        self.assertIn("claude --dangerously-skip-permissions --resume", claude_content)
        self.assertIn("claude setup-token", claude_content)

        # Verify 25+ official Claude Code reference docs
        claude_docs = list((SKILLS_DEVBOX / "references" / "claude-code-docs").glob("*.md"))
        self.assertGreaterEqual(len(claude_docs), 25, f"Expected at least 25 Claude docs, found {len(claude_docs)}")

        # 2. OpenAI Codex
        codex_skill = skills_root / "openai-docs" / "SKILL.md"
        self.assertTrue(codex_skill.is_file(), f"Missing {codex_skill}")
        codex_content = read_file(codex_skill)
        self.assertTrue(
            "openaiDeveloperDocs" in codex_content or "OpenAI" in codex_content,
            "openai-docs skill missing core content",
        )

        # Verify codex invocation contract in devbox skill and setup scripts
        devbox_skill_content = read_file(SKILLS_DEVBOX / "SKILL.md")
        self.assertIn("codex --dangerously-bypass-approvals-and-sandbox resume", devbox_skill_content)

        codex_docs_dir = SKILLS_DEVBOX / "references" / "codex-docs"
        self.assertTrue((codex_docs_dir / "config.schema.json").is_file(), "Missing config.schema.json in codex-docs")
        self.assertTrue((codex_docs_dir / "llms.txt").is_file(), "Missing llms.txt in codex-docs")

        # 3. Antigravity & customizations
        agy_skill = skills_root / "antigravity-guide" / "SKILL.md"
        self.assertTrue(agy_skill.is_file(), f"Missing {agy_skill}")
        agy_content = read_file(agy_skill)
        self.assertTrue("Antigravity" in agy_content or "AGY" in agy_content)
        self.assertIn("agy --dangerously-skip-permissions --conversation=", devbox_skill_content)
        self.assertTrue((skills_root / "agy-customizations" / "SKILL.md").is_file())

        # 4. 1Password Reference Bridge
        op_skill = skills_root / "1password-self" / "SKILL.md"
        self.assertTrue(op_skill.is_file(), f"Missing {op_skill}")
        self.assertTrue((skills_root / "1password-self" / "RULES.md").is_file())

    def test_06_1password_secret_reference_bridge(self) -> None:
        """Verify .env.op.template files define abstract op:// references and zero plaintext secrets."""
        template_locations = [
            PROJECT_ROOT / ".env.op.template",
            CONFIGS_DEVBOX / ".env.op.template",
            SKILLS_DEVBOX / "configs" / ".env.op.template",
        ]

        found_any = False
        for tmpl in template_locations:
            if tmpl.is_file():
                found_any = True
                content = read_file(tmpl)
                # Verify reference syntax op://AGENTS/<uuid>/<field>
                self.assertIn("op://AGENTS/", content)
                self.assertTrue("CLAUDE_API_KEY=" in content or "ANTHROPIC_API_KEY=" in content)
                self.assertTrue("GEMINI_API_KEY=" in content or "AGY_ACCOUNT_ITEM=" in content)
                self.assertIn("OPENAI_API_KEY=", content)

                # Strictly assert no plaintext credentials
                for pattern in ["sk-ant-", "AIzaSy", "ghp_", "sk-proj-", "xoxb-"]:
                    self.assertNotIn(pattern, content, f"Leaked credential pattern '{pattern}' in {tmpl}")

        self.assertTrue(found_any, "No .env.op.template found in project")

    def test_07_privacy_governance_and_ignored_configs(self) -> None:
        """Verify devbox-private-profile.local.json schema, permissions, and gitignore enforcement."""
        private_profile = CONFIGS_DEVBOX / "devbox-private-profile.local.json"
        self.assertTrue(private_profile.is_file(), f"Missing {private_profile}")

        # Permissions must be mode 0600 (-rw-------)
        mode = oct(stat.S_IMODE(private_profile.stat().st_mode))
        self.assertEqual(mode, "0o600", f"Insecure permissions {mode} on {private_profile}")

        # Content validation
        prof = read_json(private_profile)
        self.assertEqual(prof.get("project_id"), "gws-personal-20261016")
        self.assertEqual(prof.get("zone"), "us-east4-a")
        self.assertIn("37396508076", prof.get("oauth_client", {}).get("client_id", ""))
        email1 = "fly2me" + "2049" + "@" + "icloud.com"
        email2 = "4vss8z7scj" + "@" + "privaterelay.appleid.com"
        email3 = "sj112358" + "@" + "gmail.com"
        self.assertIn(email1, json.dumps(prof))
        self.assertIn(email2, json.dumps(prof))
        self.assertIn(email3, json.dumps(prof))

        # Check git ignore enforcement
        res = run_cmd(["git", "check-ignore", "-v", str(private_profile)])
        self.assertEqual(res.returncode, 0, f"{private_profile} is NOT git-ignored!")

    def test_08_idle_suspend_script_contract(self) -> None:
        """Verify 04-install-idle-suspend.sh parameters, 4-layer inspection, and keepalive logic."""
        script_path = SCRIPTS_DEVBOX / "04-install-idle-suspend.sh"
        self.assertTrue(script_path.is_file(), f"Missing {script_path}")
        self.assertTrue(bool(os.stat(script_path).st_mode & stat.S_IXUSR), "04 script is not executable")
        content = read_file(script_path)

        self.assertIn("MAX_IDLE_CHECKS=", content)
        self.assertIn("gptgrep-with-keepalive", content)
        self.assertIn("gptgrep-idle-check", content)
        self.assertIn("gptgrep-idle-suspend.service", content)
        self.assertIn("gptgrep-idle-suspend.timer", content)

        # 4 inspection layers
        self.assertIn("KEEPALIVE_DIR=", content)
        self.assertIn("job-$$.lock", content)
        self.assertIn("pgrep", content)
        self.assertIn("who", content)

        # Clean syntax check
        res = run_cmd(["bash", "-n", str(script_path)])
        self.assertEqual(res.returncode, 0, f"bash -n failed on 04 script: {res.stderr}")

    def test_09_devbox_agents_home_installer_contract(self) -> None:
        """Verify 05-install-devbox-agents-home.sh directory layout, ~/.agents symlink, and selfcheck symlink."""
        script_path = SCRIPTS_DEVBOX / "05-install-devbox-agents-home.sh"
        self.assertTrue(script_path.is_file(), f"Missing {script_path}")
        self.assertTrue(bool(os.stat(script_path).st_mode & stat.S_IXUSR), "05 script is not executable")
        content = read_file(script_path)

        self.assertIn("AGENTS_HOME=", content)
        self.assertIn("/mnt/gptgrep-dev/agents-home", content)
        self.assertIn(".agents", content)
        self.assertIn("gptgrep-devbox-selfcheck", content)
        self.assertIn("/usr/local/bin/gptgrep-devbox-selfcheck", content)

        # Subdirectory layout
        for sub in ["bin", "config", "skills", "references", "clients", "runtimes"]:
            self.assertIn(f"${{AGENTS_HOME}}/{sub}", content)

        # Clean syntax check
        res = run_cmd(["bash", "-n", str(script_path)])
        self.assertEqual(res.returncode, 0, f"bash -n failed on 05 script: {res.stderr}")

    def test_10_devbox_ai_clients_setup_contract(self) -> None:
        """Verify 06-setup-devbox-ai-clients.sh symlink wiring, lease broker, and auth helper."""
        script_path = SCRIPTS_DEVBOX / "06-setup-devbox-ai-clients.sh"
        self.assertTrue(script_path.is_file(), f"Missing {script_path}")
        self.assertTrue(bool(os.stat(script_path).st_mode & stat.S_IXUSR), "06 script is not executable")
        content = read_file(script_path)

        # Symlinks for 3 AI clients
        self.assertIn(".claude", content)
        self.assertIn(".codex", content)
        self.assertIn(".gemini", content)
        self.assertIn("CLAUDE.md", content)
        self.assertIn("AGENTS.md", content)
        self.assertIn("GEMINI.md", content)

        # Lease broker and auth helper
        self.assertIn("codex-auth", content)
        self.assertIn("gptgrep-devbox-auth-helper", content)

        # Clean syntax check
        res = run_cmd(["bash", "-n", str(script_path)])
        self.assertEqual(res.returncode, 0, f"bash -n failed on 06 script: {res.stderr}")

    def test_11_devbox_wake_and_suspend_lifecycle(self) -> None:
        """Verify GCE instance wake (resume), suspend, and IAP tunneling contracts in scripts and skills."""
        script01 = SCRIPTS_DEVBOX / "01-provision-gce-devbox.sh"
        content01 = read_file(script01)

        self.assertIn('INSTANCE_NAME="${INSTANCE_NAME:-gptgrep-devbox-01}"', content01)
        self.assertIn('PROJECT_ID="${PROJECT_ID:-gws-personal-20261016}"', content01)
        self.assertIn('ZONE="${ZONE:-us-east4-a}"', content01)
        self.assertIn("allow-ssh-ingress-from-iap", content01)
        self.assertIn("--tunnel-through-iap", content01)

        # Verify resume / suspend workflows in SKILL.md
        skill_content = read_file(SKILLS_DEVBOX / "SKILL.md")
        self.assertIn("gcloud compute instances resume gptgrep-devbox-01", skill_content)
        self.assertIn("gcloud compute instances suspend gptgrep-devbox-01", skill_content)

    def test_12_five_pillar_selfcheck_receipt_schema(self) -> None:
        """Verify v2 current-host observation schema without claiming qualification."""
        res = run_cmd([str(BIN_SELFCHECK), "--mode", "local", "--json"])
        self.assertEqual(res.returncode, 1)
        data = json.loads(res.stdout)
        self.assertEqual(data["schema"], "gptgrep.devbox-selfcheck.receipt.v2")
        for key in ["receipt_id", "timestamp_utc", "mode", "binding", "binding_sha256"]:
            self.assertIn(key, data)
        self.assertEqual(data["overall_status"], "UNKNOWN")
        self.assertEqual(data["execution_scope"], "current_host")
        self.assertEqual(len(data["gates"]), 5)
        for gate in data["gates"]:
            self.assertEqual(gate["status"], "UNKNOWN")
        self.assertFalse(data["qualification"]["attempted"])
        self.assertEqual(data["workload_admission"]["status"], "not_granted")


if __name__ == "__main__":
    unittest.main()
