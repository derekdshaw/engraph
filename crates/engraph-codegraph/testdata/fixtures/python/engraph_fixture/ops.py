"""Operations that cross the module boundary into models and text."""

from __future__ import annotations

from engraph_fixture.models import Book, Genre, Loan, Member, Shelf
from engraph_fixture.text import pluralize


class Library:
    def __init__(self) -> None:
        self.shelves: list[Shelf] = []
        self.members: list[Member] = []
        self.loans: list[Loan] = []

    def add_shelf(self, shelf: Shelf) -> None:
        self.shelves.append(shelf)

    def register(self, member: Member) -> None:
        self.members.append(member)

    def checkout(self, book: Book, member: Member) -> None:
        self.loans.append(Loan(book.title, member.member_id))

    def overdue(self) -> list[Loan]:
        return [loan for loan in self.loans if loan.is_overdue()]

    def book_count(self) -> int:
        return sum(shelf.count() for shelf in self.shelves)


def merge(a: Shelf, b: Shelf) -> Shelf:
    for book in b.books:
        a.add(book)
    return a


def average_pages(shelf: Shelf) -> int:
    if shelf.count() == 0:
        return 0
    return shelf.total_pages() // shelf.count()


def longest_title(shelf: Shelf) -> str | None:
    longest = shelf.longest()
    if longest is None:
        return None
    return longest.label()


def science_books(shelf: Shelf) -> int:
    return len(shelf.by_genre(Genre.SCIENCE))


def describe_all(shelf: Shelf) -> list[str]:
    return [book.describe() for book in shelf.books]


def summary(library: Library) -> str:
    shelves = pluralize("shelf", len(library.shelves))
    books = pluralize("book", library.book_count())
    return f"{shelves}, {books}"


def starter_library() -> Library:
    shelf = Shelf("general")
    shelf.add(Book("dune", "Frank Herbert", 412, Genre.FICTION))
    shelf.add(Book("cosmos", "Carl Sagan", 365, Genre.SCIENCE))
    library = Library()
    library.add_shelf(shelf)
    library.register(Member(1, "Ada Lovelace"))
    return library
