//! Text extraction from uploaded files, for file search (`FileInfo.Content`), and the `extract_content` job that
//! catches up on files without content.
//!
//! As Go's `docextractor`: the extractors are tried in order, the first that matches the name and succeeds
//! wins: documents (docx, pptx, odt as their XML text, pdf through `pdftotext` as `docconv` runs it), archives
//! (the names of their entries), then plain text (anything that starts as printable UTF-8). `.html`, `.doc` and
//! `.rtf` run the external tools Go's `docconv` runs (`tidy`, `wvText`, `unrtf`), with Go's outcome when they are
//! missing: `.html` and `.rtf` fall through to plain text, `.doc` is tried as a docx.

// Based on Mattermost server/platform/services/docextractor/*.go, channels/app/file.go
// (ExtractContentFromFileInfo), channels/jobs/extract_content/worker.go, and docconv's docx.go, pptx.go, odt.go,
// doc.go, rtf.go, html.go, tidy.go, xml.go and pdf_text.go (code.sajari.com/docconv, MIT), Copyright (c)
// 2015-present Mattermost, Inc. Licensed under AGPL-3.0. HTML paragraphs as github.com/JalfResi/justext (MIT) finds
// them.

use std::io::Read;

use crate::app::{App, State, now_ms};
use crate::jobs::{self, Job};

const MAX_CONTENT: usize = 1024 * 1024;
const MAX_PLAIN: usize = 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 1000;

/// `ignoredFiles` of the job.
const IGNORED: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "wmv", "mpg", "mpeg", "mp3", "mp4", "ogg", "ogv", "mov", "apk", "svg", "webm", "mkv",
];

fn ext(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_string()).unwrap_or_default()
}

/// `docextractor.Extract`: the text of a file, empty if no extractor gives any.
pub fn extract(name: &str, data: &[u8], max_file_size: i64) -> String {
    let e = ext(name);
    let limited = |d: &[u8]| -> Result<Vec<u8>, String> {
        if max_file_size > 0 && d.len() as i64 > max_file_size {
            return Err("size limit exceeded".into());
        }
        Ok(d.to_vec())
    };
    // documentExtractor
    if matches!(e.as_str(), "doc" | "docx" | "pptx" | "odt" | "html" | "rtf" | "pdf")
        && let Ok(d) = limited(data)
        && let Ok(t) = document(&e, &d)
    {
        return t;
    }
    // archiveExtractor (without recursion into the entries, ArchiveRecursion's default)
    if is_archive(name)
        && let Ok(t) = archive(name, data)
    {
        return t;
    }
    plain(data, max_file_size)
}

fn document(e: &str, d: &[u8]) -> Result<String, String> {
    match e {
        "docx" => {
            let mut z =
                zip::ZipArchive::new(std::io::Cursor::new(d)).map_err(|e| format!("error unzipping data: {e}"))?;
            let (mut header, mut body, mut footer) = (String::new(), String::new(), String::new());
            for (ct, part) in overrides(&mut z)? {
                let target = match ct.as_str() {
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml" => &mut body,
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml" => &mut footer,
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml" => &mut header,
                    _ => continue,
                };
                let x = zip_file(&mut z, &part)?;
                *target += &xml_to_text(&x, &["br", "p", "tab"], &["instrText", "script"])?;
                *target += "\n";
            }
            Ok(format!("{header}\n{body}\n{footer}"))
        }
        "pptx" => {
            let mut z = zip::ZipArchive::new(std::io::Cursor::new(d)).map_err(|e| format!("could not unzip: {e}"))?;
            let mut body = String::new();
            for (ct, part) in overrides(&mut z)? {
                if ct == "application/vnd.openxmlformats-officedocument.presentationml.slide+xml"
                    || ct == "application/vnd.openxmlformats-officedocument.drawingml.diagramData+xml"
                {
                    let x = zip_file(&mut z, &part)?;
                    body += &xml_to_text(&x, &["br", "p", "tab"], &["instrText", "script"])?;
                    body += "\n";
                }
            }
            Ok(body.strip_suffix('\n').unwrap_or(&body).to_string())
        }
        "odt" => {
            let mut z =
                zip::ZipArchive::new(std::io::Cursor::new(d)).map_err(|e| format!("error unzipping data: {e}"))?;
            match zip_file(&mut z, "content.xml") {
                Ok(x) => xml_to_text(&x, &["br", "p", "tab"], &[]),
                Err(_) => Ok(String::new()),
            }
        }
        "pdf" => pdftotext(d),
        "html" => convert_html(d),
        "doc" => convert_doc(d),
        "rtf" => convert_rtf(d),
        _ => Err("unsupported".into()),
    }
}

/// A converter's input in a temporary file (docconv's `NewLocalFile`), removed when dropped.
struct TempFile(std::path::PathBuf);

impl TempFile {
    fn new(prefix: &str, d: &[u8]) -> Result<TempFile, String> {
        let path = std::env::temp_dir().join(format!("{prefix}{}", crate::app::new_id()));
        std::fs::write(&path, d).map_err(|e| format!("error creating local file: {e}"))?;
        Ok(TempFile(path))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `ConvertHTML(r, readability=true)`: `Tidy` (`tidy -numeric -asxml -quiet -utf8`, exit status 1 taken), then
/// justext with all-zero options and the class list `[""]`, which keeps no paragraph: the text is empty when
/// justext finds a paragraph and an error ("MAIN: P is nil") when it finds none. Without `tidy` (missing, or its
/// exit status 2), Go cleans the buffer `Tidy` has already read to the end, which leaves nothing: no paragraph,
/// the error, and the file goes on to the plain-text extractor.
fn convert_html(d: &[u8]) -> Result<String, String> {
    let f = TempFile::new("docconv", d)?;
    let out = std::process::Command::new("tidy")
        .args(["-numeric", "-asxml", "-quiet", "-utf8"])
        .arg(&f.0)
        .output()
        .map_err(|e| e.to_string())?;
    if !matches!(out.status.code(), Some(0 | 1)) {
        return Err("MAIN: P is nil".into());
    }
    if justext_has_paragraph(&String::from_utf8_lossy(&out.stdout)) {
        Ok(String::new())
    } else {
        Err("MAIN: P is nil".into())
    }
}

/// Whether justext's `ReadAll` finds a paragraph: the document parsed, `head`, `script` and `style` removed, only
/// the document's last child rendered (`nodesToString`), then a non-blank text closed by the start or end of a
/// paragraph tag (or a second `br` in a row). Text after the last such tag is never kept.
fn justext_has_paragraph(html: &str) -> bool {
    use html5ever::tendril::TendrilSink;
    use markup5ever_rcdom::{Handle, NodeData, RcDom};
    const PARAGRAPH: &[&str] = &[
        "blockquote",
        "caption",
        "center",
        "col",
        "colgroup",
        "dd",
        "div",
        "dl",
        "dt",
        "fieldset",
        "form",
        "legend",
        "optgroup",
        "option",
        "p",
        "pre",
        "table",
        "td",
        "textarea",
        "tfoot",
        "th",
        "thead",
        "tr",
        "ul",
        "li",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
    ];
    const SELF_CLOSING: &[&str] = &["area", "base", "basefont", "br", "hr", "input", "img", "link", "meta"];
    #[derive(Default)]
    struct Walk {
        pending: bool,
        br: bool,
        found: bool,
    }
    impl Walk {
        fn start(&mut self, name: &str) {
            if PARAGRAPH.contains(&name) || (name == "br" && self.br) {
                self.found |= self.pending;
                self.pending = false;
            } else {
                self.br = name == "br";
            }
        }
        fn end(&mut self, name: &str) {
            if PARAGRAPH.contains(&name) {
                self.found |= self.pending;
                self.pending = false;
            }
        }
        fn node(&mut self, n: &Handle) {
            match &n.data {
                NodeData::Text { contents } => {
                    if !contents.borrow().trim().is_empty() {
                        // addKwTags wraps it in a `kw` element first.
                        self.br = false;
                        self.pending = true;
                    }
                }
                NodeData::Element { name, .. } => {
                    let tag = name.local.to_string();
                    if matches!(tag.as_str(), "head" | "script" | "style") {
                        return;
                    }
                    self.start(&tag);
                    if SELF_CLOSING.contains(&tag.as_str()) {
                        return;
                    }
                    for c in n.children.borrow().iter() {
                        self.node(c);
                    }
                    self.end(&tag);
                }
                // A doctype renders as an empty `html` element.
                NodeData::Doctype { .. } => {
                    self.start("html");
                    self.end("html");
                }
                _ => {}
            }
        }
    }
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(html);
    let last = dom.document.children.borrow().last().cloned();
    let mut w = Walk::default();
    if let Some(n) = last {
        w.node(&n);
    }
    w.found
}

/// `ConvertDoc`: `wvText <file> <out>`; with no text (no `wvText` either) the file is read as a docx, which fails
/// for a real Word 97 file.
fn convert_doc(d: &[u8]) -> Result<String, String> {
    let f = TempFile::new("docconv", d)?;
    let out = TempFile::new("sajari-convert-", b"")?;
    let _ = std::process::Command::new("wvText").arg(&f.0).arg(&out.0).output();
    let body = std::fs::read(&out.0).unwrap_or_default();
    if body.is_empty() {
        return document("docx", d);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// `ConvertRTF`: `unrtf --nopict --text <file>` without its `### ` comment lines; an error without `unrtf`.
fn convert_rtf(d: &[u8]) -> Result<String, String> {
    let f = TempFile::new("docconv", d)?;
    let out = std::process::Command::new("unrtf")
        .args(["--nopict", "--text"])
        .arg(&f.0)
        .output()
        .map_err(|e| format!("unrtf error: {e}"))?;
    if !out.status.success() {
        return Err(format!("unrtf error: {}", out.status));
    }
    let mut text = String::new();
    for line in String::from_utf8_lossy(&out.stdout).split('\n') {
        if !line.starts_with("### ") {
            text.push_str(line);
            text.push('\n');
        }
    }
    Ok(text)
}

/// `[Content_Types].xml`'s overrides: (content type, part name without its leading slash).
fn overrides(z: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>) -> Result<Vec<(String, String)>, String> {
    let x = zip_file(z, "[Content_Types].xml")?;
    let mut r = quick_xml::Reader::from_reader(x.as_slice());
    let mut out = vec![];
    let mut buf = vec![];
    loop {
        match r.read_event_into(&mut buf).map_err(|e| e.to_string())? {
            quick_xml::events::Event::Empty(e) | quick_xml::events::Event::Start(e)
                if e.local_name().as_ref() == b"Override" =>
            {
                let (mut ct, mut part) = (String::new(), String::new());
                for a in e.attributes().flatten() {
                    let v = a.unescape_value().map(|v| v.to_string()).unwrap_or_default();
                    match a.key.local_name().as_ref() {
                        b"ContentType" => ct = v,
                        b"PartName" => part = v,
                        _ => {}
                    }
                }
                out.push((ct, part.trim_start_matches('/').to_string()));
            }
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

fn zip_file(z: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, name: &str) -> Result<Vec<u8>, String> {
    let mut f = z.by_name(name).map_err(|e| format!("error opening '{name}' from archive: {e}"))?;
    let mut v = vec![];
    f.read_to_end(&mut v).map_err(|e| e.to_string())?;
    Ok(v)
}

/// docconv's `XMLToText`: character data, a newline at each break element, skipped elements left out.
fn xml_to_text(x: &[u8], breaks: &[&str], skip: &[&str]) -> Result<String, String> {
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_reader(x);
    let mut out = String::new();
    let mut buf = vec![];
    let mut skipping = 0usize;
    loop {
        let ev = r.read_event_into(&mut buf).map_err(|e| e.to_string())?;
        match &ev {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                let empty = matches!(ev, Event::Empty(_));
                if skipping > 0 {
                    if !empty {
                        skipping += 1;
                    }
                } else {
                    let name = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                    if breaks.contains(&name.as_str()) {
                        out.push('\n');
                    }
                    if skip.contains(&name.as_str()) && !empty {
                        skipping = 1;
                    }
                }
            }
            Event::End(_) => {
                skipping = skipping.saturating_sub(1);
            }
            Event::Text(t) if skipping == 0 => {
                out += &t.decode().map(|s| s.to_string()).unwrap_or_default();
            }
            Event::GeneralRef(g) if skipping == 0 => {
                let name = String::from_utf8_lossy(g.as_ref()).to_string();
                out += match name.as_str() {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" => "'",
                    _ => "",
                };
                if let Some(n) = name.strip_prefix('#') {
                    let c = if let Some(h) = n.strip_prefix('x') {
                        u32::from_str_radix(h, 16).ok()
                    } else {
                        n.parse().ok()
                    };
                    if let Some(c) = c.and_then(char::from_u32) {
                        out.push(c);
                    }
                }
            }
            Event::CData(t) if skipping == 0 => out += &String::from_utf8_lossy(t),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// `ConvertPDFText`: `pdftotext -q -nopgbrk -enc UTF-8 -eol unix <file> -`.
fn pdftotext(d: &[u8]) -> Result<String, String> {
    let path = std::env::temp_dir().join(format!("pdflib{}", crate::app::new_id()));
    std::fs::write(&path, d).map_err(|e| e.to_string())?;
    let out = std::process::Command::new("pdftotext")
        .args(["-q", "-nopgbrk", "-enc", "UTF-8", "-eol", "unix"])
        .arg(&path)
        .arg("-")
        .output();
    let _ = std::fs::remove_file(&path);
    let out = out.map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("pdftotext: {}", out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// `archives.Identify` by name: the formats Go's extractor can open.
fn is_archive(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        ".zip", ".tar", ".tar.gz", ".tgz", ".gz", ".bz2", ".tbz2", ".xz", ".txz", ".zst", ".7z", ".rar", ".lz4", ".br",
        ".sz", ".lz",
    ]
    .iter()
    .any(|e| n.ends_with(e))
}

/// The archive extractor without recursion: each entry's path and a space. 7-zip archives give nothing.
fn archive(name: &str, d: &[u8]) -> Result<String, String> {
    let n = name.to_lowercase();
    if n.ends_with(".7z") {
        return Ok(String::new());
    }
    if n.ends_with(".zip") {
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(d)).map_err(|e| e.to_string())?;
        let names: Vec<String> = (0..z.len())
            .filter_map(|i| z.by_index(i).ok().filter(|f| !f.is_dir()).map(|f| f.name().to_string()))
            .collect();
        return Ok(entry_list(names));
    }
    let tar_bytes: Vec<u8> = if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        let mut v = vec![];
        flate2::read::GzDecoder::new(d).read_to_end(&mut v).map_err(|e| e.to_string())?;
        v
    } else if n.ends_with(".tar") {
        d.to_vec()
    } else {
        return Err("unsupported archive".into());
    };
    let mut a = tar::Archive::new(tar_bytes.as_slice());
    let mut names = vec![];
    for e in a.entries().map_err(|e| e.to_string())? {
        let e = e.map_err(|e| e.to_string())?;
        if e.header().entry_type().is_dir() {
            continue;
        }
        names.push(e.path().map_err(|e| e.to_string())?.to_string_lossy().to_string());
    }
    Ok(entry_list(names))
}

/// Entries in `fs.WalkDir`'s lexical order, at most 1000 and 1 MB of text.
fn entry_list(mut names: Vec<String>) -> String {
    names.sort();
    let mut out = String::new();
    for p in names.into_iter().take(MAX_ARCHIVE_ENTRIES) {
        if out.len() >= MAX_CONTENT {
            break;
        }
        out.push_str(&p);
        out.push(' ');
    }
    out
}

/// `plainExtractor`: the first KB must be graphic or space characters, then up to 1 MB (or the size limit) is
/// kept, without a broken last character.
fn plain(d: &[u8], max_file_size: i64) -> String {
    let mut limit = MAX_PLAIN;
    if max_file_size > 0 && (max_file_size as usize) < limit {
        limit = max_file_size as usize;
    }
    let probe = &d[..d.len().min(limit.min(1024))];
    if probe.is_empty() {
        return String::new();
    }
    let total = probe.len();
    let mut i = 0;
    loop {
        let (c, size) = decode_rune(&probe[i..]);
        if size == 0 {
            break;
        }
        if !is_graphic_or_space(c) {
            return String::new();
        }
        i += size;
        if i as isize > total as isize - 4 {
            break;
        }
    }
    let body = &d[..d.len().min(limit)];
    // truncateUTF8: invalid bytes at the end dropped.
    let mut end = body.len();
    while end > 0 && std::str::from_utf8(&body[..end]).is_err() {
        match std::str::from_utf8(&body[..end]) {
            Err(e) if e.error_len().is_none() => end = e.valid_up_to(),
            _ => break,
        }
    }
    String::from_utf8_lossy(&body[..end]).to_string()
}

/// `utf8.DecodeRune`: the char and its size, U+FFFD and 1 for an invalid byte, size 0 at the end.
pub(crate) fn decode_rune(b: &[u8]) -> (char, usize) {
    if b.is_empty() {
        return ('\u{FFFD}', 0);
    }
    for n in 1..=4.min(b.len()) {
        if let Ok(s) = std::str::from_utf8(&b[..n]) {
            return (s.chars().next().unwrap_or('\u{FFFD}'), n);
        }
    }
    ('\u{FFFD}', 1)
}

/// `unicode.In(c, GraphicRanges..., White_Space)`: letters, marks, numbers, punctuation, symbols, the space
/// separator and white space; not control or format characters.
fn is_graphic_or_space(c: char) -> bool {
    if c.is_whitespace() {
        return true;
    }
    !(c.is_control()
        || matches!(c as u32, 0xAD | 0x600..=0x605 | 0x61C | 0x6DD | 0x70F | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB | 0xE000..=0xF8FF | 0xD800..=0xDFFF))
}

impl App {
    /// `ExtractContentFromFileInfo`: images are skipped; text found is saved (1 MB at most) and the post's file
    /// infos dropped from the cache. An error is a file that could not be read or saved.
    pub async fn extract_content_from_file_info(
        &self,
        id: &str,
        name: &str,
        path: &str,
        mime: &str,
        post_id: &str,
    ) -> Result<(), String> {
        if mime.starts_with("image") {
            return Ok(());
        }
        let data =
            self.files.read(path).await.map_err(|e| format!("failed to open file for extract file content: {e}"))?;
        let max = self.cfg.i64("FileSettings.MaxFileSize");
        let name = name.to_string();
        let mut text = tokio::task::spawn_blocking(move || extract(&name, &data, max)).await.unwrap_or_default();
        if text.is_empty() {
            return Ok(());
        }
        if text.len() > MAX_CONTENT {
            let mut cut = MAX_CONTENT;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        sqlx::query("UPDATE FileInfo SET Content = $1 WHERE Id = $2")
            .bind(text.replace('\0', ""))
            .bind(id)
            .execute(&self.db)
            .await
            .map_err(|e| format!("failed to save the extracted file content: {e}"))?;
        if !post_id.is_empty() {
            self.caches.file_infos.remove(post_id);
        }
        Ok(())
    }
}

/// The `extract_content` worker: with `catchup`, files of the last 24 hours without content; otherwise those
/// created between `from` and `to` (seconds). Counts in `processed` and `errors`.
pub async fn extract_content_job(app: &State, job: &mut Job) -> Result<(), String> {
    type Row = (String, String, String, String, String, String, i64);
    let catchup = job.get("catchup") == Some("true");
    let (mut from, to) = if catchup {
        (now_ms() - 24 * 3_600_000, i64::MAX)
    } else {
        let p = |k: &str, d: i64| -> Result<i64, String> {
            match job.get(k) {
                None => Ok(d),
                Some(s) => {
                    s.parse::<i64>().map(|v| v * 1000).map_err(|e| format!("strconv.ParseInt: parsing \"{s}\": {e}"))
                }
            }
        };
        (p("from", 0)?, p("to", now_ms() / 1000)?)
    };
    let (mut files, mut errors) = (0usize, 0usize);
    loop {
        let sql = format!(
            "SELECT Id, COALESCE(Name, ''), COALESCE(Path, ''), COALESCE(MimeType, ''), COALESCE(Extension, ''), COALESCE(PostId, ''),
             CreateAt FROM FileInfo WHERE {}DeleteAt = 0{} ORDER BY CreateAt ASC, Id ASC LIMIT 1000",
            if from > 0 { "CreateAt >= $1 AND " } else { "$1 = $1 AND " },
            if catchup { " AND (Content IS NULL OR Content = '')" } else { "" }
        );
        let infos: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(from)
            .fetch_all(&app.db)
            .await
            .map_err(|e| format!("failed to find FileInfos: {e}"))?;
        let Some(last) = infos.last().map(|r| r.6) else { break };
        let n = infos.len();
        for (id, name, path, mime, ext, post_id, _) in infos {
            if IGNORED.contains(&ext.as_str()) {
                continue;
            }
            if app.extract_content_from_file_info(&id, &name, &path, &mime, &post_id).await.is_err() {
                errors += 1;
            }
            files += 1;
        }
        if catchup {
            from = last + 1;
            if n < 1000 {
                break;
            }
        } else {
            if last > to {
                break;
            }
            from = last + 1;
        }
    }
    let d = job.data_mut();
    d.insert("errors".into(), errors.to_string());
    d.insert("processed".into(), files.to_string());
    let _ = jobs::update_in_progress_data(app, job).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_and_binary() {
        assert_eq!(extract("a.txt", b"hello world", 0), "hello world");
        assert_eq!(extract("a.bin", b"\x00\x01\x02binary", 0), "");
        assert_eq!(extract("a.txt", "привет".as_bytes(), 0), "привет");
    }

    /// Tidy's output for files Go's docconv (with justext) gives empty text (a paragraph) or an error (none).
    #[test]
    fn justext_paragraphs() {
        let doc = |body: &str| {
            format!("<!DOCTYPE html>\n<html>\n<head>\n<title>t</title>\n</head>\n<body>\n{body}\n</body>\n</html>\n")
        };
        assert!(justext_has_paragraph(&doc("<p>fragment</p>")));
        assert!(justext_has_paragraph(&doc("<div>x</div>")));
        assert!(justext_has_paragraph(&doc("text<br>\n<br>\nmore")));
        assert!(justext_has_paragraph(&doc("<table><tr><td>cell</td></tr></table>")));
        assert!(!justext_has_paragraph(&doc("no tags at all, just text")));
        assert!(!justext_has_paragraph(&doc("tail text only<span>x</span>")));
        assert!(!justext_has_paragraph(&doc("<script>var x = 1;</script>")));
        assert!(!justext_has_paragraph(&format!("{}<!-- last -->", doc("<p>a</p>"))));
    }

    #[test]
    fn xml_text_with_breaks() {
        let x = br#"<w:document><w:body><w:p><w:r><w:t>Hello</w:t></w:r></w:p><w:p><w:r><w:t>A &amp; B</w:t></w:r><w:instrText>skip</w:instrText></w:p></w:body></w:document>"#;
        assert_eq!(xml_to_text(x, &["br", "p", "tab"], &["instrText", "script"]).unwrap(), "\nHello\nA & B");
    }
}
