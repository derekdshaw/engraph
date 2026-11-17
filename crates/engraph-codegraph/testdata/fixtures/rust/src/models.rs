//! Core data types of the fixture library.

#[derive(Clone, Copy, PartialEq)]
pub enum Genre {
    Fiction,
    Science,
    History,
}

#[derive(Clone)]
pub struct Book {
    pub title: String,
    pub author: String,
    pub pages: u32,
    pub genre: Genre,
}

impl Book {
    pub fn new(title: &str, author: &str, pages: u32, genre: Genre) -> Self {
        Self {
            title: title.to_string(),
            author: author.to_string(),
            pages,
            genre,
        }
    }

    pub fn is_long(&self) -> bool {
        self.pages > 300
    }

    pub fn label(&self) -> String {
        crate::text::title_case(&self.title)
    }

    pub fn matches(&self, genre: Genre) -> bool {
        self.genre == genre
    }
}

pub struct Shelf {
    pub name: String,
    pub books: Vec<Book>,
}

impl Shelf {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            books: Vec::new(),
        }
    }

    pub fn add(&mut self, book: Book) {
        self.books.push(book);
    }

    pub fn count(&self) -> usize {
        self.books.len()
    }

    pub fn total_pages(&self) -> u32 {
        self.books.iter().map(|b| b.pages).sum()
    }

    pub fn longest(&self) -> Option<&Book> {
        self.books.iter().max_by_key(|b| b.pages)
    }

    pub fn by_genre(&self, genre: Genre) -> Vec<&Book> {
        self.books.iter().filter(|b| b.matches(genre)).collect()
    }
}

pub struct Member {
    pub id: u32,
    pub name: String,
}

impl Member {
    pub fn new(id: u32, name: &str) -> Self {
        Self {
            id,
            name: name.to_string(),
        }
    }

    pub fn display_name(&self) -> String {
        crate::text::initials(&self.name)
    }
}

pub struct Loan {
    pub book_title: String,
    pub member_id: u32,
    pub days_out: u32,
}

impl Loan {
    pub fn new(book_title: &str, member_id: u32) -> Self {
        Self {
            book_title: book_title.to_string(),
            member_id,
            days_out: 0,
        }
    }

    pub fn is_overdue(&self) -> bool {
        self.days_out > 14
    }

    pub fn renew(&mut self) {
        self.days_out = 0;
    }
}

pub trait Describe {
    fn describe(&self) -> String;
}

impl Describe for Book {
    fn describe(&self) -> String {
        format!("{} by {}", self.label(), self.author)
    }
}

impl Describe for Shelf {
    fn describe(&self) -> String {
        format!("{} ({} books)", self.name, self.count())
    }
}
