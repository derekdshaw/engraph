package fixture.models;

public final class Loan {
    private final String bookTitle;
    private final int memberId;
    private int daysOut;

    public Loan(String bookTitle, int memberId) {
        this.bookTitle = bookTitle;
        this.memberId = memberId;
        this.daysOut = 0;
    }

    public String bookTitle() {
        return bookTitle;
    }

    public int memberId() {
        return memberId;
    }

    public boolean isOverdue() {
        return daysOut > 14;
    }

    public void renew() {
        daysOut = 0;
    }
}
