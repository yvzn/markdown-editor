//! Spell checking with the Windows Spell Checking API (ISpellCheckerFactory, Windows 8+): no
//! dictionaries to ship, and the user dictionary is shared with the rest of Windows. Only the
//! lines on screen (plus a margin) are checked, word by word, through a per-word cache, so a
//! repaint after typing or scrolling costs next to nothing. A word passes if any of the enabled
//! languages accepts it, which keeps mixed French / English text quiet without per-paragraph
//! language detection.

use std::collections::HashMap;
use std::ffi::c_void;

use windows_sys::core::GUID;
use windows_sys::Win32::Globalization::SpellCheckerFactory;
use windows_sys::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};

use crate::codehl;
use crate::sci::{self, Editor};
use crate::win::wide;

/// Languages checked by default; each falls back to any installed variant (fr-CA, en-GB, ...).
pub const DEFAULT_LANGUAGES: &str = "fr-FR,en-US";
/// Lines checked above and below the visible ones, so short scrolls find them already marked.
const MARGIN_LINES: usize = 30;
/// The cache is dropped past this many words (a very long session in a huge workspace).
const MAX_CACHE: usize = 50_000;

const IID_ISPELLCHECKERFACTORY: GUID = GUID::from_u128(0x8e018a9d_2415_4677_bf08_794ea61f94bb);

// ---------------------------------------------------------------- minimal COM plumbing

/// An owned COM interface pointer, released on drop. Methods are called by vtable slot.
struct Com(*mut c_void);

impl Com {
    /// The function in vtable slot `i` (0..=2 are IUnknown's).
    unsafe fn slot<F: Copy>(&self, i: usize) -> F {
        let vtbl = *(self.0 as *const *const usize);
        std::mem::transmute_copy(&*vtbl.add(i))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let release: unsafe extern "system" fn(*mut c_void) -> u32 = self.slot(2);
                release(self.0);
            }
        }
    }
}

type FnWide = unsafe extern "system" fn(*mut c_void, *const u16) -> i32;
type FnWideOut = unsafe extern "system" fn(*mut c_void, *const u16, *mut *mut c_void) -> i32;

/// Every string of an IEnumString, freeing each one.
fn drain_strings(e: &Com) -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        // IEnumString::Next(celt, rgelt, pceltFetched)
        let next: unsafe extern "system" fn(*mut c_void, u32, *mut *mut u16, *mut u32) -> i32 = e.slot(3);
        loop {
            let mut s: *mut u16 = std::ptr::null_mut();
            let mut got = 0u32;
            if next(e.0, 1, &mut s, &mut got) != 0 || got == 0 || s.is_null() {
                break;
            }
            let len = (0..).take_while(|&i| *s.add(i) != 0).count();
            out.push(String::from_utf16_lossy(std::slice::from_raw_parts(s, len)));
            CoTaskMemFree(s as *const c_void);
        }
    }
    out
}

/// One ISpellChecker, for one language.
struct Checker {
    com: Com,
    tag: String,
}

impl Checker {
    /// Whether the checker reports no error in `word`. ISpellChecker::Check (slot 4) returns an
    /// IEnumSpellingError; any error at all means the word is misspelled.
    fn accepts(&self, word: &[u16]) -> bool {
        unsafe {
            let check: FnWideOut = self.com.slot(4);
            let mut errs: *mut c_void = std::ptr::null_mut();
            if check(self.com.0, word.as_ptr(), &mut errs) != 0 || errs.is_null() {
                return true; // a failing checker must not flag the whole document
            }
            let errs = Com(errs);
            let next: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32 = errs.slot(3);
            let mut err: *mut c_void = std::ptr::null_mut();
            let hr = next(errs.0, &mut err);
            drop(Com(err));
            hr != 0 // S_OK (0) = an error was found; S_FALSE = none
        }
    }

    /// ISpellChecker::Suggest (slot 5).
    fn suggest(&self, word: &[u16]) -> Vec<String> {
        unsafe {
            let suggest: FnWideOut = self.com.slot(5);
            let mut e: *mut c_void = std::ptr::null_mut();
            if suggest(self.com.0, word.as_ptr(), &mut e) != 0 || e.is_null() {
                return Vec::new();
            }
            drain_strings(&Com(e))
        }
    }

    /// ISpellChecker::Add (slot 6): the Windows user dictionary for this language.
    fn add(&self, word: &[u16]) {
        unsafe {
            let add: FnWide = self.com.slot(6);
            add(self.com.0, word.as_ptr());
        }
    }
}

/// The installed language to use for `tag`: itself, else the main variant of its language
/// (en-US, then en-GB; fr-FR, de-DE, ...), else any variant. Windows ships, say, en-GB but
/// not en-US with a French install, next to regional ones like en-029 that sort first.
fn pick_language<'a>(tag: &str, supported: &'a [String]) -> Option<&'a String> {
    let lang = tag.split('-').next().unwrap_or(tag).to_ascii_lowercase();
    let main = [format!("{lang}-{}", lang.to_ascii_uppercase()), "en-US".into(), "en-GB".into()];
    let same_lang = |s: &&String| s.split('-').next().is_some_and(|l| l.eq_ignore_ascii_case(&lang));
    supported
        .iter()
        .find(|s| s.eq_ignore_ascii_case(tag))
        .or_else(|| main.iter().find_map(|m| supported.iter().filter(same_lang).find(|s| s.eq_ignore_ascii_case(m))))
        .or_else(|| supported.iter().find(same_lang))
}

/// Checkers for the requested language tags (comma-separated), each one replaced by an
/// installed variant of the same language when not available itself.
/// Also returns every language tag Windows can check.
fn create_checkers(tags: &str) -> (Vec<String>, Vec<Checker>) {
    let mut out: Vec<Checker> = Vec::new();
    unsafe {
        let mut f: *mut c_void = std::ptr::null_mut();
        if CoCreateInstance(&SpellCheckerFactory, std::ptr::null_mut(), CLSCTX_INPROC_SERVER, &IID_ISPELLCHECKERFACTORY, &mut f) != 0 || f.is_null() {
            return (Vec::new(), out);
        }
        let factory = Com(f);
        let supported = {
            let get: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32 = factory.slot(3);
            let mut e: *mut c_void = std::ptr::null_mut();
            if get(factory.0, &mut e) == 0 && !e.is_null() { drain_strings(&Com(e)) } else { Vec::new() }
        };
        let create: FnWideOut = factory.slot(5);
        for tag in tags.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let Some(pick) = pick_language(tag, &supported) else { continue };
            if out.iter().any(|c| c.tag == *pick) {
                continue;
            }
            let mut c: *mut c_void = std::ptr::null_mut();
            let w = wide(pick);
            if create(factory.0, w.as_ptr(), &mut c) == 0 && !c.is_null() {
                out.push(Checker { com: Com(c), tag: pick.clone() });
            }
        }
        (supported, out)
    }
}

/// English name of a language tag's language ("fr-FR" → "French").
fn language_name(tag: &str) -> String {
    use windows_sys::Win32::Globalization::{GetLocaleInfoEx, LOCALE_SENGLISHLANGUAGENAME};
    let mut buf = [0u16; 128];
    let w = wide(tag);
    let n = unsafe { GetLocaleInfoEx(w.as_ptr(), LOCALE_SENGLISHLANGUAGENAME, buf.as_mut_ptr(), buf.len() as i32) };
    if n > 1 { String::from_utf16_lossy(&buf[..n as usize - 1]) } else { tag.to_string() }
}

/// Primary language subtag, lowercase ("en-GB" → "en").
fn primary(tag: &str) -> String {
    tag.split('-').next().unwrap_or(tag).to_ascii_lowercase()
}

// ---------------------------------------------------------------- what counts as prose

/// Byte ranges of the lines `[lo, hi)` of `text` that are not prose: fenced code (fences
/// included) and YAML front matter. Both ends are line-aligned.
fn code_lines(text: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let line_start = |p: usize| text[..p].rfind('\n').map_or(0, |i| i + 1);
    let line_end = |p: usize| text[p..].find('\n').map_or(text.len(), |i| p + i + 1);
    for (start, end, _) in codehl::fenced_blocks(text) {
        out.push((line_start(start.saturating_sub(1)), line_end(end.min(text.len()))));
    }
    // Front matter: a first line of exactly "---", up to the next "---" or "..." line.
    if let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) {
        let mut pos = text.len() - rest.len();
        for line in rest.split_inclusive('\n') {
            pos += line.len();
            if matches!(line.trim_end(), "---" | "...") {
                out.push((0, pos));
                break;
            }
        }
    }
    out
}

/// Byte spans of one line that are not prose: inline code, URLs, link and image targets,
/// HTML tags / autolinks, entities and the target of a reference definition.
fn skip_spans(line: &str) -> Vec<(usize, usize)> {
    let b = line.as_bytes();
    let mut out = sci::url_ranges(line);
    // Reference definition: [label]: target
    let t = line.trim_start();
    if t.starts_with('[') {
        if let Some(i) = t.find("]:") {
            out.push((line.len() - t.len() + i, line.len()));
        }
    }
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'`' => {
                let n = b[i..].iter().take_while(|&&c| c == b'`').count();
                let fence = &line[i..i + n];
                let close = line[i + n..].find(fence).map_or(line.len(), |j| i + n + j + n);
                out.push((i, close));
                i = close;
                continue;
            }
            b'(' if i > 0 && b[i - 1] == b']' => {
                let close = line[i..].find(')').map_or(line.len(), |j| i + j + 1);
                out.push((i, close));
                i = close;
                continue;
            }
            b'<' if b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, b'/' | b'!')) => {
                let close = line[i..].find('>').map_or(line.len(), |j| i + j + 1);
                out.push((i, close));
                i = close;
                continue;
            }
            b'&' => {
                let n = b[i + 1..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'#').count();
                if n > 0 && b.get(i + 1 + n) == Some(&b';') {
                    out.push((i, i + n + 2));
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '\'' | '\u{2019}' | '-')
}

/// Whether a word is worth checking: no digits or underscores (identifiers), at least two
/// letters, not an acronym (DPI, HTTP) and not camelCase / PascalCase-with-humps (FooBar).
fn checkable(w: &str) -> bool {
    let letters = w.chars().filter(|c| c.is_alphabetic()).count();
    if letters < 2 || w.chars().any(|c| c.is_numeric() || c == '_') {
        return false;
    }
    if !w.chars().any(|c| c.is_lowercase()) {
        return false;
    }
    let mut prev_lower = false;
    for c in w.chars() {
        if c.is_uppercase() && prev_lower {
            return false;
        }
        prev_lower = c.is_lowercase();
    }
    true
}

/// Byte ranges of the prose words in the lines of `text` overlapping `[lo, hi)`.
pub fn prose_words(text: &str, lo: usize, hi: usize) -> Vec<(usize, usize)> {
    let code = code_lines(text);
    let mut out = Vec::new();
    let mut pos = text[..lo.min(text.len())].rfind('\n').map_or(0, |i| i + 1);
    while pos < text.len() && pos < hi {
        let end = text[pos..].find('\n').map_or(text.len(), |i| pos + i + 1);
        let line = &text[pos..end];
        if !code.iter().any(|&(s, e)| pos >= s && pos < e) {
            line_words(line, pos, &mut out);
        }
        pos = end;
    }
    out
}

fn line_words(line: &str, base: usize, out: &mut Vec<(usize, usize)>) {
    let skip = skip_spans(line);
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        if !is_word_char(chars[i].1) {
            i += 1;
            continue;
        }
        let j = (i..chars.len()).find(|&k| !is_word_char(chars[k].1)).unwrap_or(chars.len());
        let (s, e) = (chars[i].0, chars.get(j).map_or(line.len(), |c| c.0));
        let before = i.checked_sub(1).map(|k| chars[k].1);
        let after = chars.get(j).map(|c| c.1);
        let after2 = chars.get(j + 1).map(|c| c.1);
        i = j;
        if skip.iter().any(|&(a, b)| s < b && e > a) {
            continue;
        }
        // Part of a path, file name, domain, mention or tag: foo/bar, file.md, a@b, #tag.
        if matches!(before, Some('.' | '/' | '\\' | '@' | '#' | '$' | '%' | '~' | '=' | ':'))
            || matches!(after, Some('/' | '\\' | '@' | '='))
            || (matches!(after, Some('.' | ':')) && after2.is_some_and(|c| c.is_alphanumeric()))
        {
            continue;
        }
        let trim = |c: char| matches!(c, '_' | '\'' | '\u{2019}' | '-');
        let word = &line[s..e];
        let lead = word.len() - word.trim_start_matches(trim).len();
        let word = word.trim_matches(trim);
        // Hyphenated compounds are checked part by part (peut-être, e-mail, well-known).
        let mut off = s + lead;
        for part in word.split('-') {
            let p = part.trim_matches(trim);
            let p_off = off + (part.len() - part.trim_start_matches(trim).len());
            if checkable(p) {
                out.push((base + p_off, base + p_off + p.len()));
            }
            off += part.len() + 1;
        }
    }
}

// ---------------------------------------------------------------- the checker the app holds

pub struct Spell {
    pub enabled: bool,
    languages: String,
    /// Created on first use: loading the dictionaries is the only slow part.
    checkers: Option<Vec<Checker>>,
    /// Every language tag Windows can check (filled with the checkers).
    supported: Vec<String>,
    /// Word → accepted by at least one language.
    cache: HashMap<String, bool>,
    /// Caret position whose word was left unchecked because it was being typed.
    pub skipped_at: Option<usize>,
}

impl Spell {
    pub fn new(enabled: bool, languages: &str) -> Spell {
        Spell { enabled, languages: languages.to_string(), checkers: None, supported: Vec::new(), cache: HashMap::new(), skipped_at: None }
    }

    fn checkers(&mut self) -> &[Checker] {
        if self.checkers.is_none() {
            let (supported, checkers) = create_checkers(&self.languages);
            self.supported = supported;
            self.checkers = Some(checkers);
        }
        self.checkers.as_deref().unwrap_or(&[])
    }

    /// The requested languages, comma-separated (the `spellLanguages` setting).
    pub fn languages(&self) -> &str {
        &self.languages
    }

    /// Switch languages; the dictionaries are loaded again on next use.
    pub fn set_languages(&mut self, tags: &str) {
        self.languages = tags.to_string();
        self.checkers = None;
        self.cache.clear();
    }

    /// Short label of the languages in use, "FR · EN". Before the dictionaries are loaded it
    /// comes from the setting, so painting the toolbar never loads them.
    pub fn label(&self) -> String {
        let tags: Vec<String> = match &self.checkers {
            Some(c) => c.iter().map(|c| c.tag.clone()).collect(),
            None => self.languages.split(',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect(),
        };
        tags.iter().map(|t| primary(t).to_ascii_uppercase()).collect::<Vec<_>>().join(" · ")
    }

    /// Whether no requested language turned out to be installed (false until loaded).
    pub fn missing(&self) -> bool {
        self.checkers.as_ref().is_some_and(Vec::is_empty)
    }

    /// One entry per language Windows can check: (tag to request, English name, in use).
    pub fn available(&mut self) -> Vec<(String, String, bool)> {
        let active: Vec<String> = self.checkers().iter().map(|c| primary(&c.tag)).collect();
        let mut langs: Vec<String> = self.supported.iter().map(|t| primary(t)).collect();
        langs.sort();
        langs.dedup();
        let mut out: Vec<(String, String, bool)> = langs
            .into_iter()
            .filter_map(|l| {
                let tag = pick_language(&l, &self.supported)?.clone();
                let on = active.contains(&l);
                Some((tag.clone(), language_name(&tag), on))
            })
            .collect();
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    /// Tags of the languages actually in use (empty when none is installed).
    pub fn active_languages(&mut self) -> Vec<String> {
        self.checkers().iter().map(|c| c.tag.clone()).collect()
    }

    pub fn is_ok(&mut self, word: &str) -> bool {
        if let Some(&ok) = self.cache.get(word) {
            return ok;
        }
        let w = wide(&word.replace('\u{2019}', "'"));
        let checkers = self.checkers();
        let ok = checkers.is_empty() || checkers.iter().any(|c| c.accepts(&w));
        if self.cache.len() > MAX_CACHE {
            self.cache.clear();
        }
        self.cache.insert(word.to_string(), ok);
        ok
    }

    /// Suggestions from every language that rejects `word`, best first, without duplicates.
    pub fn suggest(&mut self, word: &str, max: usize) -> Vec<String> {
        let w = wide(&word.replace('\u{2019}', "'"));
        let mut out: Vec<String> = Vec::new();
        let lists: Vec<Vec<String>> = self.checkers().iter().filter(|c| !c.accepts(&w)).map(|c| c.suggest(&w)).collect();
        // Interleave so each language's best guess comes first.
        for rank in 0..lists.iter().map(Vec::len).max().unwrap_or(0) {
            for l in &lists {
                if let Some(s) = l.get(rank) {
                    if !out.contains(s) {
                        out.push(s.clone());
                    }
                }
            }
        }
        // Keep the typographic apostrophe the document uses.
        if word.contains('\u{2019}') {
            for s in &mut out {
                *s = s.replace('\'', "\u{2019}");
            }
        }
        out.truncate(max);
        out
    }

    /// Add `word` to the Windows user dictionary of every active language.
    pub fn add(&mut self, word: &str) {
        let w = wide(&word.replace('\u{2019}', "'"));
        for c in self.checkers() {
            c.add(&w);
        }
        self.cache.insert(word.to_string(), true);
    }

    /// Accept `word` until the app closes.
    pub fn ignore(&mut self, word: &str) {
        self.cache.insert(word.to_string(), true);
    }

    /// Re-mark the lines on screen (and a margin around them). `caret`: the word ending there
    /// is being typed, so it is left alone until the caret moves on.
    pub fn paint(&mut self, ed: &Editor, caret: Option<usize>) {
        let (lo, hi) = ed.visible_range(MARGIN_LINES);
        ed.clear_spelling(lo, hi);
        self.skipped_at = None;
        if !self.enabled {
            return;
        }
        let text = ed.text();
        for (s, e) in prose_words(&text, lo, hi) {
            if Some(e) == caret {
                self.skipped_at = caret;
                continue;
            }
            if !self.is_ok(&text[s..e]) {
                ed.mark_misspelled(s, e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::prose_words;

    fn words(t: &str) -> Vec<&str> {
        prose_words(t, 0, t.len()).into_iter().map(|(s, e)| &t[s..e]).collect()
    }

    #[test]
    fn picks_prose_words() {
        assert_eq!(words("Hello **wrold**, l'homme _aujourd’hui_."), ["Hello", "wrold", "l'homme", "aujourd’hui"]);
        assert_eq!(words("peut-être a well-known e-mail"), ["peut", "être", "well", "known", "mail"]);
    }

    #[test]
    fn skips_code_and_identifiers() {
        assert_eq!(words("Use `fooo bar` and snake_case or camelCase, HTTP v2"), ["Use", "and", "or"]);
        assert_eq!(words("text\n```rust\nlet zzz = 1;\n```\nmore"), ["text", "more"]);
        assert_eq!(words("~~~\nqqq\n"), Vec::<&str>::new());
        assert_eq!(words("---\ntitle: zzz\n---\nbody"), ["body"]);
    }

    #[test]
    fn picks_main_variant() {
        let s: Vec<String> = ["en-029", "en-AU", "en-GB", "fr-015", "fr-BE", "fr-FR"].map(String::from).to_vec();
        assert_eq!(super::pick_language("en-US", &s).unwrap(), "en-GB");
        assert_eq!(super::pick_language("fr-CA", &s).unwrap(), "fr-FR");
        assert_eq!(super::pick_language("en-AU", &s).unwrap(), "en-AU");
        assert!(super::pick_language("de-DE", &s).is_none());
    }

    /// Talks to the real Windows spell checker; passes vacuously when no language is installed.
    #[test]
    fn windows_checker() {
        unsafe { windows_sys::Win32::System::Com::CoInitializeEx(std::ptr::null(), 0) };
        let mut sp = super::Spell::new(true, super::DEFAULT_LANGUAGES);
        let langs = sp.active_languages();
        eprintln!("languages: {langs:?}");
        if langs.is_empty() {
            return;
        }
        assert!(!sp.is_ok("wroldd"));
        if langs.iter().any(|l| l.starts_with("en")) {
            assert!(sp.is_ok("world") && sp.is_ok("don't"));
        }
        if langs.iter().any(|l| l.starts_with("fr")) {
            assert!(sp.is_ok("aujourd'hui") && sp.is_ok("aujourd’hui") && sp.is_ok("l'homme") && sp.is_ok("être"));
            assert!(!sp.is_ok("bonjoru"));
        }
        let avail = sp.available();
        eprintln!("available: {avail:?}");
        assert_eq!(avail.iter().filter(|l| l.2).count(), langs.len());
        assert!(!sp.label().is_empty());
        let s = sp.suggest("wroldd", 6);
        eprintln!("suggestions: {s:?}");
        assert!(!s.is_empty());
        sp.ignore("wroldd");
        assert!(sp.is_ok("wroldd"));
    }

    #[test]
    fn skips_links_paths_and_html() {
        assert_eq!(words("[see docs](./doc/fichier.md) <span class=\"x\">ok</span>"), ["see", "docs", "ok"]);
        assert_eq!(words("open readme.md in src/main.rs at https://exemple.com/xyz"), ["open", "in", "at"]);
        assert_eq!(words("[ref]: https://a.b/c \"titre\"\na&nbsp;here"), ["ref", "here"]);
        assert_eq!(words("#tag @mention C:\\Users\\yvan"), Vec::<&str>::new());
    }
}
