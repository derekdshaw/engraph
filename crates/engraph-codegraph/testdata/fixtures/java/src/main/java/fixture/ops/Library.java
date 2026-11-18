package fixture.ops;

import fixture.models.Book;
import fixture.models.Loan;
import fixture.models.Member;
import fixture.models.Shelf;
import java.util.ArrayList;
import java.util.List;

public final class Library {
    private final List<Shelf> shelves = new ArrayList<>();
    private final List<Member> members = new ArrayList<>();
    private final List<Loan> loans = new ArrayList<>();

    public List<Shelf> shelves() {
        return shelves;
    }

    public void addShelf(Shelf shelf) {
        shelves.add(shelf);
    }

    public void register(Member member) {
        members.add(member);
    }

    public void checkout(Book book, Member member) {
        loans.add(new Loan(book.title(), member.id()));
    }

    public List<Loan> overdue() {
        List<Loan> out = new ArrayList<>();
        for (Loan loan : loans) {
            if (loan.isOverdue()) {
                out.add(loan);
            }
        }
        return out;
    }

    public int bookCount() {
        int total = 0;
        for (Shelf shelf : shelves) {
            total += shelf.count();
        }
        return total;
    }
}
