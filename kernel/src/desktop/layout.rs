//! Where the icons are on the home screen, for every folder whose icons were moved: saved in
//! `/system/icons.cfg`, one icon per line:
//!
//! ```text
//! "/desktop" "notes.txt" 3 0
//! "/desktop" :terminal 0 1
//! ```
//!
//! The folder and the file name are in double quotes (names cannot contain them); the built-in
//! icons are `:terminal`, `:files`, `:paint` and `:up`. Then the column and the row in the icon grid,
//! counted from 0.

use crate::fs;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub const LAYOUT_PATH: &str = "/system/icons.cfg";

/// One icon's place: `id` is the icon as the file writes it (`"name"` or `:terminal`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Place {
    pub id: String,
    pub col: usize,
    pub row: usize,
}

/// `"text"` at the start of `s`: the text and the rest of `s`.
fn quoted(s: &str) -> Option<(&str, &str)> {
    let s = s.strip_prefix('"')?;
    let end = s.find('"')?;
    Some((&s[..end], &s[end + 1..]))
}

fn parse_line(line: &str) -> Option<(String, Place)> {
    let (folder, rest) = quoted(line.trim())?;
    let rest = rest.trim_start();
    let (id, rest) = if rest.starts_with('"') {
        let (name, rest) = quoted(rest)?;
        (format!("\"{name}\""), rest)
    } else {
        let end = rest.find(char::is_whitespace)?;
        (String::from(&rest[..end]), &rest[end..])
    };
    let mut numbers = rest.split_whitespace().map(|n| n.parse::<usize>().ok());
    let (col, row) = (numbers.next()??, numbers.next()??);
    if numbers.next().is_some() {
        return None;
    }
    Some((String::from(folder), Place { id, col, row }))
}

/// Every place in a layout file; lines that are not places (comments, mistakes) are skipped.
pub fn parse(text: &str) -> Vec<(String, Place)> {
    text.lines().filter(|l| !l.trim_start().starts_with('#')).filter_map(parse_line).collect()
}

pub fn format(entries: &[(String, Place)]) -> String {
    let mut text = String::from(
        "# Where the desktop's icons are, written when you move them (M on the\n\
         # home screen; A puts a folder's icons back in order). Each line: folder,\n\
         # icon (a name in quotes, or :terminal :files :paint :up), column, row.\n",
    );
    for (folder, place) in entries {
        text.push_str(&format!("\"{folder}\" {} {} {}\n", place.id, place.col, place.row));
    }
    text
}

fn read_all() -> Vec<(String, Place)> {
    match fs::with(|fs| fs.read(LAYOUT_PATH)) {
        Ok(data) => parse(&String::from_utf8_lossy(&data)),
        Err(_) => Vec::new(),
    }
}

/// The saved places of the icons of `folder`.
pub fn load(folder: &str) -> Vec<Place> {
    read_all().into_iter().filter(|(f, _)| f == folder).map(|(_, p)| p).collect()
}

/// Replaces the places saved for `folder` (none: the folder goes back to the automatic order).
pub fn save(folder: &str, places: &[Place]) -> fs::Result<()> {
    let mut entries: Vec<(String, Place)> = read_all().into_iter().filter(|(f, _)| f != folder).collect();
    entries.extend(places.iter().map(|p| (String::from(folder), p.clone())));
    let text = format(&entries);
    fs::with(|fs| fs.write(LAYOUT_PATH, text.as_bytes()))
}
