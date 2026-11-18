package models

// Archived reports whether a book is shorter than the cutoff. It lives in a
// second file so the models package symbol is defined in two documents, the
// case the snapshot guards: the package must keep one canonical location.
func Archived(b Book, cutoffPages int) bool {
	return b.Pages < cutoffPages
}
