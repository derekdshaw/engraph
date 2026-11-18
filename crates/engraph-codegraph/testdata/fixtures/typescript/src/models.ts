// Core data types of the fixture library.
// Part of the stable-symbol-id snapshot fixture (plan 05): renaming or
// moving anything here invalidates the snapshot on purpose.

import { initials, pluralize, titleCase } from "./text";

export enum Genre {
  Fiction = "fiction",
  Science = "science",
  History = "history",
}

export interface Describable {
  describe(): string;
}

export class Book implements Describable {
  constructor(
    public title: string,
    public author: string,
    public pages: number,
    public genre: Genre,
  ) {}

  isLong(): boolean {
    return this.pages > 300;
  }

  label(): string {
    return titleCase(this.title);
  }

  matches(genre: Genre): boolean {
    return this.genre === genre;
  }

  describe(): string {
    return `${this.label()} by ${this.author}`;
  }
}

export class Shelf implements Describable {
  books: Book[] = [];

  constructor(public name: string) {}

  add(book: Book): void {
    this.books.push(book);
  }

  count(): number {
    return this.books.length;
  }

  totalPages(): number {
    return this.books.reduce((total, book) => total + book.pages, 0);
  }

  longest(): Book | undefined {
    let longest: Book | undefined;
    for (const book of this.books) {
      if (longest === undefined || book.pages > longest.pages) {
        longest = book;
      }
    }
    return longest;
  }

  byGenre(genre: Genre): Book[] {
    return this.books.filter((book) => book.matches(genre));
  }

  describe(): string {
    return `${this.name} (${pluralize("book", this.count())})`;
  }
}

export class Member {
  constructor(
    public id: number,
    public name: string,
  ) {}

  displayName(): string {
    return initials(this.name);
  }
}

export class Loan {
  daysOut = 0;

  constructor(
    public bookTitle: string,
    public memberId: number,
  ) {}

  isOverdue(): boolean {
    return this.daysOut > 14;
  }

  renew(): void {
    this.daysOut = 0;
  }
}
