package fixture.models;

import fixture.text.Text;

/**
 * Part of the stable-symbol-id snapshot fixture (plan 05): renaming or moving
 * anything here invalidates the snapshot on purpose.
 */
public final class Book implements Describable {
    private final String title;
    private final String author;
    private final int pages;
    private final Genre genre;

    public Book(String title, String author, int pages, Genre genre) {
        this.title = title;
        this.author = author;
        this.pages = pages;
        this.genre = genre;
    }

    public String title() {
        return title;
    }

    public String author() {
        return author;
    }

    public int pages() {
        return pages;
    }

    public boolean isLong() {
        return pages > 300;
    }

    public String label() {
        return Text.titleCase(title);
    }

    public boolean matches(Genre other) {
        return genre == other;
    }

    @Override
    public String describe() {
        return label() + " by " + author;
    }
}
