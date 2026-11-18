// Operations that cross the module boundary into models and text.

import { Book, Genre, Loan, Member, Shelf } from "./models";
import { pluralize } from "./text";

export class Library {
  shelves: Shelf[] = [];
  members: Member[] = [];
  loans: Loan[] = [];

  addShelf(shelf: Shelf): void {
    this.shelves.push(shelf);
  }

  register(member: Member): void {
    this.members.push(member);
  }

  checkout(book: Book, member: Member): void {
    this.loans.push(new Loan(book.title, member.id));
  }

  overdue(): Loan[] {
    return this.loans.filter((loan) => loan.isOverdue());
  }

  bookCount(): number {
    return this.shelves.reduce((total, shelf) => total + shelf.count(), 0);
  }
}

export function merge(a: Shelf, b: Shelf): Shelf {
  for (const book of b.books) {
    a.add(book);
  }
  return a;
}

export function averagePages(shelf: Shelf): number {
  if (shelf.count() === 0) {
    return 0;
  }
  return Math.floor(shelf.totalPages() / shelf.count());
}

export function longestTitle(shelf: Shelf): string | undefined {
  return shelf.longest()?.label();
}

export function scienceBooks(shelf: Shelf): number {
  return shelf.byGenre(Genre.Science).length;
}

export function describeAll(shelf: Shelf): string[] {
  return shelf.books.map((book) => book.describe());
}

export function summary(library: Library): string {
  const shelves = pluralize("shelf", library.shelves.length);
  const books = pluralize("book", library.bookCount());
  return `${shelves}, ${books}`;
}

export function starterLibrary(): Library {
  const shelf = new Shelf("general");
  shelf.add(new Book("dune", "Frank Herbert", 412, Genre.Fiction));
  shelf.add(new Book("cosmos", "Carl Sagan", 365, Genre.Science));
  const library = new Library();
  library.addShelf(shelf);
  library.register(new Member(1, "Ada Lovelace"));
  return library;
}
