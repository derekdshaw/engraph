// Package ops crosses the package boundary into models and text.
package ops

import (
	"example.com/engraph/fixture/models"
	"example.com/engraph/fixture/text"
)

type Library struct {
	Shelves []*models.Shelf
	Members []models.Member
	Loans   []models.Loan
}

func NewLibrary() *Library {
	return &Library{}
}

func (l *Library) AddShelf(s *models.Shelf) {
	l.Shelves = append(l.Shelves, s)
}

func (l *Library) Register(m models.Member) {
	l.Members = append(l.Members, m)
}

func (l *Library) Checkout(b models.Book, m models.Member) {
	l.Loans = append(l.Loans, models.NewLoan(b.Title, m.ID))
}

func (l *Library) Overdue() []models.Loan {
	var out []models.Loan
	for _, loan := range l.Loans {
		if loan.IsOverdue() {
			out = append(out, loan)
		}
	}
	return out
}

func (l *Library) BookCount() int {
	total := 0
	for _, s := range l.Shelves {
		total += s.Count()
	}
	return total
}

func Merge(a, b *models.Shelf) *models.Shelf {
	for _, book := range b.Books {
		a.Add(book)
	}
	return a
}

func AveragePages(s *models.Shelf) int {
	if s.Count() == 0 {
		return 0
	}
	return s.TotalPages() / s.Count()
}

func LongestTitle(s *models.Shelf) string {
	longest := s.Longest()
	if longest == nil {
		return ""
	}
	return longest.Label()
}

func ScienceBooks(s *models.Shelf) int {
	return len(s.ByGenre(models.Science))
}

func DescribeAll(s *models.Shelf) []string {
	var out []string
	for _, b := range s.Books {
		out = append(out, b.Describe())
	}
	return out
}

func Summary(l *Library) string {
	return text.Pluralize("shelf", len(l.Shelves)) + ", " + text.Pluralize("book", l.BookCount())
}

func StarterLibrary() *Library {
	shelf := models.NewShelf("general")
	shelf.Add(models.NewBook("dune", "Frank Herbert", 412, models.Fiction))
	shelf.Add(models.NewBook("cosmos", "Carl Sagan", 365, models.Science))
	library := NewLibrary()
	library.AddShelf(shelf)
	library.Register(models.NewMember(1, "Ada Lovelace"))
	return library
}
