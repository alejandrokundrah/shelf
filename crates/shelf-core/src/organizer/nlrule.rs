//! Plain-English → [`Rule`]. A small offline keyword parser: no network, no
//! account, nothing leaves the PC. It understands things like
//!
//! - `put school PDFs in Study`
//! - `if it's an image over 2 MB convert to WebP, compress, then move to Pictures/Designs`
//! - `resize screenshots to 1200px wide and rename with the date`
//! - `move installers to Installers`
//!
//! It never guesses silently: if it can't find both *what* to match and *what
//! to do*, it says so, and the UI always shows the parsed rule before saving.

use super::{Action, Category, Condition, ImageFormat, Rule};
use std::path::PathBuf;

struct Tok {
    text: String,
    lower: String,
    /// The word ended a clause (comma, period, semicolon, arrow).
    end: bool,
}

fn tokenize(input: &str) -> Vec<Tok> {
    let input = input.replace('→', " then ").replace("->", " then ");
    let mut out: Vec<Tok> = Vec::new();
    for raw in input.split_whitespace() {
        let end = raw.ends_with(|c| matches!(c, ',' | ';' | '.' | ':'));
        let text = raw
            .trim_matches(|c: char| {
                matches!(c, ',' | ';' | ':' | '!' | '?' | '"' | '\'' | '(' | ')' | '“' | '”' | '‘' | '’')
            })
            .trim_end_matches('.')
            .to_string();
        if text.is_empty() {
            if end {
                if let Some(last) = out.last_mut() {
                    last.end = true;
                }
            }
            continue;
        }
        let lower = text.to_lowercase();
        out.push(Tok { text, lower, end });
    }
    out
}

fn format_word(w: &str) -> Option<ImageFormat> {
    match w {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "webp" => Some(ImageFormat::Webp),
        _ => None,
    }
}

fn category_word(w: &str) -> Option<Category> {
    Some(match w {
        "pdf" | "pdfs" => Category::Pdfs,
        "image" | "images" | "photo" | "photos" | "picture" | "pictures" | "screenshot"
        | "screenshots" => Category::Images,
        "installer" | "installers" | "setup" | "setups" => Category::Installers,
        "zip" | "zips" | "archive" | "archives" | "zipped" => Category::Archives,
        "doc" | "docs" | "document" | "documents" | "spreadsheet" | "spreadsheets" | "word"
        | "excel" | "powerpoint" | "slides" => Category::Documents,
        "song" | "songs" | "music" | "audio" => Category::Audio,
        "video" | "videos" | "movie" | "movies" => Category::Video,
        _ => return None,
    })
}

fn extension_word(w: &str) -> Option<&'static str> {
    Some(match w.trim_start_matches('.') {
        "png" => "png",
        "jpg" | "jpeg" => "jpg",
        "webp" => "webp",
        "gif" => "gif",
        "heic" => "heic",
        "svg" => "svg",
        "bmp" => "bmp",
        "mp3" => "mp3",
        "mp4" => "mp4",
        "csv" => "csv",
        "txt" => "txt",
        "docx" => "docx",
        "xlsx" => "xlsx",
        "pptx" => "pptx",
        "exe" => "exe",
        "msi" => "msi",
        _ => return None,
    })
}

/// Words that are never a useful "name contains" hint.
fn is_filler(w: &str) -> bool {
    matches!(
        w,
        "all" | "every" | "any" | "my" | "the" | "a" | "an" | "new" | "old" | "big" | "large"
            | "small" | "huge" | "file" | "files" | "and" | "or" | "of" | "these" | "those"
            | "them" | "it" | "its" | "that" | "which" | "if" | "when" | "whenever" | "is"
            | "are" | "i" | "so" | "to" | "in" | "into" | "then" | "also" | "just" | "only"
            | "folder" | "called" | "named" | "with" | "over" | "under" | "from"
    )
}

fn is_stop(w: &str) -> bool {
    matches!(
        w,
        "and" | "then" | "after" | "if" | "when" | "over" | "under" | "larger" | "bigger"
            | "smaller" | "convert" | "compress" | "resize" | "rename" | "that" | "which"
            | "with" | "but" | "also" | "above" | "below"
    )
}

fn parse_size(toks: &[Tok], j: usize) -> Option<(u64, usize)> {
    let t = &toks.get(j)?.lower;
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let value: f64 = num.parse().ok()?;
    let (unit, used) = if unit.is_empty() { (toks.get(j + 1)?.lower.as_str(), 2) } else { (unit, 1) };
    let mult = match unit {
        "b" | "bytes" => 1.0,
        "kb" | "k" | "kilobytes" => 1024.0,
        "mb" | "m" | "meg" | "megs" | "megabytes" => 1_048_576.0,
        "gb" | "g" | "gigabytes" => 1_073_741_824.0,
        _ => return None,
    };
    Some(((value * mult) as u64, used))
}

fn parse_dimensions(token: &str) -> Option<(u32, u32)> {
    if let Some((w, h)) = token.split_once('x') {
        let w = w.parse().ok()?;
        let h = h.trim_end_matches("px").parse().ok()?;
        return Some((w, h));
    }
    None
}

fn parse_pixels(token: &str) -> Option<u32> {
    token.trim_end_matches("px").parse().ok().filter(|n| *n > 0)
}

fn singular(word: &str) -> String {
    if word.len() > 4 && word.ends_with('s') && !word.ends_with("ss") {
        word[..word.len() - 1].to_string()
    } else {
        word.to_string()
    }
}

pub fn parse_rule(input: &str) -> Result<Rule, String> {
    let toks = tokenize(input);
    if toks.is_empty() {
        return Err("Type a rule first, for example: put school PDFs in Study".into());
    }
    let n = toks.len();
    let mut used = vec![false; n];
    let mut conditions: Vec<Condition> = Vec::new();
    let add_condition = |c: Condition, conditions: &mut Vec<Condition>| {
        if !conditions.contains(&c) {
            conditions.push(c);
        }
    };

    let mut convert: Option<ImageFormat> = None;
    let mut resize: Option<(u32, u32)> = None;
    let mut compress = false;
    let mut quality: Option<u8> = None;
    let mut rename: Option<String> = None;
    let mut dest: Option<PathBuf> = None;
    let mut objects: Vec<usize> = Vec::new();

    // convert [source] to <format>
    for i in 0..n {
        if used[i] || !matches!(toks[i].lower.as_str(), "convert" | "turn" | "change" | "transcode") {
            continue;
        }
        let limit = (i + 5).min(n);
        if let Some(k) = (i + 1..limit).find(|&k| matches!(toks[k].lower.as_str(), "to" | "into" | "as")) {
            if let Some(fmt) = toks.get(k + 1).and_then(|t| format_word(&t.lower)) {
                for j in i + 1..k {
                    if let Some(ext) = extension_word(&toks[j].lower) {
                        add_condition(Condition::Extension(ext.into()), &mut conditions);
                    }
                }
                convert = Some(fmt);
                (i..=k + 1).for_each(|j| used[j] = true);
            }
        }
    }

    // resize / scale
    for i in 0..n {
        if used[i] || !matches!(toks[i].lower.as_str(), "resize" | "resized" | "scale" | "downscale") {
            continue;
        }
        let limit = (i + 7).min(n);
        for j in i + 1..limit {
            if used[j] {
                continue;
            }
            if let Some(dims) = parse_dimensions(&toks[j].lower) {
                resize = Some(dims);
                used[j] = true;
                break;
            }
            if let Some(px) = parse_pixels(&toks[j].lower) {
                let tall = toks
                    .get(j + 1)
                    .map(|t| matches!(t.lower.as_str(), "tall" | "high" | "height"))
                    .unwrap_or(false);
                resize = Some(if tall { (0, px) } else { (px, 0) });
                used[j] = true;
                if toks.get(j + 1).map(|t| matches!(t.lower.as_str(), "wide" | "width" | "tall" | "high" | "height" | "px")).unwrap_or(false) {
                    used[j + 1] = true;
                }
                break;
            }
        }
        if resize.is_some() {
            used[i] = true;
            break;
        }
    }

    // compress / quality
    for i in 0..n {
        if !used[i] && matches!(toks[i].lower.as_str(), "compress" | "compressed" | "compressing" | "optimize" | "optimise" | "shrink") {
            compress = true;
            used[i] = true;
        }
    }
    for i in 0..n {
        if used[i] || toks[i].lower != "quality" {
            continue;
        }
        let near = [i.checked_sub(1), Some(i + 1)];
        for j in near.into_iter().flatten().filter(|j| *j < n) {
            let digits = toks[j].lower.trim_end_matches('%');
            if let Ok(q) = digits.parse::<u8>() {
                if (1..=100).contains(&q) {
                    quality = Some(q);
                    compress = true;
                    used[j] = true;
                    used[i] = true;
                    break;
                }
            }
        }
    }

    // rename
    let has_name_word = toks.iter().any(|t| matches!(t.lower.as_str(), "rename" | "prefix" | "add" | "append" | "name" | "stamp"));
    if let Some(d) = (0..n).find(|&i| !used[i] && matches!(toks[i].lower.as_str(), "date" | "dated" | "datestamp" | "timestamp")) {
        if has_name_word {
            rename = Some("{date}_{name}".into());
            used[d] = true;
        }
    }
    if let Some(r) = (0..n).find(|&i| !used[i] && toks[i].lower == "rename") {
        used[r] = true;
        if rename.is_none() {
            let limit = (r + 4).min(n);
            if let Some(k) = (r + 1..limit).find(|&k| matches!(toks[k].lower.as_str(), "to" | "as")) {
                if let Some(word) = toks.get(k + 1) {
                    if !is_stop(&word.lower) {
                        rename = Some(format!("{}_{{n}}", word.text));
                        used[k] = true;
                        used[k + 1] = true;
                    }
                }
            }
        }
    }

    // size
    for i in 0..n {
        if used[i] {
            continue;
        }
        let over = matches!(toks[i].lower.as_str(), "over" | "above" | "larger" | "bigger" | "greater" | "more" | "exceeding");
        let under = matches!(toks[i].lower.as_str(), "under" | "below" | "smaller" | "less" | "beneath");
        if !over && !under {
            continue;
        }
        let mut j = i + 1;
        if toks.get(j).map(|t| t.lower == "than").unwrap_or(false) {
            j += 1;
        }
        if let Some((bytes, count)) = parse_size(&toks, j) {
            add_condition(
                if over { Condition::SizeOver(bytes) } else { Condition::SizeUnder(bytes) },
                &mut conditions,
            );
            (i..j + count).for_each(|k| used[k] = true);
        }
    }

    // destination: <verb> … (to|in|into) <folder>
    let verb = (0..n).find(|&i| {
        !used[i]
            && matches!(
                toks[i].lower.as_str(),
                "move" | "put" | "send" | "file" | "save" | "store" | "sort" | "place" | "drop" | "organize" | "organise" | "stash"
            )
    });
    let prep_search_from = verb.map(|v| v + 1).unwrap_or(0);
    let prep = (prep_search_from..n).find(|&i| {
        !used[i]
            && (matches!(toks[i].lower.as_str(), "to" | "in" | "into" | "inside")
                && (verb.is_some() || toks[i].lower == "into"))
    });
    if let Some(p) = prep {
        let mut parts: Vec<&str> = Vec::new();
        let mut last = p;
        for k in p + 1..n {
            let w = toks[k].lower.as_str();
            if used[k] || is_stop(w) {
                break;
            }
            if matches!(w, "the" | "my" | "a" | "an" | "new" | "folder" | "called" | "named" | "directory") {
                last = k;
                if toks[k].end {
                    break;
                }
                continue;
            }
            parts.push(toks[k].text.as_str());
            last = k;
            if toks[k].end {
                break;
            }
        }
        if !parts.is_empty() {
            let joined = parts.join(" ").replace('\\', "/");
            let mut path = PathBuf::new();
            for seg in joined.split('/').filter(|s| !s.trim().is_empty() && *s != "." && *s != "..") {
                path.push(seg.trim());
            }
            if !path.as_os_str().is_empty() && !path.is_absolute() {
                dest = Some(path);
                (p..=last).for_each(|j| used[j] = true);
                if let Some(v) = verb {
                    used[v] = true;
                    objects = (v + 1..p).filter(|&j| !used[j]).collect();
                }
            }
        }
    }

    // explicit name filter: named/called/containing X
    for i in 0..n {
        if used[i] || !matches!(toks[i].lower.as_str(), "named" | "called" | "containing" | "contains" | "contain") {
            continue;
        }
        let mut j = i + 1;
        if toks.get(j).map(|t| t.lower == "with").unwrap_or(false) {
            j += 1;
        }
        if let Some(word) = toks.get(j).filter(|t| !is_filler(&t.lower)) {
            add_condition(Condition::NameContains(word.text.clone()), &mut conditions);
            (i..=j).for_each(|k| used[k] = true);
        }
    }

    // what kind of file: category / extension words, with an adjective before them
    let mut found_kind = false;
    for i in 0..n {
        if used[i] {
            continue;
        }
        let w = toks[i].lower.as_str();
        let ext = extension_word(w).filter(|_| w.starts_with('.') || format_word(w).is_some() || toks.get(i + 1).map(|t| matches!(t.lower.as_str(), "file" | "files")).unwrap_or(false));
        let cat = category_word(w);
        if ext.is_none() && cat.is_none() {
            continue;
        }
        found_kind = true;
        used[i] = true;
        if let Some(e) = ext {
            add_condition(Condition::Extension(e.into()), &mut conditions);
        } else if let Some(c) = cat {
            add_condition(Condition::Category(c), &mut conditions);
            if matches!(w, "screenshot" | "screenshots") {
                add_condition(Condition::NameContains("screenshot".into()), &mut conditions);
            }
        }
        // "school PDFs": the word right before the type narrows by name.
        if i > 0 && !used[i - 1] {
            let prev = toks[i - 1].lower.as_str();
            if !is_filler(prev) && !is_stop(prev) && !toks[i - 1].end && category_word(prev).is_none() && extension_word(prev).is_none() && prev.parse::<f64>().is_err() {
                add_condition(Condition::NameContains(toks[i - 1].text.clone()), &mut conditions);
                used[i - 1] = true;
            }
        }
    }

    // "put invoices in Finance": no file type, just a word.
    if !found_kind && conditions.is_empty() {
        if let Some(&j) = objects.iter().find(|&&j| !is_filler(&toks[j].lower)) {
            add_condition(Condition::NameContains(singular(&toks[j].text)), &mut conditions);
        }
    }

    let mut actions: Vec<Action> = Vec::new();
    if let Some((w, h)) = resize {
        actions.push(Action::Resize { max_width: w, max_height: h });
    }
    if let Some(f) = convert {
        actions.push(Action::Convert(f));
    }
    if compress {
        actions.push(Action::Compress { quality: quality.unwrap_or(80) });
    }
    if let Some(t) = rename {
        actions.push(Action::Rename(t));
    }
    if let Some(d) = dest {
        actions.push(Action::MoveTo(d));
    }

    if actions.is_empty() {
        return Err("I couldn't find what to do. Try \"move to <folder>\", \"convert to WebP\", \"compress\", \"resize to 1200px wide\" or \"rename with the date\".".into());
    }
    if conditions.is_empty() {
        return Err("I couldn't tell which files this applies to. Mention a type (PDFs, images, installers, zips…), a word in the name, or a size.".into());
    }
    let image_work = actions
        .iter()
        .any(|a| matches!(a, Action::Resize { .. } | Action::Convert(_) | Action::Compress { .. }));
    let has_kind = conditions.iter().any(|c| matches!(c, Condition::Category(_) | Condition::Extension(_)));
    if image_work && !has_kind {
        conditions.insert(0, Condition::Category(Category::Images));
    }

    let mut name: String = input.trim().chars().take(60).collect();
    if let Some(first) = name.get(..1) {
        name = first.to_uppercase() + &name[1..];
    }
    Ok(Rule { id: 0, name, enabled: true, conditions, actions })
}
