"""The daemon: wires the store, Slack, the parent, the thread actors, the workers, and the control API.

``Daemon`` is transport-agnostic so tests drive it with fakes; ``serve`` connects it
to Slack Socket Mode with the owner's tokens.
"""

from __future__ import annotations

import asyncio
from dataclasses import replace
import logging
import os

from .approvals.broker import ApprovalBroker
from .config import editor
from .config.loader import fingerprint, load_config
from .config.schema import Config
from .control.api import serve as serve_control
from .core.bus import Bus
from .core.clock import Clock
from .core.errors import ConfigError
from .core.models import Message, WorkerRecord
from .parent.agent import ParentAgent
from .slack.egress import SlackAPI
from .slack.outbox import OutboxDispatcher
from .store import Store
from .threads.manager import ThreadManager
from .workers.supervisor import Supervisor, default_factory

logger = logging.getLogger(__name__)
HEARTBEAT = 5.0


class Daemon:
    def __init__(self, config: Config, slack: SlackAPI, *, store: Store | None = None, parent: ParentAgent | None = None,
                 factory=default_factory, clock: Clock | None = None, observe_only: bool = False, github=None):
        self.config = config
        self.slack = slack
        self.clock = clock or Clock()
        self.observe_only = observe_only
        self.github = github
        """A GitHubLinks for following pull request and issue links; None turns the feature off (tests, no network)."""
        self.store = store or Store(config.state.path)
        self.store.db.bind(config.owner.slack_user, config.slack.workspace)
        self.bus = Bus()
        self._own_parent = parent is None
        self.parent = parent or ParentAgent(config)
        self.broker = ApprovalBroker(config, self.store, clock=self.clock)
        self.supervisor = Supervisor(config, self.store, self.bus, instructions=self.worker_instructions,
                                     approvals=self.broker, factory=factory, clock=self.clock)
        self.dispatcher = OutboxDispatcher(self.store, slack, self.bus, owner=config.owner.slack_user, clock=self.clock)
        self.threads = ThreadManager(self)
        self.bus.on_thread(self.threads.notify)
        self.parent_slots = asyncio.Semaphore(config.limits.parent_concurrency)
        self.started_at = self.clock.now()
        self.slack_status = "starting"
        self._background: set[asyncio.Task] = set()
        self._recovered = False

    # ----- runtime services for actors -----

    def repositories(self) -> tuple[dict, ...]:
        try:
            return self.parent.repositories()
        except ValueError as error:
            logger.error("repository list unavailable: %s", error)
            return ()

    def worker_instructions(self, record: WorkerRecord) -> str:
        machine = self.config.machines[record.machine]
        return self.parent.worker_instructions(machine=machine.payload(), workspace=record.workspace)

    def spawn(self, coroutine) -> None:
        task = asyncio.get_running_loop().create_task(coroutine)
        self._background.add(task)
        task.add_done_callback(self._background.discard)

    # ----- intake -----

    def receive(self, message: Message) -> bool:
        """Store a Slack message (before it is acknowledged) and wake its thread; False if out of scope or a repeat."""
        if message.workspace != self.config.slack.workspace or message.channel not in self.config.slack.channels:
            return False
        session_id, inbox_id = self.store.messages.intake(message, self.clock.now())
        if inbox_id is None:
            return False
        self.bus.thread(session_id)
        return True

    # ----- control API -----

    async def thread_action(self, session_id: str, action: str, actor: str) -> dict:
        if self.store.threads.get(session_id) is None:
            raise ValueError("no such thread")
        self.store.inbox.add(session_id, "control", self.clock.now(), payload={"action": action, "actor": actor})
        self.bus.thread(session_id)
        return {"session": session_id, "action": action, "queued": True}

    async def instruct_thread(self, session_id: str, text: str, client_id: str) -> dict:
        session = self.store.threads.get(session_id)
        if session is None:
            raise ValueError("no such thread")
        if self.observe_only or session.key.channel not in self.config.slack.channels:
            raise ValueError("this thread cannot run an instruction")
        if session.control in ("closed", "archived", "cleaned"):
            raise ValueError(f"restore the {session.control} thread before giving an instruction")
        if not isinstance(text, str) or not 1 <= len(text.strip()) <= 4000:
            raise ValueError("instruction must be 1–4000 characters")
        if not isinstance(client_id, str) or not 8 <= len(client_id) <= 80 or not client_id.isascii() or not client_id.replace("-", "").isalnum():
            raise ValueError("client_id must be an ASCII identifier of 8–80 characters")
        instruction_id = self.store.inbox.add_once(session_id, "owner_instruction", client_id,
                                                    self.clock.now(), {"text": text.strip()})
        self.bus.thread(session_id)
        return {"instruction_id": instruction_id, "queued": True}

    def update_limits(self, changes: dict) -> Config:
        self.reload(editor.update(self.config.path, {"limits": changes}))
        return self.config

    def update_parent(self, changes: dict) -> Config:
        self.reload(editor.update(self.config.path, {"parent": changes}))
        return self.config

    def reload(self, config: Config) -> None:
        """Adopt a new configuration; machine changes apply to workers started afterwards."""
        if (config.owner.slack_user, config.slack.workspace) != (self.config.owner.slack_user, self.config.slack.workspace):
            logger.error("the Slack identity in the configuration changed; restart Fridica to switch identities")
            config = replace(config, owner=self.config.owner, slack=replace(config.slack, workspace=self.config.slack.workspace))
        if config.github.token_env != self.config.github.token_env:
            logger.error("github.token_env changed; restart Fridica to use another GitHub token")
            config = replace(config, github=replace(config.github, token_env=self.config.github.token_env))
        if self.github is not None:
            self.github.cache_seconds = config.github.cache_seconds
        elif config.github.enabled and not self.config.github.enabled:
            logger.warning("github.enabled turned on; restart Fridica to start following GitHub links")
        if self._own_parent and (config.parent != self.config.parent or config.secret_env() != self.config.secret_env()):
            self.parent = ParentAgent(config)
        if config.limits.parent_concurrency != self.config.limits.parent_concurrency:
            self.parent_slots = asyncio.Semaphore(config.limits.parent_concurrency)  # holders release the old one
        self.config = config
        self.parent.config = config
        self.broker.config = config
        self.supervisor.config = config
        logger.info("configuration reloaded (%s)", config.fingerprint[:12])

    async def watch_config(self) -> None:
        while True:
            await self.clock.sleep(HEARTBEAT)
            if self.config.path is None or fingerprint(self.config.path) in ("", self.config.fingerprint):
                continue
            try:
                self.reload(load_config(self.config.path))
            except ConfigError as error:
                logger.error("configuration change rejected; keeping the previous one: %s", error)
                self.config = replace(self.config, fingerprint=fingerprint(self.config.path))

    async def heartbeat(self) -> None:
        while True:
            self.store.heartbeat(started_at=self.started_at, now=self.clock.now(), slack_status=self.slack_status,
                                 observe_only=self.observe_only, control_socket=str(self.config.state.control_socket),
                                 fingerprint=self.config.fingerprint)
            await self.clock.sleep(HEARTBEAT)

    # ----- lifecycle -----

    def recover(self) -> None:
        """Settle the previous run's in-flight work; must happen before any new message is processed."""
        if self._recovered:
            return
        self._recovered = True
        counts = self.store.recover(self.clock.now())
        if any(counts.values()):
            logger.info("recovered after restart: %s", ", ".join(f"{key}={value}" for key, value in counts.items() if value))
        for session in self.store.threads.with_status("waiting"):
            if session.wait_streak >= self.config.limits.max_wait_replies:
                self.store.threads.save(replace(session, control="paused", pause_reason=(
                    f"{self.config.limits.max_wait_replies} consecutive replies needed more information.")), self.clock.now())

    async def run(self, *, control: bool = True) -> None:
        """Run until cancelled: recovery, then the scheduler, outbox, thread actors, heartbeat, and control API."""
        self.recover()
        runner = await serve_control(self, self.config.state.control_socket) if control else None
        try:
            async with asyncio.TaskGroup() as group:
                if not self.observe_only:  # observe-only never posts and never runs work, not even leftovers
                    group.create_task(self.supervisor.run())
                    group.create_task(self.dispatcher.run())
                group.create_task(self.threads.run())
                group.create_task(self.heartbeat())
                group.create_task(self.watch_config())
        finally:
            await self.close()
            if runner is not None:
                await runner.cleanup()
                self.config.state.control_socket.unlink(missing_ok=True)

    async def close(self) -> None:
        await self.threads.close()
        for task in list(self._background):
            task.cancel()
        await asyncio.gather(*self._background, return_exceptions=True)
        await self.supervisor.close()
        self.slack_status = "stopped"
        self.store.heartbeat(started_at=self.started_at, now=self.clock.now(), slack_status="stopped",
                             observe_only=self.observe_only, fingerprint=self.config.fingerprint)


async def serve(config: Config, *, observe_only: bool = False) -> None:
    """Connect to Slack Socket Mode as the owner and run the daemon until cancelled."""
    import aiohttp
    from slack_sdk.socket_mode.aiohttp import SocketModeClient
    from slack_sdk.socket_mode.response import SocketModeResponse
    from slack_sdk.web.async_client import AsyncWebClient

    from .slack import catchup
    from .slack.egress import SlackClient
    from .slack.ingress import dropped_mention, normalize

    app_token, user_token = config.tokens()
    async with aiohttp.ClientSession() as session:
        web = AsyncWebClient(token=user_token, session=session, retry_handlers=[])
        slack = SlackClient(config, web)
        await slack.validate()
        github = None
        if config.github.enabled:
            from .github.client import GitHubClient
            from .github.links import GitHubLinks
            github = GitHubLinks(GitHubClient(session, os.environ.get(config.github.token_env, "")),
                                 cache_seconds=config.github.cache_seconds)
        daemon = Daemon(config, slack, observe_only=observe_only, github=github)
        socket = SocketModeClient(app_token=app_token, web_client=web)

        async def receive(client, request):
            if request.type == "events_api":
                message = normalize(request.payload)
                if message is not None:
                    daemon.receive(message)
                else:
                    reason = dropped_mention(request.payload, daemon.config.owner.slack_user)
                    if reason:
                        logger.warning("ignored a message that mentions you: %s", reason)
            await client.send_socket_mode_response(SocketModeResponse(envelope_id=request.envelope_id))

        async def status():
            while True:
                daemon.slack_status = "connected" if await socket.is_connected() else "reconnecting"
                await asyncio.sleep(HEARTBEAT)

        socket.socket_mode_request_listeners.append(receive)
        daemon.recover()
        try:
            await socket.connect()
            logger.info("listening as %s in %d channels", config.owner.slack_user, len(config.slack.channels))
            async with asyncio.TaskGroup() as group:
                group.create_task(daemon.run())
                group.create_task(status())
                group.create_task(catchup.run(slack, daemon.store, lambda: daemon.config, daemon.bus, daemon.clock))
        finally:
            await socket.close()
            daemon.store.close()
