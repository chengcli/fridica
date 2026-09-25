import json

from fridica.workers.result import RESULT_SCHEMA, coerce, fallback, parse, prose

VALID = {"status": "done", "summary": "Fixed.", "changes": [], "validation": [], "artifacts": [],
         "machine_state": {"branch": "", "commit": "", "dirty": False, "notes": ""}, "unresolved": [],
         "question": "", "report": "Fixed it."}


def test_schema_is_strict_for_codex():
    def check(node):
        if node.get("type") == "object":
            assert node["additionalProperties"] is False and set(node["required"]) == set(node["properties"])
            for child in node["properties"].values():
                check(child)
        if node.get("type") == "array":
            check(node["items"])
    check(RESULT_SCHEMA)


def test_parses_bare_json_and_the_last_fenced_block():
    assert parse(json.dumps(VALID)).report == "Fixed it."
    text = f"Some prose.\n```json\n{json.dumps({**VALID, 'summary': 'old'})}\n```\nmore\n```json\n{json.dumps(VALID)}\n```"
    assert parse(text).summary == "Fixed."
    assert prose(text).startswith("Some prose.") and "Fixed." not in prose(text)


def test_coerce_bounds_and_filters():
    data = {**VALID, "summary": "x" * 5000, "artifacts": [
        {"path": "/w/a.png", "kind": "png", "caption": "a"}, {"path": "/w/b.exe", "kind": "exe", "caption": ""},
        {"path": "/w/c.pdf", "kind": "pdf"}, {"path": "/w/d.md", "kind": "md"}, {"path": "/w/e.md", "kind": "md"}],
        "changes": [{"path": "a", "change": "weird"}, "junk"], "validation": [{"command": "pytest", "outcome": "?"}]}
    result = coerce(data)
    assert len(result.summary) == 1500
    assert [item.path for item in result.artifacts] == ["/w/a.png", "/w/c.pdf", "/w/d.md"]
    assert result.changes[0].change == "modified" and result.validation[0].outcome == "skipped"


def test_missing_structure_is_none_and_fallback_is_partial():
    assert parse("no json here") is None
    assert coerce({"status": "done", "summary": ""}) is None
    assert coerce({"status": "great", "summary": "x"}) is None
    result = fallback("I did the thing.\n```json\n{broken\n```")
    assert result.status == "partial" and result.report.startswith("I did the thing.") and "{broken" in result.report


def test_prose_keeps_code_blocks_that_are_not_the_result():
    text = "Here is the fix:\n```python\nprint('hi')\n```\nand a broken block\n```json\n{broken\n```"
    assert prose(text) == text.strip()
    assert fallback(text).report.endswith("```")
    with_result = f"Done.\n```python\nx = 1\n```\n```json\n{json.dumps(VALID)}\n```"
    assert prose(with_result) == "Done.\n```python\nx = 1\n```"
