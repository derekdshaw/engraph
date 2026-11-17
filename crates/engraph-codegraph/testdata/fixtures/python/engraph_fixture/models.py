"""Core data types of the fixture library."""

from __future__ import annotations

from enum import Enum

from engraph_fixture.text import initials, pluralize, title_case


class Genre(Enum):
    FICTION = "fiction"
    SCIENCE = "science"
    HISTORY = "history"


class Book:
    def __init__(self, title: str, author: str, pages: int, genre: Genre) -> None:
        self.title = title
        self.author = author
        self.pages = pages
        self.genre = genre

    def is_long(self) -> bool:
        return self.pages > 300

    def label(self) -> str:
        return title_case(self.title)

    def matches(self, genre: Genre) -> bool:
        return self.genre is genre

    def describe(self) -> str:
        return f"{self.label()} by {self.author}"


class Shelf:
    def __init__(self, name: str) -> None:
        self.name = name
        self.books: list[Book] = []

    def add(self, book: Book) -> None:
        self.books.append(book)

    def count(self) -> int:
        return len(self.books)

    def total_pages(self) -> int:
        return sum(book.pages for book in self.books)

    def longest(self) -> Book | None:
        if not self.books:
            return None
        return max(self.books, key=lambda book: book.pages)

    def by_genre(self, genre: Genre) -> list[Book]:
        return [book for book in self.books if book.matches(genre)]

    def describe(self) -> str:
        return f"{self.name} ({pluralize('book', self.count())})"


class Member:
    def __init__(self, member_id: int, name: str) -> None:
        self.member_id = member_id
        self.name = name

    def display_name(self) -> str:
        return initials(self.name)


class Loan:
    def __init__(self, book_title: str, member_id: int) -> None:
        self.book_title = book_title
        self.member_id = member_id
        self.days_out = 0

    def is_overdue(self) -> bool:
        return self.days_out > 14

    def renew(self) -> None:
        self.days_out = 0
