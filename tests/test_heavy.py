"""Heavy-task escalation: the replica hands a brief to the persistent worker and posts its report."""
import asyncio
from dataclasses import replace
import json

import pytest

from fridica.config import Resources, load_config
from fridica.models import AgentResult, ConversationContext, Decision, Message
from fridica.prompts import HEAVY_NOTE, RESPONSE_SCHEMA, conversation_prompt, worker_prompt
from fridica.replica import HEAVY_FAILED_NOTICE, HEAVY_INTERRUPTED_NOTICE, Replica
from fridica.store import Store
from test_replica import Transport


class HeavyAgent:
    """A fake backend whose replies escalate and whose worker reports after a controllable delay."""

    def __init__(self, results, reports=None, error=None, delay=0.0):
        self.results = list(results)
        self.reports = list(reports or ["Full suite: 312 passed, 0 failed."])
        self.error = error
        self.delay = delay
        self.responded = []
        self.worked = []
        self.closed = 0

    async def classify(self, message, context):
        return Decision.RESPOND

    async def respond(self, message, context):
        self.responded.append((message, context))
        return self.results.pop(0)

    async def summarize(self, context):
        return "summary"

    async def debrief(self, context):
        return "debrief"

    async def work(self, brief, context, resume, host=""):
        self.worked.append((brief, context, resume, host))
        await asyncio.sleep(self.delay)
        if self.error:
            raise self.error
        return self.reports.pop(0), f"thr-{len(self.worked)}"

    async def close(self):
        self.closed += 1


@pytest.fixture
def heavy_config(config):
    return replace(config, heavy_tasks=True, heavy_task_timeout=5, resources=Resources(cpus=8, gpus=(0,)))


async def settle(replica):
    """Let background heavy jobs finish."""
    for _ in range(200):
        if not replica._background:
            return
        await asyncio.sleep(0.01)
    raise AssertionError("background work did not finish")


def run(replica, message):
    async def go():
        replica.receive(message)
        await replica.process(message)
        await settle(replica)
    asyncio.run(go())


def test_escalation_posts_report_in_thread(heavy_config, store, message):
    agent = HeavyAgent([AgentResult("Starting the full run; I'll post the result here.", escalate="Run the full test suite on the GPU box.")])
    transport = Transport()
    replica = Replica(heavy_config, store, agent, transport)
    first = message()
    run(replica, first)
    assert [sent[1].text for sent in transport.sent] == ["Starting the full run; I'll post the result here.", "Full suite: 312 passed, 0 failed."]
    brief, context, resume, host = agent.worked[0]
    assert host == "local"
    assert brief == "Run the full test suite on the GPU box." and resume is None
    assert context.worker == {"state": "running", "since": pytest.approx(store.task(first)["worker_since"]), "host": "local"}
    assert context.task_id == store.task(first)["task_id"]
    task = store.task(first)
    assert task["worker_state"] == "done" and task["worker_thread"] == "thr-1"
    # The report is recorded as our own generated message so it is thread context and never re-ingested.
    recorded = [Message(**json.loads(row["payload"])) for row in store.connection.execute("SELECT payload FROM events ORDER BY timestamp")]
    assert recorded[-1].generated and recorded[-1].text == "Full suite: 312 passed, 0 failed."
    assert transport.sent[1][2] == task["task_id"] and transport.sent[1][3] == task["turns"]


def test_running_worker_blocks_second_escalation_and_is_visible(heavy_config, store, message):
    agent = HeavyAgent([AgentResult("Started.", escalate="job one"), AgentResult("Still running.", escalate="job two"),
                        AgentResult("Next.", escalate="job three")], reports=["one done", "three done"], delay=0.2)
    transport = Transport()
    replica = Replica(heavy_config, store, agent, transport)
    first = message()

    async def go():
        replica.receive(first)
        await replica.process(first)
        await asyncio.sleep(0)  # let the background job start, as the daemon's idle sleep would
        second = message("event2", text="<@UOWNER> how is it going?", timestamp="100.000003")
        replica.receive(second)
        await replica.process(second)
        assert agent.responded[1][1].worker["state"] == "running"
        assert len(agent.worked) == 1  # job two ignored while job one runs
        await settle(replica)
        assert store.task(first)["worker_state"] == "done"
        third = message("event3", text="<@UOWNER> now the next one", timestamp="100.000005")
        replica.receive(third)
        await replica.process(third)
        assert agent.responded[2][1].worker["state"] == "done"
        await settle(replica)
    asyncio.run(go())
    assert [w[0] for w in agent.worked] == ["job one", "job three"]
    assert agent.worked[1][2] == "thr-1"  # the second job resumes the first job's worker thread
    # The reply to the second message goes out while job one is still running; its report follows.
    assert [sent[1].text for sent in transport.sent] == ["Started.", "Still running.", "one done", "Next.", "three done"]


def test_failed_job_posts_notice(heavy_config, store, message, caplog):
    import logging
    agent = HeavyAgent([AgentResult("Started.", escalate="job")], error=RuntimeError("PRIVATE cuda error"))
    transport = Transport()
    replica = Replica(heavy_config, store, agent, transport)
    with caplog.at_level(logging.ERROR, logger="fridica.replica"):
        run(replica, message())
    assert transport.sent[-1][1].text == HEAVY_FAILED_NOTICE
    assert store.task(message())["worker_state"] == "failed"
    assert "PRIVATE cuda error" in caplog.text and "PRIVATE" not in transport.sent[-1][1].text


def test_heavy_tasks_disabled_never_works(config, store, message):
    agent = HeavyAgent([AgentResult("Started.", escalate="job")])
    transport = Transport()
    replica = Replica(config, store, agent, transport)
    run(replica, message())
    assert not agent.worked and len(transport.sent) == 1
    assert store.task(message())["worker_state"] is None


def test_shutdown_cancels_jobs_and_closes_workers(heavy_config, store, message):
    agent = HeavyAgent([AgentResult("Started.", escalate="job")], delay=10)
    transport = Transport()
    replica = Replica(heavy_config, store, agent, transport)
    first = message()

    async def go():
        replica.receive(first)
        await replica.process(first)
        assert replica._background
        runner = asyncio.create_task(replica.run())
        await asyncio.sleep(0.05)
        runner.cancel()
        with pytest.raises(asyncio.CancelledError):
            await runner
    asyncio.run(go())
    assert agent.closed == 1 and not replica._background
    assert store.task(first)["worker_state"] == "interrupted"


def test_restart_reports_interrupted_jobs(heavy_config, message):
    database = Store(heavy_config.state_path)
    first = message()
    database.add(first)
    database.begin(first, "task-1", 1)
    database.save_worker(first, "running", "thr-9")
    database.close()
    database = Store(heavy_config.state_path)
    try:
        assert database.task(first)["worker_state"] == "interrupted"
        transport = Transport()
        replica = Replica(heavy_config, database, HeavyAgent([]), transport)

        async def go():
            runner = asyncio.create_task(replica.run())
            await asyncio.sleep(0.1)
            runner.cancel()
            with pytest.raises(asyncio.CancelledError):
                await runner
        asyncio.run(go())
        assert transport.sent[0][1].text == HEAVY_INTERRUPTED_NOTICE and transport.sent[0][2] == "task-1"
        assert database.task(first)["worker_state"] == "failed" and database.task(first)["worker_thread"] == "thr-9"
    finally:
        database.close()


def test_reload_closes_previous_backend(heavy_config, store, message, tmp_path, monkeypatch):
    from fridica import agents as agents_module
    path = tmp_path / "config.toml"
    path.write_text(f'owner_id="UOWNER"\nworkspace_id="TTEAM"\nchannels=["CROOM"]\nworkspace="{heavy_config.workspace}"\n'
                    f'state_path="{heavy_config.state_path}"\nheavy_tasks=true\n')
    agent = HeavyAgent([])
    replica = Replica(load_config(path), store, agent, Transport(), config_path=path)
    with path.open("a") as stream:
        stream.write('max_turns = 9\n')
    created = []
    monkeypatch.setattr(agents_module, "create_backend", lambda config: created.append(config) or HeavyAgent([]))

    async def go():
        replica.reload_config()
        await settle(replica)
    asyncio.run(go())
    assert created and created[0].max_turns == 9 and agent.closed == 1


def test_response_schema_and_prompts(config, message):
    assert "escalate" in RESPONSE_SCHEMA["required"] and RESPONSE_SCHEMA["properties"]["escalate"]["type"] == "string"
    context = ConversationContext([message()], "UOWNER", "profile", "task", 1, worker={"state": "running", "since": 1.0})
    plain = conversation_prompt(message(), context, False)
    heavy = conversation_prompt(message(), context, False, heavy=True, resources={"cpus": 8, "gpus": [0]})
    assert HEAVY_NOTE.strip() not in plain and HEAVY_NOTE.strip() in heavy
    data = json.loads(heavy.split("Conversation data:\n", 1)[1])
    assert data["worker"] == {"state": "running", "since": 1.0} and data["resources"] == {"cpus": 8, "gpus": [0]}
    assert "worker" not in conversation_prompt(message(), context, True)
    job = worker_prompt("Run the GPU tests", context, resources={"gpus": [0]})
    payload = json.loads(job.split("Job data:\n", 1)[1])
    assert payload["brief"] == "Run the GPU tests" and payload["resources"] == {"gpus": [0]}
    assert "final message is posted to the Slack thread" in job and "Slack replies" in job
    # GPU notes appear only when the worker really has GPU access.
    assert "GPU devices listed in resources are available" not in job
    assert "GPU devices listed in resources are available" in worker_prompt("x", context, resources={"gpus": [0], "gpu_access": True})
    assert "can only run in a heavy task" not in heavy
    assert "can only run in a heavy task" in conversation_prompt(message(), context, False, heavy=True, resources={"gpus": [0], "gpu_access": True})


def test_backend_result_carries_escalation(config, tmp_path, monkeypatch, message):
    from fridica import agents
    from fridica.agents import ClaudeBackend

    async def run_cli(command, prompt, cwd, settings):
        assert ("Heavy tasks are enabled" in prompt) == settings.heavy_tasks
        return json.dumps({"structured_output": {"text": "Kicking it off.", "status": "complete", "discussion": "ongoing",
                                                 "send": True, "escalate": "  build everything  "}})

    monkeypatch.setattr(agents, "_run", run_cli)
    context = ConversationContext([], config.owner_id, "profile", "task", 1)
    heavy = asyncio.run(ClaudeBackend(replace(config, heavy_tasks=True, resume_sessions=False)).respond(message(), context))
    assert heavy.escalate == "build everything" and heavy.text == "Kicking it off."
    plain = asyncio.run(ClaudeBackend(replace(config, resume_sessions=False)).respond(message(), context))
    assert plain.escalate == ""


def test_backend_work_uses_thread_worker(config, message, monkeypatch):
    from fridica.agents import ClaudeBackend
    backend = ClaudeBackend(replace(config, heavy_tasks=True, resources=Resources(cpus=2)))
    seen = {}

    class FakeWorker:
        async def run(self, prompt, resume):
            seen["prompt"], seen["resume"] = prompt, resume
            return "x" * 5000, "thr-1"

    backend.workers.workers[("task", "local")] = FakeWorker()
    context = ConversationContext([], config.owner_id, "profile", "task", 1)
    report, thread = asyncio.run(backend.work("do it", context, "thr-0"))
    assert thread == "thr-1" and len(report) == 3500 and seen["resume"] == "thr-0"
    assert '"brief": "do it"' in seen["prompt"] and '"cpus": 2' in seen["prompt"]


def test_resources_and_heavy_config(tmp_path, config):
    source = tmp_path / "config.toml"
    base = f'owner_id="UOWNER"\nworkspace_id="TTEAM"\nchannels=["CROOM"]\nworkspace="{config.workspace}"\nstate_path="{config.state_path}"\n'
    source.write_text(base + 'heavy_tasks = true\nheavy_task_timeout = 60\nheavy_task_idle = 0\n[resources]\ncpus = 8\ngpus = [1, 0]\ngpu_type = "A100"\nmemory_gb = 64\nnotes = "use srun"\n')
    loaded = load_config(source)
    assert loaded.heavy_tasks and loaded.heavy_task_timeout == 60 and loaded.heavy_task_idle == 0
    assert loaded.resources == Resources(8, (1, 0), "A100", 64, "use srun")
    assert loaded.resources.payload() == {"cpus": 8, "gpus": [1, 0], "gpu_type": "A100", "memory_gb": 64, "notes": "use srun", "gpu_access": True}
    assert loaded.resources.environment() == {"OMP_NUM_THREADS": "8", "CUDA_VISIBLE_DEVICES": "1,0"}
    assert Resources().payload() == {} and Resources().environment() == {}
    assert Resources(gpus=()).environment() == {"CUDA_VISIBLE_DEVICES": ""}
    assert loaded.resources.gpu_worker and loaded.resources.payload()["gpu_access"] is True
    assert not Resources(gpus=()).gpu_worker and "gpu_access" not in Resources(gpus=()).payload()
    assert not Resources(cpus=2).gpu_worker and "gpu_access" not in Resources(cpus=2).payload()
    kept = Resources(gpus=(0,), gpu_access=False)
    assert not kept.gpu_worker and kept.payload()["gpu_access"] is False
    with pytest.raises(ValueError):
        Resources(gpu_access="yes")
    assert load_config(source).resources == loaded.resources  # frozen and comparable for hot reload
    for body in ('[resources]\ncpus = 0\n', '[resources]\ngpus = [0, 0]\n', '[resources]\ngpus = "0"\n', '[resources]\nmemory_gb = -1\n',
                 '[resources]\ntpus = 1\n', 'resources = 3\n', 'heavy_tasks = 1\n', 'heavy_task_timeout = 0\n', 'heavy_task_idle = -1\n',
                 'heavy_tasks = true\nfile_access = true\n'):
        source.write_text(base + body)
        with pytest.raises(ValueError):
            load_config(source)


def test_heavy_capability_check(config, monkeypatch):
    from fridica import agents
    import subprocess
    monkeypatch.setattr(agents.shutil, "which", lambda name: f"/bin/{name}")
    monkeypatch.setenv(config.user_token_env, "xoxp-secret")
    outputs = {"exec --help": "--ignore-user-config --ignore-rules --output-schema --ephemeral resume",
               "app-server --help": "--listen <URL>",
               "--help": "--setting-sources --strict-mcp-config --json-schema dontAsk acceptEdits --session-id --resume"}

    def run(command, **kwargs):
        text = outputs[" ".join(command[1:])]
        return subprocess.CompletedProcess(command, 0, text, "")

    monkeypatch.setattr(agents.subprocess, "run", run)
    assert agents.check_backend(replace(config, backend="codex", heavy_tasks=True)) == []
    (problem,) = agents.check_backend(replace(config, backend="claude", heavy_tasks=True))
    assert "--input-format" in problem
    outputs["--help"] += " --input-format"
    assert agents.check_backend(replace(config, backend="claude", heavy_tasks=True)) == []
    outputs["app-server --help"] = "error"
    (problem,) = agents.check_backend(replace(config, backend="codex", heavy_tasks=True))
    assert "app-server" in problem
    assert agents.check_backend(replace(config, backend="codex")) == []


def test_escalation_routes_to_named_host(config, store, message, tmp_path):
    from pathlib import PurePosixPath
    from fridica.config import Host, Resources
    dart9 = Host("dart9", (PurePosixPath("/mnt/data1/projects"),), Resources(gpus=(0,)))
    heavy = replace(config, heavy_tasks=True, heavy_task_timeout=5, remote_hosts=(dart9,))
    agent = HeavyAgent([AgentResult("Training on the GPU box.", escalate="train", escalate_host="dart9"),
                        AgentResult("Local build next.", escalate="build", escalate_host="")],
                       reports=["trained", "built"])
    transport = Transport()
    replica = Replica(heavy, store, agent, transport)
    first = message()
    run(replica, first)
    assert agent.worked[0][3] == "dart9" and agent.worked[0][2] is None
    task = store.task(first)
    assert task["worker_host"] == "dart9" and task["worker_thread"] == "thr-1" and task["worker_state"] == "done"
    assert agent.responded[0][1].worker is None
    # A job on another host must not resume dart9's worker thread.
    run(replica, message("event2", text="<@UOWNER> build it", timestamp="100.000003"))
    assert agent.worked[1][3] == "local" and agent.worked[1][2] is None
    assert store.task(first)["worker_host"] == "local" and store.task(first)["worker_thread"] == "thr-2"
    assert [sent[1].text for sent in transport.sent] == ["Training on the GPU box.", "trained", "Local build next.", "built"]


def test_backend_validates_escalation_host(config, monkeypatch, message, caplog):
    from pathlib import PurePosixPath
    from fridica import agents
    from fridica.agents import ClaudeBackend
    from fridica.config import Host
    heavy = replace(config, heavy_tasks=True, resume_sessions=False, remote_hosts=(Host("dart9", (PurePosixPath("/mnt/a"),)),))
    answers = iter([("dart9", "dart9"), ("local", ""), ("", ""), ("snowy", None)])

    async def run_cli(command, prompt, cwd, settings):
        data = json.loads(prompt.split("Conversation data:\n", 1)[1])
        assert [host["name"] for host in data["hosts"]] == ["local", "dart9"]
        return json.dumps({"structured_output": {"text": "Started.", "status": "complete", "discussion": "ongoing",
                                                 "send": True, "escalate": "job", "escalate_host": current[0]}})

    monkeypatch.setattr(agents, "_run", run_cli)
    context = ConversationContext([], config.owner_id, "profile", "task", 1)
    import logging
    for current in answers:
        with caplog.at_level(logging.WARNING, logger="fridica.agents"):
            result = asyncio.run(ClaudeBackend(heavy).respond(message(), context))
        if current[1] is None:
            assert result.escalate == "job" and result.escalate_host == "" and "unknown host" in caplog.text
        else:
            assert result.escalate == "job" and result.escalate_host == current[1]
