//! File name helpers, ported from src/nautilus-filename-utilities.c.

/// Byte offset where the extension starts (at the dot), or `name.len()`
/// when there is none. `.tar.*` counts as one extension.
pub fn extension_start(name: &str) -> usize {
    let Some(first) = name.chars().next() else {
        return 0;
    };
    // The base name must keep at least one character.
    let start = first.len_utf8();
    let Some(dot) = name[start..].rfind('.').map(|i| i + start) else {
        return name.len();
    };
    let ext = &name[dot..];
    if ext.len() == 1 || ext.chars().any(char::is_whitespace) {
        return name.len();
    }
    // `get` keeps this safe when byte dot-4 isn't a char boundary.
    if dot > 4
        && name
            .get(dot - 4..dot)
            .is_some_and(|t| t.eq_ignore_ascii_case(".tar"))
    {
        return dot - 4;
    }
    dot
}

/// Splits `name` into `(base, extension)`; directories keep the whole name.
fn split(name: &str, ignore_extension: bool) -> (&str, &str) {
    if ignore_extension {
        (name, "")
    } else {
        name.split_at(extension_start(name))
    }
}

/// Finds a trailing ` (appendix)` on `base` and returns the base without it
/// plus the copy count `parse` read from the appendix.
fn parse_previous(base: &str, parse: fn(&str) -> Option<usize>) -> (&str, usize) {
    // Any appendix we add is at least 4 characters long.
    if base.len() <= 4 || !base.ends_with(')') {
        return (base, 0);
    }
    let inner = &base[..base.len() - 1];
    match inner.rfind(" (") {
        Some(0) | None => (base, 0),
        Some(pos) => parse(&inner[pos + 2..]).map_or((base, 0), |count| (&base[..pos], count)),
    }
}

fn appendix_name(
    name: &str,
    increment: usize,
    ignore_extension: bool,
    parse: fn(&str) -> Option<usize>,
    make: fn(usize) -> String,
) -> String {
    let (base, ext) = split(name, ignore_extension);
    let (base, count) = parse_previous(base, parse);
    format!("{base} ({}){ext}", make(count + increment))
}

fn parse_conflict(appendix: &str) -> Option<usize> {
    let n: usize = appendix.parse().ok()?;
    // Reject things like "(007)" that only parse to a number.
    (n.to_string() == appendix).then_some(n)
}

fn make_conflict(count: usize) -> String {
    // The first conflict is "(2)", not "(1)".
    count.max(2).to_string()
}

fn parse_copy(appendix: &str) -> Option<usize> {
    if appendix == "Copy" {
        return Some(1);
    }
    let n: usize = appendix.strip_prefix("Copy ")?.parse().ok()?;
    (format!("Copy {n}") == appendix).then_some(n)
}

fn make_copy(count: usize) -> String {
    if count == 1 {
        "Copy".into()
    } else {
        format!("Copy {count}")
    }
}

/// "a.txt" -> "a (2).txt" -> "a (3).txt"; used when a paste target exists.
pub fn for_conflict(name: &str, increment: usize, ignore_extension: bool) -> String {
    appendix_name(
        name,
        increment,
        ignore_extension,
        parse_conflict,
        make_conflict,
    )
}

/// "a.txt" -> "a (Copy).txt" -> "a (Copy 2).txt"; used for duplicates.
pub fn for_copy(name: &str, increment: usize, ignore_extension: bool) -> String {
    appendix_name(name, increment, ignore_extension, parse_copy, make_copy)
}

/// Symlink names (`nautilus_filename_for_link`): same name in another
/// folder, then "Link to a", "Link to a (2)", ...
pub fn for_link(name: &str, count: usize) -> String {
    match count {
        0 => name.to_owned(),
        1 => format!("Link to {name}"),
        n => format!("Link to {name} ({n})"),
    }
}

/// A file name for dropped text (files-view-dnd.c `get_drop_filename`): the
/// first sentence of the first 64 characters, cut at a word boundary, or
/// "Dropped Text.txt" when that is 8 characters or fewer.
pub fn for_dropped_text(text: &str) -> String {
    const MAX: usize = 64;
    const MIN: usize = 8;
    let head: String = text.trim_start().chars().take(MAX).collect();
    let sentence_end = head
        .char_indices()
        .find(|&(_, c)| matches!(c, '.' | '!' | '?' | '\n'))
        .map(|(i, _)| i);
    let cut = match sentence_end {
        Some(i) => &head[..i],
        // No sentence end: drop the last (possibly partial) word.
        None if head.chars().count() == MAX => head
            .rfind(char::is_whitespace)
            .map_or(&head[..], |i| &head[..i]),
        None => &head[..],
    };
    let cut = cut.trim();
    if cut.chars().count() <= MIN {
        return "Dropped Text.txt".into();
    }
    format!("{}.txt", cut.replace('/', "-"))
}

/// Rejects names GIO would refuse or that would escape the directory.
pub fn validate(name: &str) -> Result<(), &'static str> {
    match name {
        "" => Err("Name can't be empty"),
        "." | ".." => Err("“.” and “..” are reserved"),
        _ if name.contains('/') => Err("Names can't contain “/”"),
        _ if name.len() > 255 => Err("Name is too long"),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert_eq!(extension_start("a.txt"), 1);
        assert_eq!(extension_start(".bashrc"), 7);
        assert_eq!(extension_start("noext"), 5);
        assert_eq!(extension_start("trailing."), 9);
        assert_eq!(extension_start("pkg.tar.gz"), 3);
        assert_eq!(extension_start("my file.with space"), 18);
    }

    #[test]
    fn conflicts() {
        assert_eq!(for_conflict("a.txt", 1, false), "a (2).txt");
        assert_eq!(for_conflict("a (2).txt", 1, false), "a (3).txt");
        assert_eq!(for_conflict("a (007).txt", 1, false), "a (007) (2).txt");
        assert_eq!(for_conflict("dir.d", 1, true), "dir.d (2)");
        assert_eq!(for_conflict("x.tar.gz", 1, false), "x (2).tar.gz");
    }

    #[test]
    fn copies() {
        assert_eq!(for_copy("a.txt", 1, false), "a (Copy).txt");
        assert_eq!(for_copy("a (Copy).txt", 1, false), "a (Copy 2).txt");
        assert_eq!(for_copy("a (Copy 9).txt", 1, false), "a (Copy 10).txt");
    }

    #[test]
    fn links() {
        assert_eq!(for_link("a.txt", 0), "a.txt");
        assert_eq!(for_link("a.txt", 1), "Link to a.txt");
        assert_eq!(for_link("a.txt", 3), "Link to a.txt (3)");
    }

    #[test]
    fn dropped_text() {
        assert_eq!(for_dropped_text("hi"), "Dropped Text.txt");
        assert_eq!(
            for_dropped_text("Shopping list for the weekend. Eggs, milk"),
            "Shopping list for the weekend.txt"
        );
        assert_eq!(
            for_dropped_text("see a/b paths here\nmore"),
            "see a-b paths here.txt"
        );
        let long = "word ".repeat(20);
        let name = for_dropped_text(&long);
        assert!(name.len() <= 64 + 4 && name.ends_with("word.txt"), "{name}");
    }

    #[test]
    fn validation() {
        assert!(validate("ok").is_ok());
        assert!(validate("a/b").is_err());
        assert!(validate("..").is_err());
    }
}
