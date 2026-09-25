"""The parent agent: triage, decide (with one repair round), and digest."""

from __future__ import annotations

import logging
import time

from ..config.schema import Config
from ..core.errors import BackendError
from . import prompts
from .actions import Action, Reply, Rules, validate
from .contract import Contract, load
from .llm import StructuredLLM, make_llm
from .prompts import ParentContext
from .repos import load_repos
from .schemas import ACTION_SCHEMA, DEBRIEF_SCHEMA, DIGEST_CHARS, TRIAGE_SCHEMA

logger = logging.getLogger(__name__)
UNAVAILABLE = "I couldn't get to this just now; I'll need to look at it myself before anyone retries."
FAILURES = (BackendError, ValueError, KeyError, TypeError, OSError)


class ParentUnavailable(RuntimeError):
    pass


class ParentAgent:
    def __init__(self, config: Config, llm: StructuredLLM | None = None):
        self.config = config
        self.llm = llm or make_llm(config.parent.backend, model=config.parent.model,
                                   reasoning_effort=config.parent.reasoning_effort, timeout=config.parent.timeout,
                                   excluded_env=(config.slack.app_token_env, config.slack.user_token_env))

    def contract(self) -> Contract:
        return load(self.config.owner.contract)

    def repositories(self) -> tuple[dict, ...]:
        return tuple(repo.payload() for repo in load_repos(self.config.parent.repos))

    async def _call(self, name: str, prompt: str, schema: dict, *, model: str = "", ledger: list | None = None) -> dict:
        """One LLM call; its bookkeeping (for parent_turns rows) is appended to ``ledger``."""
        started = time.monotonic()
        record = {"call": name, "backend": self.llm.backend, "model": model or self.config.parent.model,
                  "prompt_chars": len(prompt), "error": ""}
        try:
            return await self.llm.call(prompt, schema, model=model)
        except FAILURES as error:
            record["error"] = f"{type(error).__name__}: {error}"[:500]
            raise
        finally:
            record["latency_ms"] = int((time.monotonic() - started) * 1000)
            if ledger is not None:
                ledger.append(record)

    async def triage(self, context: ParentContext, *, ledger: list | None = None) -> str:
        """respond, observe, or ignore for a message nobody addressed to the owner; failures observe."""
        try:
            result = await self._call("triage", prompts.triage(self.contract(), context), TRIAGE_SCHEMA,
                                      model=self.config.parent.triage_model, ledger=ledger)
        except FAILURES as error:
            logger.warning("triage unavailable: %s", error)
            return "observe"
        decision = result.get("decision")
        return decision if decision in ("respond", "observe", "ignore") else "observe"

    async def decide(self, context: ParentContext, rules: Rules, *, ledger: list | None = None) -> Action:
        """The parent's action for this trigger; delegation problems get one repair round, then are dropped."""
        contract = self.contract()
        try:
            raw = await self._call("decide", prompts.decide(contract, context), ACTION_SCHEMA, ledger=ledger)
            action, errors = validate(raw, rules)
            if errors:
                logger.info("parent action needs repair: %s", "; ".join(errors))
                repaired = await self._call("repair", prompts.decide(contract, context, errors=errors, previous=raw),
                                            ACTION_SCHEMA, ledger=ledger)
                action, errors = validate(repaired, rules)
                if errors:
                    logger.warning("dropping invalid parts of the parent action: %s", "; ".join(errors))
            return action
        except FAILURES as error:
            logger.warning("parent unavailable: %s", error)
            raise ParentUnavailable(str(error)) from error

    async def debrief(self, context: ParentContext, *, ledger: list | None = None) -> str:
        """The closing debrief of a finished discussion, governed by ``## Debriefs``."""
        result = await self._call("debrief", prompts.digest(self.contract().debriefs, context), DEBRIEF_SCHEMA,
                                  ledger=ledger)
        text = result.get("debrief")
        if not isinstance(text, str) or not text.strip():
            raise BackendError("the debrief came back empty")
        text = text.strip()
        return text if len(text) <= DIGEST_CHARS else text[:DIGEST_CHARS - 1].rstrip() + "…"

    def worker_instructions(self, *, machine: dict, workspace: str) -> str:
        return prompts.worker_instructions(self.contract(), owner=self.config.owner.slack_user,
                                           profile=self.config.owner.profile, repositories=self.repositories(),
                                           machine=machine, workspace=workspace)


def unavailable_reply() -> Reply:
    return Reply(send=True, text=UNAVAILABLE, status="blocked")
