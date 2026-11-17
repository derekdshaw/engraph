// Package models holds the core data types of the fixture library.
// Part of the stable-symbol-id snapshot fixture (plan 05): renaming or
// moving anything here invalidates the snapshot on purpose.
package models

import "example.com/engraph/fixture/text"

type Genre int

const (
	Fiction Genre = iota
	Science
	History
)

type Book struct {
	Title  string
	Author string
	Pages  int
	Genre  Genre
}

func NewBook(title, author string, pages int, genre Genre) Book {
	return Book{Title: title, Author: author, Pages: pages, Genre: genre}
}

func (b Book) IsLong() bool {
	return b.Pages > 300
}

func (b Book) Label() string {
	return text.TitleCase(b.Title)
}

func (b Book) Matches(g Genre) bool {
	return b.Genre == g
}

func (b Book) Describe() string {
	return b.Label() + " by " + b.Author
}

type Shelf struct {
	Name  string
	Books []Book
}

func NewShelf(name string) *Shelf {
	return &Shelf{Name: name}
}

func (s *Shelf) Add(b Book) {
	s.Books = append(s.Books, b)
}

func (s *Shelf) Count() int {
	return len(s.Books)
}

func (s *Shelf) TotalPages() int {
	total := 0
	for _, b := range s.Books {
		total += b.Pages
	}
	return total
}

func (s *Shelf) Longest() *Book {
	var longest *Book
	for i := range s.Books {
		if longest == nil || s.Books[i].Pages > longest.Pages {
			longest = &s.Books[i]
		}
	}
	return longest
}

func (s *Shelf) ByGenre(g Genre) []Book {
	var out []Book
	for _, b := range s.Books {
		if b.Matches(g) {
			out = append(out, b)
		}
	}
	return out
}

func (s *Shelf) Describe() string {
	return s.Name + " (" + text.Pluralize("book", s.Count()) + ")"
}

type Member struct {
	ID   int
	Name string
}

func NewMember(id int, name string) Member {
	return Member{ID: id, Name: name}
}

func (m Member) DisplayName() string {
	return text.Initials(m.Name)
}

type Loan struct {
	BookTitle string
	MemberID  int
	DaysOut   int
}

func NewLoan(bookTitle string, memberID int) Loan {
	return Loan{BookTitle: bookTitle, MemberID: memberID}
}

func (l Loan) IsOverdue() bool {
	return l.DaysOut > 14
}

func (l *Loan) Renew() {
	l.DaysOut = 0
}

type Describer interface {
	Describe() string
}
