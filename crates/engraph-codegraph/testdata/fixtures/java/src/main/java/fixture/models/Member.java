package fixture.models;

import fixture.text.Text;

public final class Member {
    private final int id;
    private final String name;

    public Member(int id, String name) {
        this.id = id;
        this.name = name;
    }

    public int id() {
        return id;
    }

    public String name() {
        return name;
    }

    public String displayName() {
        return Text.initials(name);
    }
}
