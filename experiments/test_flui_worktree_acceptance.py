"""Lightweight public-harness safety regressions; real Git, no Cargo builds.

Owned temporary repositories and receipts are retained for inspection.
"""
import argparse
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
SOURCE = Path(__file__).with_name("flui-worktree-acceptance.py")
SPEC = importlib.util.spec_from_file_location("flui_acceptance_under_test", SOURCE)
HARNESS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HARNESS)


class CleanupSafety(unittest.TestCase):
    def setUp(self):
        parent = os.environ.get("HELI_ACCEPTANCE_TEST_ROOT")
        if parent:
            Path(parent).mkdir(parents=True, exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix="flui-cleanup-", dir=parent)).resolve()
        self.repo = self.root / "repo"
        self.wt = self.root / "checkout"
        self.hooks = self.root / "empty-hooks"
        self.hooks.mkdir()
        self.env = os.environ | {"GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
        for key in tuple(self.env):
            if key.startswith("GIT_") and key not in ("GIT_CONFIG_GLOBAL", "GIT_CONFIG_NOSYSTEM"):
                self.env.pop(key)
        self.git(self.root, "init", str(self.repo))
        (self.repo / "source.txt").write_text("pinned\n", encoding="utf-8")
        self.git(self.repo, "add", "source.txt")
        self.git(self.repo, "commit", "-m", "pinned source")
        self.pinned = self.git(self.repo, "rev-parse", "HEAD").strip()
        self.git(self.repo, "worktree", "add", "--detach", str(self.wt), self.pinned)

    def git(self, cwd, *args):
        completed = subprocess.run(
            ["git", "-c", "core.hooksPath=" + str(self.hooks),
             "-c", "user.name=Heli acceptance", "-c", "user.email=heli@example.invalid",
             "-c", "commit.gpgsign=false", "-c", "core.autocrlf=false", *args],
            cwd=cwd, env=self.env, capture_output=True, text=True, timeout=30,
            **HARNESS.HELPER.PROCESS_OPTIONS)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        return completed.stdout

    def cleanup(self, scenario):
        run = HARNESS.Run.__new__(HARNESS.Run)
        run.clients, run.owned, run.logs = [], [], []
        run.worktrees = {"a": self.wt}
        run.args = argparse.Namespace(flui=str(self.repo), flui_ref=self.pinned)
        run.root, run.env = self.root, self.env
        run.endpoint_directory = None
        run.result = {}
        before = self.git(self.wt, "rev-parse", "HEAD").strip()
        run.cleanup()
        receipt = {"scenario": scenario, "pinned": self.pinned, "before_head": before,
                   "worktree_exists_after": self.wt.exists(), "cleanup_result": run.result}
        (self.root / "receipt.json").write_text(json.dumps(receipt, indent=2), encoding="utf-8")
        return run.result

    def test_clean_pinned_checkout_is_removed(self):
        result = self.cleanup("clean-pinned")
        self.assertFalse(self.wt.exists())
        self.assertFalse(result.get("worktree_cleanup_errors"))

    def test_dirty_checkout_is_retained(self):
        (self.wt / "source.txt").write_text("unpublished edit\n", encoding="utf-8")
        result = self.cleanup("dirty")
        self.assertTrue(self.wt.exists())
        self.assertIn("a", result["retained_dirty_worktrees"])

    def test_clean_unpublished_head_is_retained(self):
        (self.wt / "source.txt").write_text("sole unpublished commit\n", encoding="utf-8")
        self.git(self.wt, "add", "source.txt")
        self.git(self.wt, "commit", "-m", "unpublished detached commit")
        unpublished = self.git(self.wt, "rev-parse", "HEAD").strip()
        self.assertNotEqual(unpublished, self.pinned)
        result = self.cleanup("clean-wrong-head")
        self.assertTrue(self.wt.exists(), "cleanup removed the sole checkout of an unpublished HEAD")
        self.assertEqual(self.git(self.wt, "rev-parse", "HEAD").strip(), unpublished)
        self.assertEqual(result["retained_worktrees"]["a"]["reason"], "head_differs_from_pinned_commit")


class InvocationSafety(unittest.TestCase):
    def invoke(self, *extra):
        root = Path(tempfile.mkdtemp(prefix="flui-invocation-")) / "lab"
        self.assertFalse(root.exists())
        args = [str(SOURCE), "--heli", "nonexistent-heli", "--flui", "nonexistent-flui",
                "--flui-ref", "0" * 40, "--budget-mb", "8192", "--output", str(root), *extra]
        with patch.object(sys, "argv", args), patch.object(subprocess, "Popen", side_effect=AssertionError("process launched")), patch.object(subprocess, "run", side_effect=AssertionError("process launched")), contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            try:
                code = HARNESS.main()
            except SystemExit as error:
                code = error.code
        self.assertFalse(root.exists())
        return code

    def test_prepare_only_needs_no_real_tools_or_output(self):
        self.assertEqual(self.invoke(), 0)

    def test_run_requires_compiler_instrumentation_before_any_side_effect(self):
        self.assertEqual(self.invoke("--run"), 2)


if __name__ == "__main__":
    unittest.main()
