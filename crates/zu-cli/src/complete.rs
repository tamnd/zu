//! Tab completion: keywords, and the names the open file actually has.
//!
//! The point of completing against the catalog rather than against a
//! word list is that the shell knows something the user is guessing at.
//! A label that comes back from a tab is a label that is in the file,
//! so a tab that offers nothing is worth as much as one that offers a
//! name: it says the thing being typed is not there.
//!
//! What is offered depends on the character in front of the word, which
//! is as much context as a scanner has and is enough for the three
//! places a name appears. A colon in front means a label or a rel type,
//! as in `(a:Person)` and `-[r:KNOWS]->`. A dot means a property, as in
//! `a.age`. Anything else means a keyword, a table or a graph. The
//! wrong guess costs a list that has too much in it, never a wrong
//! insert, because everything offered is filtered by what was typed.
//!
//! Nothing here talks to the catalog. The caller gathers [`Names`] and
//! hands them over, which keeps this a function of two strings and a
//! list, testable without a database and cheap enough to run on a
//! keystroke.

use crate::highlight::{self, KEYWORDS, Kind};

/// The names the file holds, in the three places a statement puts them.
#[derive(Default)]
pub(crate) struct Names {
    /// What may follow a colon: labels and rel table names.
    pub(crate) labels: Vec<String>,
    /// What may follow a dot: the properties declared by a graph type.
    pub(crate) properties: Vec<String>,
    /// What may stand on its own: node and rel tables, and graphs.
    pub(crate) tables: Vec<String>,
}

impl Names {
    /// Sorts each list and drops the repeats, which a catalog produces
    /// freely: one label is on many tables and one property name is on
    /// many element types.
    pub(crate) fn tidy(&mut self) {
        for list in [&mut self.labels, &mut self.properties, &mut self.tables] {
            list.sort();
            list.dedup();
        }
    }
}

/// What a tab did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Completion {
    /// The text to insert at the cursor. Empty when the candidates
    /// share no more spelling than the user has already typed.
    pub(crate) insert: String,
    /// The candidates worth showing, empty when there was one and it
    /// was inserted.
    pub(crate) list: Vec<String>,
}

/// The completion for the word ending at `cursor`.
pub(crate) fn complete(text: &str, cursor: usize, names: &Names) -> Completion {
    // Inside a string or a comment there is no name to complete, and a
    // shell that offered SHORTEST inside a comment would be offering to
    // edit prose.
    if matches!(highlight::kind_at(text, cursor), Kind::Text | Kind::Comment) {
        return Completion::default();
    }
    let start = word_start(text, cursor);
    let prefix = &text[start..cursor];
    if prefix.is_empty() {
        // A tab on nothing lists the whole language, which is a screen
        // of keywords nobody asked for. It stays a tab that did
        // nothing.
        return Completion::default();
    }
    let before = text[..start].chars().next_back();
    let candidates = match before {
        Some(':') => matching(&names.labels, prefix),
        // A dot after a digit is a decimal point, and 1.5 is not a
        // property of 1. The dot is one byte, so what is in front of it
        // is what is in front of the word minus that byte.
        Some('.') if !ends_in_digit(&text[..start - 1]) => matching(&names.properties, prefix),
        Some('.') => Vec::new(),
        // A parameter is named by whoever runs the statement, so the
        // shell has nothing to offer and says so rather than offering
        // keywords for a word that cannot be one.
        Some('$') => Vec::new(),
        _ => {
            let mut all = matching(&names.tables, prefix);
            all.extend(keywords(prefix));
            all.sort();
            all.dedup();
            all
        }
    };
    if candidates.is_empty() {
        return Completion::default();
    }
    let shared = shared_prefix(&candidates);
    let insert = shared.get(prefix.len()..).unwrap_or("").to_string();
    if candidates.len() == 1 {
        return Completion {
            insert,
            list: Vec::new(),
        };
    }
    Completion {
        insert,
        list: candidates,
    }
}

/// The candidates laid out in columns for a window this wide, as lines
/// a raw terminal can be handed straight.
///
/// Down each column and then across, which is the order `ls` prints in
/// and the order a sorted list wants: a reader looking for a name runs
/// their eye down one column rather than across every row. Every line
/// ends `\r\n` because the driver is in raw mode and a bare newline
/// would step down without stepping back.
pub(crate) fn grid(list: &[String], width: usize) -> String {
    let widest = list
        .iter()
        .map(|c| crate::line::columns(c))
        .max()
        .unwrap_or(0);
    let cell = widest + 2;
    let columns = (width.max(1) / cell.max(1)).max(1);
    let rows = list.len().div_ceil(columns);
    let mut out = String::new();
    for row in 0..rows {
        let mut line = String::new();
        for column in 0..columns {
            let Some(name) = list.get(column * rows + row) else {
                continue;
            };
            line.push_str(name);
            if column + 1 < columns && list.len() > (column + 1) * rows + row {
                for _ in crate::line::columns(name)..cell {
                    line.push(' ');
                }
            }
        }
        out.push_str(line.trim_end());
        out.push_str("\r\n");
    }
    out
}

/// The names that start with `prefix`, ignoring case, spelled the way
/// the file spells them.
fn matching(names: &[String], prefix: &str) -> Vec<String> {
    names
        .iter()
        .filter(|name| starts_with(name, prefix))
        .cloned()
        .collect()
}

/// The keywords that start with `prefix`, in the case the user is
/// typing in.
///
/// A user who types `mat` gets `match`, and one who types `MAT` gets
/// `MATCH`. The language does not care which, and a shell that shouted
/// back at somebody typing in lower case would be a shell that made
/// them go back and fix the line. One lower case letter anywhere in
/// the prefix settles it, so `Matc` finishes as `Match` rather than as
/// the `MatcH` a straight append of the table's spelling would give.
fn keywords(prefix: &str) -> Vec<String> {
    let upper = !prefix.chars().any(char::is_lowercase);
    KEYWORDS
        .iter()
        .filter(|word| starts_with(word, prefix))
        .map(|word| {
            if upper {
                (*word).to_string()
            } else {
                word.to_lowercase()
            }
        })
        .collect()
}

/// Whether `name` starts with `prefix`, ignoring case, without
/// allocating a lowered copy of either.
fn starts_with(name: &str, prefix: &str) -> bool {
    name.len() >= prefix.len()
        && name
            .chars()
            .zip(prefix.chars())
            .all(|(a, b)| a.eq_ignore_ascii_case(&b))
}

/// The longest spelling every candidate begins with.
fn shared_prefix(candidates: &[String]) -> &str {
    let Some(first) = candidates.first().map(String::as_str) else {
        return "";
    };
    let mut end = first.len();
    for other in &candidates[1..] {
        end = end.min(other.len());
        while end > 0
            && (!first.is_char_boundary(end)
                || !other.is_char_boundary(end)
                || first[..end] != other[..end])
        {
            end -= 1;
        }
    }
    &first[..end]
}

/// The start of the word the cursor is in the middle or the end of.
fn word_start(text: &str, cursor: usize) -> usize {
    let mut at = cursor;
    while let Some(c) = text[..at].chars().next_back() {
        if !(c.is_alphanumeric() || c == '_') {
            break;
        }
        at -= c.len_utf8();
    }
    at
}

/// Whether the text ends in a digit, which is what tells a decimal
/// point from a property access.
fn ends_in_digit(text: &str) -> bool {
    text.chars().next_back().is_some_and(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Names {
        let mut names = Names {
            labels: ["Person", "Place", "KNOWS"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            properties: ["age", "name", "nation"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            tables: ["Person", "Place", "knows_graph"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        };
        names.tidy();
        names
    }

    /// Completes at the end of `text`, which is where a tab is pressed.
    fn at_end(text: &str) -> Completion {
        complete(text, text.len(), &names())
    }

    #[test]
    fn a_colon_completes_labels_and_a_single_match_is_inserted() {
        let done = at_end("MATCH (a:Per");
        assert_eq!(done.insert, "son");
        assert!(done.list.is_empty());
    }

    #[test]
    fn several_labels_share_what_they_can_and_the_rest_is_a_list() {
        let done = at_end("MATCH (a:P");
        // Person and Place share nothing past the P, so nothing is
        // inserted and both are offered.
        assert_eq!(done.insert, "");
        assert_eq!(done.list, ["Person", "Place"]);
    }

    #[test]
    fn a_dot_completes_properties_and_a_decimal_point_does_not() {
        assert_eq!(at_end("RETURN a.ag").insert, "e");
        // Two properties start with n, and they share the one letter.
        let both = at_end("RETURN a.n");
        assert_eq!(both.insert, "a");
        assert_eq!(both.list, ["name", "nation"]);
        assert_eq!(at_end("RETURN 1.n"), Completion::default());
    }

    #[test]
    fn a_bare_word_completes_keywords_in_the_case_it_was_typed_in() {
        assert_eq!(at_end("MATC").insert, "H");
        assert_eq!(at_end("matc").insert, "h");
        assert_eq!(at_end("Matc").insert, "h");
    }

    #[test]
    fn a_bare_word_completes_table_names_too() {
        let done = at_end("MATCH (a) COPY kno");
        assert_eq!(done.insert, "ws_graph");
        assert!(done.list.is_empty());
    }

    #[test]
    fn nothing_is_offered_where_a_name_cannot_go() {
        assert_eq!(at_end("RETURN 'Per"), Completion::default());
        assert_eq!(at_end("-- Per"), Completion::default());
        assert_eq!(at_end("/* Per"), Completion::default());
        assert_eq!(at_end("RETURN $per"), Completion::default());
        assert_eq!(at_end("MATCH "), Completion::default());
        assert_eq!(at_end("MATCH (a:Zzz"), Completion::default());
    }

    #[test]
    fn a_tab_in_the_middle_of_a_statement_completes_the_word_it_is_in() {
        let text = "MATCH (a:Per) RETURN a";
        let done = complete(text, "MATCH (a:Per".len(), &names());
        assert_eq!(done.insert, "son");
    }

    #[test]
    fn a_word_already_whole_inserts_nothing_and_lists_what_extends_it() {
        // MATCH is a keyword and nothing extends it, so the tab is a
        // tab that did nothing.
        let done = at_end("MATCH");
        assert_eq!(done.insert, "");
        assert!(done.list.is_empty());
        // Person is a label and a table, and the two spellings are the
        // same name, so it is offered once.
        let done = at_end("MATCH (a:Person");
        assert_eq!(done, Completion::default());
    }

    #[test]
    fn the_list_is_laid_out_down_the_columns_and_fits_the_window() {
        let list: Vec<String> = ["aa", "bb", "cc", "dd", "ee"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        // A window of twelve holds three columns of four, so five names
        // are two rows of three, two and read downwards.
        assert_eq!(grid(&list, 12), "aa  cc  ee\r\nbb  dd\r\n");
        // A window too narrow for two columns is one name per row.
        assert_eq!(grid(&list[..2], 3), "aa\r\nbb\r\n");
        assert_eq!(grid(&[], 80), "");
    }

    #[test]
    fn the_shared_prefix_stops_on_a_character_boundary() {
        let candidates = vec!["中文".to_string(), "中华".to_string()];
        assert_eq!(shared_prefix(&candidates), "中");
        assert_eq!(shared_prefix(&[]), "");
    }
}
