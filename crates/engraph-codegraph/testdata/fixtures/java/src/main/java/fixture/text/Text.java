package fixture.text;

import java.util.StringJoiner;

/** String helpers used from models and ops. */
public final class Text {
    private Text() {}

    public static String titleCase(String s) {
        StringJoiner joined = new StringJoiner(" ");
        for (String word : s.trim().split("\\s+")) {
            joined.add(capitalize(word));
        }
        return joined.toString();
    }

    public static String capitalize(String word) {
        if (word.isEmpty()) {
            return "";
        }
        return Character.toUpperCase(word.charAt(0)) + word.substring(1);
    }

    public static String initials(String name) {
        StringBuilder out = new StringBuilder();
        for (String word : name.trim().split("\\s+")) {
            if (!word.isEmpty()) {
                out.append(word.charAt(0));
            }
        }
        return out.toString();
    }

    public static String pluralize(String word, int n) {
        if (n == 1) {
            return "1 " + word;
        }
        return n + " " + word + "s";
    }

    public static String truncate(String s, int max) {
        return s.substring(0, Math.min(s.length(), max));
    }

    public static String slug(String s) {
        return s.toLowerCase().replace(" ", "-");
    }
}
