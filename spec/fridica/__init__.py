"""Fridica: a person's Slack presence backed by Claude Code and Codex workers."""

from importlib.metadata import PackageNotFoundError, version

try:
    __version__ = version("fridica")
except PackageNotFoundError:
    __version__ = "0+unknown"
