package fixture.models;

import fixture.text.Text;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;

public final class Shelf implements Describable {
    private final String name;
    private final List<Book> books = new ArrayList<>();

    public Shelf(String name) {
        this.name = name;
    }

    public String name() {
        return name;
    }

    public List<Book> books() {
        return books;
    }

    public void add(Book book) {
        books.add(book);
    }

    public int count() {
        return books.size();
    }

    public int totalPages() {
        int total = 0;
        for (Book book : books) {
            total += book.pages();
        }
        return total;
    }

    public Optional<Book> longest() {
        Book longest = null;
        for (Book book : books) {
            if (longest == null || book.pages() > longest.pages()) {
                longest = book;
            }
        }
        return Optional.ofNullable(longest);
    }

    public List<Book> byGenre(Genre genre) {
        List<Book> out = new ArrayList<>();
        for (Book book : books) {
            if (book.matches(genre)) {
                out.add(book);
            }
        }
        return out;
    }

    @Override
    public String describe() {
        return name + " (" + Text.pluralize("book", count()) + ")";
    }
}
