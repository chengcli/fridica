"""F3: text attachments are read by the daemon and shown to the parent as untrusted data; optional (files:read)."""

import asyncio

from fridica.core.models import Attachment, Message
from fridica.slack import files
from fridica.slack.egress import FileUnavailable, SlackClient
from fridica.slack.ingress import normalize
from harness import Harness, action

URL = "https://files.slack.com/files-pri/T1-F1/fix.diff"
DIFF = b"diff --git a/meshblock.cpp b/meshblock.cpp\n-  old();\n+  radiating_boundary();\n"


def event(files_):
    return {"type": "event_callback", "event_id": "Ev1", "team_id": "TTEAM",
            "event": {"type": "message", "subtype": "file_share", "channel": "CROOM", "user": "UALICE",
                      "text": "<@UOWNER> please review", "ts": "100.000001", "files": files_}}


def diff_file(**values):
    item = {"id": "F1", "name": "fix.diff", "mimetype": "text/x-diff", "size": len(DIFF), "url_private": URL}
    item.update(values)
    return item


def run(harness, message):
    async def scenario():
        harness.daemon.receive(message)
        await harness.settle()
        await harness.daemon.close()
    asyncio.run(scenario())


def test_an_attached_diff_reaches_the_parents_context(config, store):
    """The named F3 test: a message with a .diff attachment; the context contains the diff text."""
    harness = Harness(config, store, lambda kind, data: action("Reviewed."))
    harness.slack.files[URL] = DIFF
    run(harness, normalize(event([diff_file()])))
    view = harness.llm.calls[0][1]["trigger"]["message"]["attachments"][0]
    assert view["text"] == DIFF.decode() and "Untrusted data" in view["header"]
    assert "attachments" not in harness.llm.calls[0][1]["history"][0]  # shown once, in the trigger


def test_without_files_read_the_parent_sees_names_only(config, store):
    harness = Harness(config, store, lambda kind, data: action("Reviewed."))
    run(harness, normalize(event([diff_file()])))
    view = harness.llm.calls[0][1]["trigger"]["message"]["attachments"][0]
    assert view == {"name": "fix.diff", "mimetype": "text/x-diff", "size": len(DIFF),
                    "note": "not read: the Slack token lacks files:read"}


class Slack:
    def __init__(self, data):
        self.data = data

    async def download(self, url, limit, *, html=False):
        return self.data[:limit + 1], len(self.data)


def message(*attachments):
    return Message("e1", "TTEAM", "CROOM", "1.0", None, "UALICE", "see file", attachments=attachments)


def test_text_is_capped_at_64_kb_with_a_marker_and_types_are_checked():
    big = ("é" * 40000).encode()  # 80,000 bytes; the cut falls inside a two-byte character
    views = asyncio.run(files.read(Slack(big), [message(Attachment("F1", "log.txt", "text/plain", len(big), URL))]))
    view = views["e1"][0]
    assert view["truncated"] is True and view["text"].endswith("[… truncated: first 64 KB of 80,000 bytes]")
    assert len(view["text"].encode()) < files.FILE_LIMIT + 100
    image = Attachment("F2", "plot.png", "image/png", 10, URL)
    binary = Attachment("F3", "data.txt", "text/plain", 3, URL)
    views = asyncio.run(files.read(Slack(b"\xff\xfe\x00"), [message(image, binary)]))
    assert [view["note"] for view in views["e1"]] == ["not read: not a text file", "not read: not UTF-8 text"]


def test_at_most_three_files_are_read_per_reply():
    many = [Attachment(f"F{index}", f"part{index}.md", "text/markdown", 2, URL) for index in range(5)]
    views = asyncio.run(files.read(Slack(b"ok"), [message(*many)]))["e1"]
    assert [view.get("text") for view in views] == ["ok", "ok", "ok", None, None]
    assert views[3]["note"] == "not read: only 3 files are read per reply"


def test_the_token_only_ever_goes_to_slacks_file_host(config):
    foreign = normalize(event([diff_file(url_private="https://evil.example/fix.diff"),
                               diff_file(id="F2", url_private="http://files.slack.com/x"),
                               diff_file(id="F3", url_private="https://files.slack.com:8443/x")]))
    assert [item.url for item in foreign.attachments] == ["", "", ""]

    class Web:
        token = "xoxp-secret"

    client = SlackClient(config, Web())
    client.scopes = frozenset({"files:read"})
    try:
        asyncio.run(client.download("https://evil.example/fix.diff", 10))
    except FileUnavailable as error:
        assert "not a Slack file URL" in str(error)
    else:
        raise AssertionError("downloaded from a foreign host")
    client.scopes = frozenset({"chat:write"})
    try:
        asyncio.run(client.download(URL, 10))
    except FileUnavailable as error:
        assert "files:read" in str(error)
    else:
        raise AssertionError("downloaded without files:read")


def test_doctor_reports_a_missing_files_read_scope(config):
    from fridica.doctor.checks import check_file_scope
    from fridica.store import Store

    assert check_file_scope(config).status == "SKIP"
    store = Store(config.state.path)
    store.db.set_meta("slack_scopes", "channels:history,chat:write")
    assert check_file_scope(config).status == "WARN"
    store.db.set_meta("slack_scopes", "chat:write,files:read")
    assert check_file_scope(config).status == "PASS"
    store.close()


def test_download_reads_a_capped_body_and_rejects_slacks_sign_in_page(config):
    class Body:
        def __init__(self, data):
            self.data = data

        async def read(self, limit):
            chunk, self.data = self.data[:min(limit, 7)], self.data[min(limit, 7):]  # short reads, like aiohttp
            return chunk

    class Response:
        def __init__(self, status, content_type, data):
            self.status, self.content_type, self.content = status, content_type, Body(data)
            self.content_length = len(data)

        async def __aenter__(self):
            return self

        async def __aexit__(self, *exc):
            return False

    class Session:
        def __init__(self, response):
            self.response, self.calls = response, []

        def get(self, url, **kwargs):
            self.calls.append((url, kwargs))
            return self.response

    class Web:
        token = "xoxp-secret"

        def __init__(self, response):
            self.session = Session(response)

        async def auth_test(self):
            class Reply(dict):
                headers = {"x-oauth-scopes": "chat:write, files:read"}
            return Reply(user_id="UOWNER", team_id="TTEAM")

        async def conversations_info(self, channel):
            return {"channel": {"is_member": True}}

    web = Web(Response(200, "text/plain", DIFF))
    client = SlackClient(config, web)
    asyncio.run(client.validate())
    assert client.scopes == {"chat:write", "files:read"}
    assert asyncio.run(client.download(URL, 10)) == (DIFF[:11], len(DIFF))  # assembled from 7-byte reads
    assert asyncio.run(client.download(URL, 10)) == (DIFF[:11], len(DIFF)) and len(web.session.calls) == 1
    assert web.session.calls[0][1]["headers"]["Authorization"] == "Bearer xoxp-secret"
    assert web.session.calls[0][1]["allow_redirects"] is False
    login = SlackClient(config, Web(Response(200, "text/html", b"<html>sign in</html>")))
    try:
        asyncio.run(login.download(URL, 10))
    except FileUnavailable as error:
        assert "did not return the file" in str(error)
    else:
        raise AssertionError("accepted Slack's sign-in page as the file")


def test_a_trigger_with_two_files_is_read_once(config, store):
    second = "https://files.slack.com/files-pri/T1-F2/tests.diff"
    harness = Harness(config, store, lambda kind, data: action("Reviewed."))
    harness.slack.files.update({URL: DIFF, second: b"+ test"})
    run(harness, normalize(event([diff_file(), diff_file(id="F2", name="tests.diff", url_private=second)])))
    views = harness.llm.calls[0][1]["trigger"]["message"]["attachments"]
    assert [view["name"] for view in views] == ["fix.diff", "tests.diff"] and all("text" in view for view in views)


def test_files_are_read_only_after_triage_says_respond(config, store):
    downloads = []
    harness = Harness(config, store, lambda kind, data: {"decision": "observe"} if kind == "triage" else action("x"))

    async def download(url, limit, *, html=False):
        downloads.append(url)
        return DIFF, len(DIFF)

    harness.slack.download = download
    unaddressed = event([diff_file()])
    unaddressed["event"]["text"] = "fyi, the log"
    run(harness, normalize(unaddressed))
    assert downloads == [] and harness.llm.calls[0][0] == "triage"
    assert "attachments" not in harness.llm.calls[0][1]["trigger"]["message"]


def test_total_budget_html_files_and_unknown_sizes():
    half = b"a" * (40 * 1024)
    two = [Attachment(f"F{index}", f"part{index}.txt", "text/plain", len(half), URL) for index in range(2)]
    views = asyncio.run(files.read(Slack(half), [message(*two)]))["e1"]
    assert "text" in views[0] and views[1]["note"] == "not read: over the 64 KB of attached text per reply"

    class Page(Slack):
        async def download(self, url, limit, *, html=False):
            self.html = html
            return b"<p>report</p>", 0

    page = Page(b"")
    view = asyncio.run(files.read(page, [message(Attachment("F9", "r.html", "text/html", 13, URL))]))["e1"][0]
    assert page.html is True and view["text"] == "<p>report</p>" and "13 bytes" in view["header"]


def test_scopes_unknown_when_slack_sends_no_header(config):
    class Web:
        token = "xoxp-secret"

        async def auth_test(self):
            return {"user_id": "UOWNER", "team_id": "TTEAM"}

        async def conversations_info(self, channel):
            return {"channel": {"is_member": True}}

    client = SlackClient(config, Web())
    asyncio.run(client.validate())
    assert client.scopes is None  # unknown: downloads are attempted rather than refused


def test_credentials_in_a_file_url_are_refused():
    from fridica.slack.ingress import file_url

    assert file_url(URL)
    for url in ("https://u@files.slack.com/x", "https://u:p@files.slack.com/x", "https://files.slack.com.evil.io/x",
                "https://evil.io/?files.slack.com", "https://[::1/x"):
        assert not file_url(url), url


def test_messages_stored_before_schema_v4_still_load(tmp_path):
    import sqlite3

    from fridica.store import Store
    from fridica.store.schema import MIGRATIONS

    path = tmp_path / "state.sqlite3"
    connection = sqlite3.connect(path)
    for number, script in enumerate(MIGRATIONS[:3], start=1):
        connection.executescript(script + f"\nINSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '{number}');")
    connection.execute("INSERT INTO messages (event_id, workspace, channel, ts, root_ts, sender, text, source, received_at)"
                       " VALUES ('e1', 'TTEAM', 'CROOM', '1.0', '1.0', 'UALICE', 'hi', 'socket', 1.0)")
    connection.commit()
    connection.close()
    store = Store(path)
    assert store.messages.get("e1").attachments == () and store.db.meta("schema_version") == str(len(MIGRATIONS))
    store.close()


def human_file(ts, file_id, name, data, harness, *, sender="UALICE", thread=None, text="the file"):
    url = f"https://files.slack.com/files-pri/T1-{file_id}/{name}"
    harness.slack.files[url] = data
    return Message(f"event-{ts}", "TTEAM", "CROOM", ts, thread, sender, text,
                   attachments=(Attachment(file_id, name, "text/markdown" if name.endswith(".md") else "text/x-diff",
                                           len(data), url),))


def uploaded(store, session_id, file_id, name):
    """An upload this daemon made: its outbox row holds the Slack file id as sent_ts."""
    from fridica.core.models import OutboxItem

    store.outbox.enqueue(OutboxItem(f"up-{file_id}", session_id, "upload", "CROOM", "100.000001", filename=name,
                                    blob=b"x"), 1.0)
    item = store.outbox.get(f"up-{file_id}")
    store.outbox.claim(item.id)
    store.outbox.sent(item.id, file_id)


def test_own_upload_echoes_do_not_take_the_slots_of_a_human_file(config, store):
    """Fix item 1: echoed details.md files next to an older human .diff; the diff must be in context."""
    harness = Harness(config, store, lambda kind, data: action("Reviewed."))

    async def scenario():
        harness.daemon.receive(human_file("100.000001", "FHUMAN", "fix.diff", DIFF, harness, text="here is the fix"))
        await harness.settle()
        for index in range(3):  # the daemon's own uploads, echoed back as the owner's messages
            name = f"details-{index}.md"
            uploaded(store, "TTEAM:CROOM:100.000001", f"FOWN{index}", name)
            harness.daemon.receive(human_file(f"100.00001{index}", f"FOWN{index}", name, b"# details", harness,
                                              sender="UOWNER", thread="100.000001", text=""))
        harness.message("<@UOWNER> is the diff above right?", thread="100.000001", ts="100.000020")  # no file
        await harness.settle()
        await harness.daemon.close()
    asyncio.run(scenario())
    data = harness.llm.calls[-1][1]
    root = next(item for item in data["history"] if item["ts"] == "100.000001")
    assert root["attachments"][0]["text"] == DIFF.decode()  # the older human diff is read from the thread
    views = [view for item in data["history"] for view in item.get("attachments", [])]
    own = [view for view in views if view["name"].startswith("details-")]
    assert len(own) == 3 and all(view["note"] == "not read: a file this Fridica posted itself" for view in own)


def test_a_file_shared_twice_is_read_once_and_ambiguous_own_uploads_are_recognised(config, store):
    from fridica.core.models import OutboxItem

    harness = Harness(config, store, lambda kind, data: action("Reviewed."))

    async def scenario():
        harness.daemon.receive(human_file("100.000001", "FDIFF", "fix.diff", DIFF, harness, text="the fix"))
        await harness.settle()
        # an upload whose outcome was unknown: no file id was recorded, but its echo arrived
        store.outbox.enqueue(OutboxItem("up-amb", "TTEAM:CROOM:100.000001", "upload", "CROOM", "100.000001",
                                        filename="details-9.md", blob=b"x"), 1.0)
        item = store.outbox.get("up-amb")
        store.outbox.claim(item.id)
        store.outbox.fail(item.id, "ambiguous", "daemon stopped while sending")
        harness.daemon.receive(human_file("100.000010", "FECHO", "details-9.md", b"# mine", harness,
                                          sender="UOWNER", thread="100.000001", text=""))
        harness.daemon.receive(human_file("100.000020", "FDIFF", "fix.diff", DIFF, harness, thread="100.000001",
                                          text="<@UOWNER> same diff again"))
        await harness.settle()
        await harness.daemon.close()
    asyncio.run(scenario())
    data = harness.llm.calls[-1][1]
    views = [view for item in data["history"] for view in item.get("attachments", [])]
    views += data["trigger"]["message"].get("attachments", [])
    assert sum(1 for view in views if view.get("text") == DIFF.decode()) == 1
    assert any(view.get("note") == "not read again: the same file is read from a newer message" for view in views)
    echo = next(view for view in views if view["name"] == "details-9.md")
    assert echo["note"] == "not read: a file this Fridica posted itself"


def test_clean_erases_attachments_so_nothing_is_read_after_restore(config, store):
    """Fix item 2: clean, restore, @-mention; no file is re-read."""
    downloads = []
    harness = Harness(config, store, lambda kind, data: action("Reviewed."))
    original = harness.slack.download

    async def counting(url, limit, *, html=False):
        downloads.append(url)
        return await original(url, limit, html=html)

    harness.slack.download = counting

    async def scenario():
        harness.daemon.receive(human_file("100.000001", "FDIFF", "fix.diff", DIFF, harness,
                                          text="<@UOWNER> review this"))
        await harness.settle()
        session = store.threads.list()[0].id
        await harness.daemon.thread_action(session, "clean", "owner")
        await harness.settle()
        await harness.daemon.thread_action(session, "restore", "owner")
        await harness.settle()
        harness.message("<@UOWNER> anything else?", thread="100.000001")
        await harness.settle()
        await harness.daemon.close()
    asyncio.run(scenario())
    assert len(downloads) == 1  # only the first reply read the file
    stored = store.messages.get("event-100.000001")
    assert stored.attachments == () and stored.files == ()
    assert "files.slack.com" not in str(harness.llm.calls[-1][1])


def test_attached_text_counts_toward_the_context_budget(config, store):
    """Fix item 3: an attachment over the remaining budget is truncated with the marker."""
    big = ("x" * 99 + "\n").encode() * 400  # 40,000 characters, under the 64 KB file cap
    harness = Harness(config, store, lambda kind, data: action("Reviewed."))
    run(harness, human_file("100.000001", "FBIG", "big.diff", big, harness, text="<@UOWNER> look"))
    view = harness.llm.calls[0][1]["trigger"]["message"]["attachments"][0]
    budget = config.parent.context_chars
    assert "truncated to fit the context budget" in view["text"] and view["truncated"] is True
    assert len(view["text"]) < budget + 200


def test_a_sign_in_page_gives_the_name_only_when_scopes_are_unknown(config):
    """Fix item 4: with unknown scopes an HTML answer is never taken for the file, not even for an HTML file."""
    class Response:
        status, content_type, content_length = 200, "text/html", 30

        class content:
            @staticmethod
            async def read(limit):
                return b"<html>Sign in to Slack</html>"

        async def __aenter__(self):
            return self

        async def __aexit__(self, *exc):
            return False

    class Web:
        token = "xoxp-secret"

        class session:
            @staticmethod
            def get(url, **kwargs):
                return Response()

    client = SlackClient(config, Web())
    assert client.scopes is None
    diff = Attachment("F1", "fix.diff", "text/x-diff", 30, URL)
    page = Attachment("F2", "report.html", "text/html", 30, URL.replace("fix.diff", "report.html"))
    views = asyncio.run(files.read(client, [message(diff, page)]))["e1"]
    assert all("text" not in view for view in views)
    assert views[0]["note"] == "not read: Slack did not return the file"
    assert views[1]["note"] == "not read: the token's scopes are unknown, so an HTML answer may be Slack's sign-in page"


def test_a_manual_owner_file_named_like_a_confirmed_upload_is_still_read(config, store):
    """Reviewer finding: a confirmed upload's name must not claim a different file the owner shared by hand."""
    harness = Harness(config, store, lambda kind, data: action("Reviewed."))

    async def scenario():
        harness.daemon.receive(human_file("100.000001", "FROOT", "start.md", b"# start", harness, text="start"))
        await harness.settle()
        uploaded(store, "TTEAM:CROOM:100.000001", "FOWN1", "details-1.md")  # confirmed: sent_ts holds FOWN1
        harness.daemon.receive(human_file("100.000010", "FMANUAL", "details-1.md", b"# my own notes", harness,
                                          sender="UOWNER", thread="100.000001", text="my notes, by hand"))
        await harness.settle()
        harness.message("<@UOWNER> what do your notes say?", thread="100.000001", ts="100.000020")
        await harness.settle()
        await harness.daemon.close()
    asyncio.run(scenario())
    manual = next(item for item in harness.llm.calls[-1][1]["history"] if item["ts"] == "100.000010")
    assert manual["attachments"][0]["name"] == "details-1.md" and manual["attachments"][0]["text"] == "# my own notes"


def test_the_truncation_marker_stays_inside_the_budget():
    """Reviewer finding: the rendered text, marker included, never exceeds what the budget allowed."""
    from fridica.threads.context import fit_attachments

    text = "x" * 3000
    for remaining in (1, 10, 60, 75, 76, 77, 100, 500, 1234, 2999, 3000, 3001):
        views = [{"text": text}]
        left = fit_attachments(views, remaining)
        if "text" in views[0]:
            assert len(views[0]["text"]) <= remaining, remaining
            assert left == (remaining - 3000 if remaining >= 3000 else 0)
            if remaining < 3000:
                assert views[0]["truncated"] is True and "truncated to fit the context budget" in views[0]["text"]
        else:
            assert views[0]["note"].startswith("not included") and left == 0
    two = [{"text": "a" * 100}, {"text": "b" * 300}]
    assert fit_attachments(two, 250) == 0 and 0 < len(two[1]["text"]) <= 150
    tight = [{"text": "a" * 100}, {"text": "b" * 100}]
    assert fit_attachments(tight, 150) == 0 and "text" not in tight[1]  # 50 left cannot hold text plus a marker


def test_remembered_failures_expire_and_are_capped(config, monkeypatch):
    from fridica.slack import egress

    client = SlackClient(config, type("Web", (), {"token": "xoxp-x"})())
    clock = [1000.0]
    monkeypatch.setattr(egress.time, "monotonic", lambda: clock[0])
    for index in range(egress.FAILURE_CACHE + 10):
        client._remember_failure(f"https://files.slack.com/f{index}", "no")
    assert len(client.failures) == egress.FAILURE_CACHE
    clock[0] += egress.FAILURE_TTL + 1
    client._remember_failure("https://files.slack.com/new", "no")
    assert list(client.failures) == ["https://files.slack.com/new"]
