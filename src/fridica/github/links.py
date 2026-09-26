"""The state of GitHub pull requests and issues linked in a thread, as compact untrusted data for the parent.

For a pull request the block carries: title, state, head commit and its tree,
base branch and whether the head is behind it, mergeable state, check runs on the
head (``cancelled`` is its own state, never folded into success or failure),
reviews as ``approved @<sha>`` with approvals on an older commit marked stale,
assignees, labels, and the status lines (owner, next, blocker, waiting on) from
the top of the body. Nothing else from the body is kept: a body can say anything,
so the block tells the parent what to look at, never what to do. A ``head:`` or
``tree:`` line in the body is never shown as a fact; it is only checked against
the real values.
"""

from __future__ import annotations

from collections.abc import Callable, Iterable
import asyncio
import re
import time
from urllib.parse import quote

from .client import GitHubAPI, GitHubError

LINK = re.compile(r"https?://(?:www\.)?github\.com/([A-Za-z0-9](?:[A-Za-z0-9-]{0,38}))/([A-Za-z0-9._-]{1,100})"
                  r"/(pull|issues)/(\d{1,9})(?!\d)")
MAX_LINKS = 3
MERGEABLE_RETRY_DELAY = 1.0
PAGE_SIZE = 100
MAX_PAGES = 3
STATUS_LINE = re.compile(
    r"^\s*(?:[-*•]\s*)?[*_`]*\s*(owner|next action|next|blocker|waiting[- ]on|head|tree)\s*[*_`]*\s*:"
    r"\s*[*_`]*\s*(.+?)\s*[*_`]*\s*$", re.IGNORECASE)
HTML_COMMENT = re.compile(r"<!--.*?(?:-->|$)", re.DOTALL)
STATUS_LINES_SCANNED = 20
STATUS_ORDER = ("owner", "next", "blocker", "waiting on")
CLAIMS = ("head", "tree")
VALUE_LIMIT = 200
TITLE_LIMIT = 200
FAILED = {"failure", "timed_out", "action_required", "startup_failure"}


def links(texts: Iterable[str]) -> list[tuple[str, str, str, int]]:
    """Distinct (owner, repo, kind, number) for pull request and issue links, in order, at most MAX_LINKS."""
    found: list[tuple[str, str, str, int]] = []
    for text in texts:
        for match in LINK.finditer(text or ""):
            owner, repo, kind, number = match.group(1), match.group(2), match.group(3), int(match.group(4))
            if repo in (".", ".."):
                continue
            if all((owner.lower(), repo.lower(), number) != (o.lower(), r.lower(), n) for o, r, _, n in found):
                found.append((owner, repo, "pull" if kind == "pull" else "issue", number))
            if len(found) == MAX_LINKS:
                return found
    return found


def status_lines(body: str | None) -> tuple[dict[str, str], dict[str, str]]:
    """(status, claims) from the top of a body: owner / next / blocker / waiting on, and any head / tree claims.

    Every other line is dropped, and so are HTML comments, which GitHub does not display.
    """
    found: dict[str, str] = {}
    text = HTML_COMMENT.sub("", body or "") if isinstance(body, str) else ""
    for line in [line for line in text.splitlines() if line.strip()][:STATUS_LINES_SCANNED]:
        match = STATUS_LINE.match(line)
        if match:
            key = match.group(1).lower().replace("-", " ")
            found.setdefault({"next action": "next"}.get(key, key), match.group(2)[:VALUE_LIMIT])
    return ({key: found[key] for key in STATUS_ORDER if key in found},
            {key: found[key] for key in CLAIMS if key in found})


def status_block(body: str | None) -> dict[str, str]:
    return status_lines(body)[0]


def claim_matches(claim: str, actual: str) -> bool:
    """A body's head or tree claim matches when it is a hex prefix (7+ digits) of the real SHA."""
    claim = claim.strip().strip("`").lower()
    return len(claim) >= 7 and re.fullmatch(r"[0-9a-f]+", claim) is not None and actual.lower().startswith(claim)


def checks_summary(runs: list) -> tuple[str, dict[str, int]]:
    """The overall CI word and counts per outcome; cancelled and pending stay separate from success and failure."""
    counts = {"success": 0, "failure": 0, "cancelled": 0, "pending": 0, "other": 0}
    for run in runs:
        if not isinstance(run, dict):
            continue
        if run.get("status") != "completed":
            counts["pending"] += 1
        elif run.get("conclusion") == "success":
            counts["success"] += 1
        elif run.get("conclusion") in FAILED:
            counts["failure"] += 1
        elif run.get("conclusion") == "cancelled":
            counts["cancelled"] += 1
        else:
            counts["other"] += 1  # neutral, skipped, stale
    if not any(counts.values()):
        return "none", counts
    for word in ("failure", "cancelled", "pending"):
        if counts[word]:
            return word, counts
    return ("success" if counts["success"] else "neutral"), counts


def reviews_summary(reviews: list, head: str) -> tuple[list[str], int, int, int]:
    """Each reviewer's latest decisive review, with approvals on an older commit marked stale.

    A later COMMENTED review does not change a reviewer's decision (GitHub's rule). Returns (lines, approvals on the
    head, stale approvals, reviewers whose latest decision is an approval or a change request).
    """
    latest: dict[str, dict] = {}
    for review in reviews:
        if not isinstance(review, dict) or review.get("state") not in ("APPROVED", "CHANGES_REQUESTED", "DISMISSED"):
            continue
        user = review.get("user") if isinstance(review.get("user"), dict) else {}
        latest[user.get("login") or f"unknown-{review.get('id', len(latest))}"] = review
    lines, current, stale = [], 0, 0
    for login, review in latest.items():
        commit = review.get("commit_id") if isinstance(review.get("commit_id"), str) else ""
        if review["state"] == "APPROVED":
            if commit == head:
                current += 1
                lines.append(f"approved @{commit[:7]} by {login}")
            else:
                stale += 1
                lines.append(f"approved @{commit[:7]} by {login} (stale: not the head)")
        elif review["state"] == "CHANGES_REQUESTED":
            lines.append(f"changes requested @{commit[:7]} by {login}")
        else:
            lines.append(f"dismissed @{commit[:7]} by {login}")
    decisive = sum(1 for review in latest.values() if review["state"] != "DISMISSED")
    return lines, current, stale, decisive


class GitHubLinks:
    """Fetches and caches link state per (repo, number) for ``cache_seconds``; pauses all calls after a rate limit."""

    def __init__(self, api: GitHubAPI, *, cache_seconds: float = 180.0, clock: Callable[[], float] = time.monotonic,
                 retry_delay: float = MERGEABLE_RETRY_DELAY):
        self.api = api
        self.cache_seconds = cache_seconds
        self.clock = clock
        self.retry_delay = retry_delay
        self.cache: dict[tuple[str, str, int], tuple[float, dict]] = {}
        self.paused_until = 0.0

    async def linked(self, texts: Iterable[str]) -> tuple[dict, ...]:
        return tuple(await asyncio.gather(*(self.state(*found) for found in links(texts))))

    async def state(self, owner: str, repo: str, kind: str, number: int) -> dict:
        key = (owner.lower(), repo.lower(), number)
        cached = self.cache.get(key)
        if cached is not None and cached[0] > self.clock():
            return cached[1]
        link = f"https://github.com/{owner}/{repo}/{'pull' if kind == 'pull' else 'issues'}/{number}"
        if self.paused_until > self.clock():
            return {"link": link, "error": "not fetched: GitHub rate limit; set [github] token_env for more requests"}
        try:
            item = await self._fetch(owner, repo, kind, number)
        except GitHubError as error:
            if error.retry_after:
                self.paused_until = self.clock() + error.retry_after
            item = {"link": link, "error": str(error)}
        except Exception:  # unexpected JSON must cost this link, not every link
            item = {"link": link, "error": "unexpected response from GitHub"}
        self.cache[key] = (self.clock() + self.cache_seconds, item)
        return item

    async def _fetch(self, owner: str, repo: str, kind: str, number: int) -> dict:
        base = f"/repos/{quote(owner)}/{quote(repo)}"
        if kind == "issue":
            issue = _object(await self.api.get(f"{base}/issues/{number}"))
            if not issue.get("pull_request"):
                return self._issue(owner, repo, issue)
        try:
            pull = _object(await self.api.get(f"{base}/pulls/{number}"))
        except GitHubError as error:
            if kind == "pull" and not error.retry_after and str(error).startswith("not found"):
                return self._issue(owner, repo, _object(await self.api.get(f"{base}/issues/{number}")))
            raise
        if pull.get("state") == "open" and pull.get("mergeable_state") in (None, "unknown"):
            # GitHub computes mergeability lazily; the first answer is often "unknown".
            await asyncio.sleep(self.retry_delay)
            pull = _object(await self.api.get(f"{base}/pulls/{number}"))
        head = pull["head"]["sha"]
        tree, behind, runs, reviews = await asyncio.gather(
            self._optional(self.api.get(f"{base}/git/commits/{head}"), lambda data: data["tree"]["sha"]),
            self._optional(self.api.get(f"{base}/compare/{quote(pull['base']['ref'], safe='/')}...{head}"),
                           lambda data: int(data["behind_by"])),
            self._optional(self._check_runs(f"{base}/commits/{head}/check-runs"), lambda data: data),
            self._optional(self._pages(f"{base}/pulls/{number}/reviews"), lambda data: data))
        return self._pull(owner, repo, pull, tree, behind, runs, reviews)

    async def _pages(self, path: str) -> tuple[list, bool]:
        """Up to MAX_PAGES pages of a list endpoint, oldest first; the flag says whether the list is complete."""
        items: list = []
        for page in range(1, MAX_PAGES + 1):
            batch = await self.api.get(f"{path}?per_page={PAGE_SIZE}&page={page}")
            if not isinstance(batch, list):
                raise GitHubError("unexpected response from GitHub")
            items += batch
            if len(batch) < PAGE_SIZE:
                return items, True
        return items, False

    async def _check_runs(self, path: str) -> tuple[list, bool]:
        runs: list = []
        for page in range(1, MAX_PAGES + 1):
            data = _object(await self.api.get(f"{path}?per_page={PAGE_SIZE}&page={page}"))
            batch = data.get("check_runs") if isinstance(data.get("check_runs"), list) else []
            runs += batch
            total = data.get("total_count") if isinstance(data.get("total_count"), int) else len(runs)
            if len(runs) >= total or len(batch) < PAGE_SIZE:
                return runs, len(runs) >= total
        return runs, False

    async def _optional(self, request, extract):
        """One auxiliary field; a failure shows as unavailable instead of losing the whole block."""
        try:
            return extract(await request)
        except GitHubError as error:
            if error.retry_after:
                self.paused_until = self.clock() + error.retry_after
            return None
        except Exception:
            return None

    def _issue(self, owner: str, repo: str, issue: dict) -> dict:
        number = issue["number"]
        status, _ = status_lines(issue.get("body"))
        item = {"link": f"https://github.com/{owner}/{repo}/issues/{number}", "kind": "issue",
                "repo": f"{owner}/{repo}", "number": number, "title": _text(issue.get("title"), TITLE_LIMIT),
                "state": _text(issue.get("state"), 20) or "?", "assignees": _logins(issue.get("assignees")),
                "labels": _labels(issue.get("labels")), "status_block": status}
        parts = [f'issue {owner}/{repo}#{number} "{item["title"]}"', item["state"]]
        item["summary"] = " · ".join(parts + _status(status) + _people(item))
        return item

    def _pull(self, owner, repo, pull, tree, behind, runs, reviews) -> dict:
        number, head = pull["number"], pull["head"]["sha"]
        state = "merged" if pull.get("merged") else _text(pull.get("state"), 20) or "?"
        if state == "open" and pull.get("draft"):
            state = "draft"
        if runs is None:
            ci, counts = "unavailable", {}
        else:
            ci, counts = checks_summary(runs[0])
            if not runs[1] and ci in ("success", "neutral", "none"):
                ci = "incomplete"  # more runs than were listed; failures may be among them
        review_lines, current, stale, decisive = reviews_summary(reviews[0] if reviews else [], head)
        status, claims = status_lines(pull.get("body"))
        mergeable = _text(pull.get("mergeable_state"), 20) or "unknown"
        item = {"link": f"https://github.com/{owner}/{repo}/pull/{number}", "kind": "pull", "repo": f"{owner}/{repo}",
                "number": number, "title": _text(pull.get("title"), TITLE_LIMIT), "state": state, "head": head,
                "tree": tree or "unavailable", "base": pull["base"]["ref"],
                "behind_base": None if behind is None else behind > 0, "mergeable": mergeable, "ci": ci,
                "checks": counts, "reviews": review_lines if reviews is not None else "unavailable",
                "assignees": _logins(pull.get("assignees")), "labels": _labels(pull.get("labels")),
                "status_block": status}
        base = f"base {item['base']}" + ("" if behind is None else " (behind base)" if behind > 0 else " (up to date)")
        # The line the parent needs first, then the details.
        parts = [f'PR {owner}/{repo}#{number} "{item["title"]}"', state, f"head {head[:7]}", f"CI {ci}",
                 f"approvals {current}/{decisive}" if reviews is not None else "reviews unavailable"]
        parts += _status(status)
        parts += [f"tree {tree[:7] if tree else 'unavailable'}", base, f"mergeable {mergeable}"]
        if counts.get("cancelled"):
            parts.append(f"cancelled checks {counts['cancelled']}")
        if stale:
            parts.append(f"stale approvals {stale}")
        if reviews is not None and not reviews[1]:
            parts.append("reviews truncated")
        for name, actual in (("head", head), ("tree", tree or "")):
            if name in claims:
                verdict = "matches" if actual and claim_matches(claims[name], actual) else "does NOT match"
                item.setdefault("claims", {})[name] = verdict
                parts.append(f"body's {name} claim {verdict}")
        item["summary"] = " · ".join(parts + _people(item))
        return item


def _object(value) -> dict:
    if not isinstance(value, dict):
        raise GitHubError("unexpected response from GitHub")
    return value


def _text(value, limit: int) -> str:
    return value[:limit] if isinstance(value, str) else ""


def _logins(users) -> list[str]:
    return [user.get("login", "?") for user in users or [] if isinstance(user, dict)] if isinstance(users, list) else []


def _labels(labels) -> list[str]:
    return [label.get("name", "?") for label in labels or [] if isinstance(label, dict)] if isinstance(labels, list) else []


def _status(status: dict[str, str]) -> list[str]:
    return [f"{key} {value}" if key == "owner" else f"{key}: {value}" for key, value in status.items()]


def _people(item: dict) -> list[str]:
    parts = []
    if item["assignees"]:
        parts.append("assignees " + ", ".join(item["assignees"]))
    if item["labels"]:
        parts.append("labels " + ", ".join(item["labels"]))
    return parts
