//! Operations that cross the module boundary into `models` and `text`.

use crate::models::{Book, Describe, Genre, Loan, Member, Shelf};
use crate::text;

pub struct Library {
    pub shelves: Vec<Shelf>,
    pub members: Vec<Member>,
    pub loans: Vec<Loan>,
}

impl Library {
    pub fn new() -> Self {
        Self {
            shelves: Vec::new(),
            members: Vec::new(),
            loans: Vec::new(),
        }
    }

    pub fn add_shelf(&mut self, shelf: Shelf) {
        self.shelves.push(shelf);
    }

    pub fn register(&mut self, member: Member) {
        self.members.push(member);
    }

    pub fn checkout(&mut self, book: &Book, member: &Member) {
        self.loans.push(Loan::new(&book.title, member.id));
    }

    pub fn overdue(&self) -> Vec<&Loan> {
        self.loans.iter().filter(|l| l.is_overdue()).collect()
    }

    pub fn book_count(&self) -> usize {
        self.shelves.iter().map(|s| s.count()).sum()
    }
}

impl Default for Library {
    fn default() -> Self {
        Self::new()
    }
}

pub fn merge(mut a: Shelf, b: Shelf) -> Shelf {
    for book in b.books {
        a.add(book);
    }
    a
}

pub fn average_pages(shelf: &Shelf) -> u32 {
    if shelf.count() == 0 {
        0
    } else {
        shelf.total_pages() / shelf.count() as u32
    }
}

pub fn longest_title(shelf: &Shelf) -> Option<String> {
    shelf.longest().map(|b| b.label())
}

pub fn science_books(shelf: &Shelf) -> usize {
    shelf.by_genre(Genre::Science).len()
}

pub fn describe_all(shelf: &Shelf) -> Vec<String> {
    shelf.books.iter().map(|b| b.describe()).collect()
}

pub fn summary(library: &Library) -> String {
    format!(
        "{}, {}",
        text::pluralize("shelf", library.shelves.len()),
        text::pluralize("book", library.book_count())
    )
}

pub fn starter_library() -> Library {
    let mut shelf = Shelf::new("general");
    shelf.add(Book::new("dune", "Frank Herbert", 412, Genre::Fiction));
    shelf.add(Book::new("cosmos", "Carl Sagan", 365, Genre::Science));
    let mut library = Library::new();
    library.add_shelf(shelf);
    library.register(Member::new(1, "Ada Lovelace"));
    library
}
