"""Focused offline checks: pinned process, targeting, identity and no replay."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch
import uuid
from types import SimpleNamespace
import adapter as a

ROOT = Path(__file__).resolve().parent
PROJECT = "333333333333333333333333"
CHANNEL = "CEXAMPLE"
WORKER = "worker-11111111-1111-4111-8111-111111111111"

class AdapterChecks(unittest.TestCase):
    def test_actual_pinned_upstream_discovery_and_memory_session(self):
        source = f"git+https://github.com/happysky19/overleaf-mcp-native.git#{a.SOURCE}"
        self.assertTrue(a.pinned_source(source))
        self.assertTrue(a.pinned_source(source.replace("git+https://", "git+ssh://git@")))
        self.assertFalse(a.pinned_source(source.replace("happysky19/", "other/")))
        self.assertFalse(a.pinned_source(source.replace(a.SOURCE, a.SOURCE[:12])))
        class InspectSession(a.Peer):
            def __init__(self, argv, env, cwd, fds):
                # Consume the actual inherited socket before the real MCP handshake;
                # tools/list alone would not exercise upstream auth/discovery.
                bootstrap = """
                    import assert from 'node:assert/strict';
                    import { fstatSync } from 'node:fs';
                    import { pathToFileURL } from 'node:url';
                    const entry = pathToFileURL(process.argv[1]);
                    const { loadStored } = await import(new URL('./auth/cookieStore.js', entry));
                    const { discoverCookie } = await import(new URL('./auth/discover.js', entry));
                    assert.equal((await loadStored('https://www.overleaf.com')).cookie, 'overleaf_session2=fixture');
                    assert.throws(() => fstatSync(Number(process.env.OL_SESSION_FD)), { code: 'EBADF' });
                    assert.equal(await discoverCookie('https://www.overleaf.com'), 'overleaf_session2=fixture');
                    await import(entry);
                """
                super().__init__([argv[0], '--input-type=module', '-e', bootstrap, argv[1]], env, cwd, fds)
        with tempfile.TemporaryDirectory(dir=os.environ["TASK_TMP"]) as directory:
            state = Path(directory)
            with patch.object(a, 'Peer', InspectSession):
                peer = a.native_peer(os.environ["TASK_NODE"], ROOT, state, "overleaf_session2=fixture")
            try:
                self.assertEqual(peer.info["serverInfo"]["version"], a.PIN)
                names = {t["name"] for t in peer.call("tools/list")["tools"]}
                self.assertTrue(set(a.OPERATIONS) <= names)
                self.assertFalse((state / "overleaf-mcp/cookie.json").exists())
                for f in state.rglob("*"):
                    if f.is_file() and not f.is_symlink():
                        self.assertNotIn(b"fixture", f.read_bytes())
            finally:
                peer.close()

    def test_scope_and_pending_only(self):
        grant = dict(schema=2, cookie="overleaf_session2=fixture", account_id="1" * 24,
                     expected_email="owner@example.invalid", projects=[PROJECT], operations=a.OPERATIONS,
                     channel=CHANNEL, recipient=dict(worker_id=WORKER, machine="dungeon3", workspace="snapy"))
        self.assertEqual(a.session(json.dumps(grant), WORKER, CHANNEL), grant)
        for key, value in (("channel", "OTHER"), ("projects", ["invalid"]), ("expires_at", 1)):
            with self.assertRaises(ValueError):
                a.session(json.dumps({**grant, key: value}), WORKER, CHANNEL)
        with self.assertRaises(ValueError):
            a.session(json.dumps(grant), "worker-22222222-2222-4222-8222-222222222222", CHANNEL)
        schema = {"properties": {k: {} for k in ["project_id", "path", "old_string", "new_string", "expected_version", "track", "strict_version", "thread_id", "content"]}}
        valid = dict(project_id=PROJECT, expected_version=3, operation_id=str(uuid.uuid4()))
        for name, args in (("native_accept_changes", valid),
                           ("native_reply_comment", {**valid, "project_id": "2" * 24}),
                           ("native_suggest_edit", {**valid, "track": "off"}),
                           ("native_suggest_edit", {**valid, "strict_version": False}),
                           ("native_suggest_edit", {**valid, "expected_version": None}),
                           ("native_add_comment", {"project_id": PROJECT})):
            with self.assertRaises(ValueError):
                a.arguments(name, args, schema, [PROJECT])
        op, args, _ = a.arguments("native_suggest_edit", dict(project_id=PROJECT, old_string="a", new_string="b", expected_version=3,
                                 operation_id=str(uuid.uuid4())), schema, [PROJECT])
        self.assertEqual(op, "find_and_replace")
        self.assertTrue(args["strict_version"])
        self.assertEqual(args["track"], "on")

    def test_expected_account_and_project(self):
        grant = {"projects": [PROJECT], "account_id": "1" * 24, "expected_email": "owner@example.invalid"}
        body = '<meta name="ol-project_id" content="' + PROJECT + '\"><meta name="ol-user" content=\'' + json.dumps({"_id": "1" * 24, "email": "owner@example.invalid"}) + "'>"
        a.identity(body, grant, PROJECT)
        for changed in (body.replace("owner@", "other@"), body.replace(PROJECT, "2" * 24)):
            with self.assertRaises(ValueError):
                a.identity(changed, grant, PROJECT)

    def test_explicit_two_project_dispatch_and_legacy_restrictions(self):
        grant = dict(schema=3, cookie="overleaf_session2=fixture", account_id="1" * 24,
                     expected_email="owner@example.invalid", operations=a.OPERATIONS,
                     channel=CHANNEL, recipient=dict(worker_id=WORKER, machine="dungeon3", workspace="snapy"))
        adapter = a.Adapter.__new__(a.Adapter)
        grant["projects"] = [PROJECT]
        adapter.raw = json.dumps(grant)
        adapter.grant = a.session(adapter.raw, WORKER, CHANNEL)
        adapter.options = SimpleNamespace(worker=WORKER, channel=CHANNEL)
        schema = {"properties": {"project_id": {"type": "string"}}}
        adapter.schemas = {op: {"name": op, "inputSchema": schema} for op in a.OPERATIONS}
        adapter.builtin = Mock()
        adapter.builtin.call.side_effect = lambda *_: {"tools": []}
        adapter.native = Mock()
        adapter.native.call.return_value = {"content": []}
        tools = adapter.dispatch("tools/list", {})["tools"]
        for tool in tools:
            self.assertIn("project_id", tool["inputSchema"]["required"])
            self.assertNotIn("default", tool["inputSchema"]["properties"]["project_id"])
        with patch.object(a, "verify_identity") as verify:
            for project in (PROJECT, "2" * 24, PROJECT):
                # Each admitted job receives a freshly narrowed session.
                grant["projects"] = [project]
                adapter.raw = json.dumps(grant)
                adapter.grant = a.session(adapter.raw, WORKER, CHANNEL)
                with self.assertRaises(ValueError):
                    adapter.dispatch("tools/call", {"name": "native_list_comments", "arguments": {"project_id": "4" * 24}})
                adapter.dispatch("tools/call", {"name": "native_list_comments", "arguments": {"project_id": project}})
                self.assertEqual(adapter.native.call.call_args.args[1]["arguments"]["project_id"], project)
                self.assertEqual(verify.call_args.args[1], project)
            with self.assertRaisesRegex(ValueError, "explicit project_id"):
                adapter.dispatch("tools/call", {"name": "native_list_comments", "arguments": {}})
            self.assertEqual(adapter.native.call.call_count, 3)
            grant.update(schema=2, projects=[PROJECT])
            adapter.raw = json.dumps(grant)
            adapter.grant = a.session(adapter.raw, WORKER, CHANNEL)
            with self.assertRaises(ValueError):
                adapter.dispatch("tools/call", {"name": "native_list_comments", "arguments": {"project_id": "2" * 24}})
            self.assertEqual(adapter.native.call.call_count, 3)

    def test_receipt_prevents_replay_after_success_and_unknown_outcome(self):
        with tempfile.TemporaryDirectory(dir=os.environ["TASK_TMP"]) as directory:
            root = Path(directory)
            call = Mock(return_value={"structuredContent": {"thread_id": "1" * 24, "content_length": 4}})
            opid = str(uuid.uuid4())
            for _ in range(2):
                a.write_once(root, "reply_comment", {"content": "private text"}, opid, call)
            self.assertEqual(call.call_count, 1)
            with self.assertRaisesRegex(ValueError, "another payload"):
                a.write_once(root, "reply_comment", {"content": "private text", "project_id": "2" * 24}, opid, call)
            self.assertEqual(call.call_count, 1)
            opid = str(uuid.uuid4())
            call = Mock(side_effect=TimeoutError)
            with self.assertRaises(TimeoutError):
                a.write_once(root, "reply_comment", {}, opid, call)
            with self.assertRaisesRegex(ValueError, "uncertain"):
                a.write_once(root, "reply_comment", {}, opid, call)
            self.assertEqual(call.call_count, 1)
            self.assertNotIn("private text", "".join(f.read_text() for f in root.iterdir()))

if __name__ == "__main__":
    unittest.main()
