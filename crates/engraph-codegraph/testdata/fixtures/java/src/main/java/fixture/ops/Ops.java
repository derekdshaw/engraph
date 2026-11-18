package fixture.ops;

import fixture.models.Book;
import fixture.models.Genre;
import fixture.models.Member;
import fixture.models.Shelf;
import fixture.text.Text;
import java.util.ArrayList;
import java.util.List;

/** Operations that cross the package boundary into models and text. */
public final class Ops {
    private Ops() {}

    public static Shelf merge(Shelf a, Shelf b) {
        for (Book book : b.books()) {
            a.add(book);
        }
        return a;
    }

    public static int averagePages(Shelf shelf) {
        if (shelf.count() == 0) {
            return 0;
        }
        return shelf.totalPages() / shelf.count();
    }

    public static String longestTitle(Shelf shelf) {
        return shelf.longest().map(Book::label).orElse("");
    }

    public static int scienceBooks(Shelf shelf) {
        return shelf.byGenre(Genre.SCIENCE).size();
    }

    public static List<String> describeAll(Shelf shelf) {
        List<String> out = new ArrayList<>();
        for (Book book : shelf.books()) {
            out.add(book.describe());
        }
        return out;
    }

    public static String summary(Library library) {
        String shelves = Text.pluralize("shelf", library.shelves().size());
        String books = Text.pluralize("book", library.bookCount());
        return shelves + ", " + books;
    }

    public static Library starterLibrary() {
        Shelf shelf = new Shelf("general");
        shelf.add(new Book("dune", "Frank Herbert", 412, Genre.FICTION));
        shelf.add(new Book("cosmos", "Carl Sagan", 365, Genre.SCIENCE));
        Library library = new Library();
        library.addShelf(shelf);
        library.register(new Member(1, "Ada Lovelace"));
        return library;
    }
}
