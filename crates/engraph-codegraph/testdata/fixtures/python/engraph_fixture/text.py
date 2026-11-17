"""String helpers used from models and ops."""

from __future__ import annotations


def title_case(s: str) -> str:
    return " ".join(capitalize(word) for word in s.split())


def capitalize(word: str) -> str:
    if not word:
        return ""
    return word[0].upper() + word[1:]


def initials(name: str) -> str:
    return "".join(word[0] for word in name.split())


def pluralize(word: str, n: int) -> str:
    if n == 1:
        return f"1 {word}"
    return f"{n} {word}s"


def truncate(s: str, max_len: int) -> str:
    return s[:max_len]


def slug(s: str) -> str:
    return s.lower().replace(" ", "-")
