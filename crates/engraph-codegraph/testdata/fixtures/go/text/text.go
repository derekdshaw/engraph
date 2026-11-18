// Package text holds string helpers used from models and ops.
package text

import (
	"strconv"
	"strings"
)

func TitleCase(s string) string {
	words := strings.Fields(s)
	for i, w := range words {
		words[i] = Capitalize(w)
	}
	return strings.Join(words, " ")
}

func Capitalize(word string) string {
	if word == "" {
		return ""
	}
	return strings.ToUpper(word[:1]) + word[1:]
}

func Initials(name string) string {
	var out strings.Builder
	for _, w := range strings.Fields(name) {
		out.WriteString(w[:1])
	}
	return out.String()
}

func Pluralize(word string, n int) string {
	if n == 1 {
		return "1 " + word
	}
	return strconv.Itoa(n) + " " + word + "s"
}

func Truncate(s string, max int) string {
	if len(s) <= max {
		return s
	}
	return s[:max]
}

func Slug(s string) string {
	return strings.ReplaceAll(strings.ToLower(s), " ", "-")
}
