//! File formats memvidd extracts from raw bytes (`add_document`'s `content_base64`).
//!
//! PDF and the OOXML family go through memvid-core's reader registry. Everything else
//! is handled here with crates memvid-core already depends on (calamine, zip,
//! quick-xml, encoding_rs), so memvid-core itself stays unpatched.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use calamine::{Data, Reader as _};
use encoding_rs::Encoding;
use memvid_core::{DocumentFormat, ReaderHint, ReaderRegistry};
use quick_xml::events::{BytesRef, Event};
use quick_xml::{Reader as XmlReader, XmlVersion};
use zip::ZipArchive;

/// Largest single archive entry read into memory (zip-bomb guard).
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Handled by memvid-core's reader registry.
    Native(DocumentFormat),
    /// ODS and XLSB workbooks.
    Spreadsheet,
    OpenDocumentText,
    OpenDocumentPresentation,
    Html,
    Epub,
    Rtf,
}

pub fn format_from_extension(name: &str) -> Option<Format> {
    let (_, extension) = name.rsplit_once('.')?;
    let format = match extension.to_ascii_lowercase().as_str() {
        "pdf" => Format::Native(DocumentFormat::Pdf),
        "docx" | "docm" | "dotx" | "dotm" => Format::Native(DocumentFormat::Docx),
        "xlsx" | "xlsm" | "xltx" | "xltm" => Format::Native(DocumentFormat::Xlsx),
        "xls" => Format::Native(DocumentFormat::Xls),
        "pptx" | "pptm" | "ppsx" | "ppsm" | "potx" | "potm" => Format::Native(DocumentFormat::Pptx),
        "ods" | "xlsb" => Format::Spreadsheet,
        "odt" => Format::OpenDocumentText,
        "odp" => Format::OpenDocumentPresentation,
        "html" | "htm" | "xhtml" => Format::Html,
        "epub" => Format::Epub,
        "rtf" => Format::Rtf,
        _ => return None,
    };
    Some(format)
}

/// Extract plain text from raw file bytes. Returns a `BAD_PARAMS`-coded error (not a
/// crash) when there is no text at all - e.g. a scanned/image-only PDF - matching the
/// create-memory-side skip-reason convention on the Node side.
pub fn extract(
    bytes: &[u8],
    format: Format,
    source: &str,
) -> Result<String, (String, &'static str)> {
    let text = match format {
        Format::Native(native) => extract_native(bytes, native),
        Format::Spreadsheet => extract_spreadsheet(bytes),
        Format::OpenDocumentText => {
            read_zip_entry(bytes, "content.xml").map(|xml| odf_text(&xml, false))
        }
        Format::OpenDocumentPresentation => {
            read_zip_entry(bytes, "content.xml").map(|xml| odf_text(&xml, true))
        }
        Format::Html => Ok(html_to_text(&decode_html(bytes))),
        Format::Epub => extract_epub(bytes),
        Format::Rtf => Ok(rtf_to_text(bytes)),
    }
    .map_err(|e| {
        (
            format!("extraction of \"{source}\" failed: {e}"),
            "INTERNAL",
        )
    })?;

    if text.trim().is_empty() {
        return Err((
            format!("no extractable text in \"{source}\" (scanned/image-only, or genuinely empty)"),
            "BAD_PARAMS",
        ));
    }
    Ok(text)
}

/// The built-in reader set (PDF, DOCX, XLSX, XLS, PPTX, plus a plain-text fallback),
/// all unconditionally compiled into memvid-core.
fn extract_native(bytes: &[u8], format: DocumentFormat) -> Result<String, String> {
    static REGISTRY: std::sync::OnceLock<ReaderRegistry> = std::sync::OnceLock::new();
    let hint = ReaderHint::new(None, Some(format));
    let reader = REGISTRY
        .get_or_init(ReaderRegistry::default)
        .find_reader(&hint)
        .ok_or_else(|| "no native reader registered".to_string())?;
    let output = reader.extract(bytes, &hint).map_err(|e| e.to_string())?;
    Ok(output.document.text.unwrap_or_default())
}

/// ODS / XLSB workbooks, rendered like memvid-core's `XlsReader`: a `Sheet: <name>`
/// line, then one tab-separated line per row.
fn extract_spreadsheet(bytes: &[u8]) -> Result<String, String> {
    let mut workbook =
        calamine::open_workbook_auto_from_rs(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut out = String::new();
    for name in workbook.sheet_names() {
        let Ok(range) = workbook.worksheet_range(&name) else {
            continue;
        };
        out.push_str(&format!("Sheet: {name}\n"));
        for row in range.rows() {
            out.push_str(&row.iter().map(cell_text).collect::<Vec<_>>().join("\t"));
            out.push('\n');
        }
        out.push('\n');
    }
    Ok(out.trim().to_string())
}

fn cell_text(cell: &Data) -> String {
    match cell {
        Data::String(s) => s.trim().to_string(),
        Data::Error(e) => format!("#{e:?}"),
        other => other.to_string(),
    }
}

fn read_zip_entry(bytes: &[u8], name: &str) -> Result<String, String> {
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("not a valid archive: {e}"))?;
    Ok(String::from_utf8_lossy(&read_entry(&mut archive, name)?).into_owned())
}

fn read_entry(archive: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> Result<Vec<u8>, String> {
    let entry = archive.by_name(name).map_err(|e| format!("{name}: {e}"))?;
    let mut bytes = Vec::new();
    entry
        .take(MAX_ENTRY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{name}: {e}"))?;
    if bytes.len() as u64 > MAX_ENTRY_BYTES {
        return Err(format!("{name} is larger than {MAX_ENTRY_BYTES} bytes"));
    }
    Ok(bytes)
}

/// Text of an OpenDocument `content.xml`: one line per `text:p` / `text:h`, and for
/// presentations a `Slide N:` header per `draw:page` - the shape memvid-core's PPTX
/// reader produces.
fn odf_text(xml: &str, presentation: bool) -> String {
    let mut reader = XmlReader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = String::new();
    let mut line = String::new();
    // Paragraph nesting: notes and text boxes can sit inside a paragraph.
    let mut depth = 0usize;
    let mut slide = 0;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"text:p" | b"text:h" => depth += 1,
                b"draw:page" if presentation => {
                    flush_line(&mut line, &mut out);
                    slide += 1;
                    out.push_str(&format!("\nSlide {slide}:\n"));
                }
                _ => {}
            },
            Ok(Event::Empty(e)) if depth > 0 => match e.name().as_ref() {
                b"text:tab" => line.push('\t'),
                b"text:line-break" => line.push('\n'),
                b"text:s" => line.push(' '),
                _ => {}
            },
            Ok(Event::End(e)) if matches!(e.name().as_ref(), b"text:p" | b"text:h") => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    flush_line(&mut line, &mut out);
                }
            }
            Ok(Event::Text(t)) if depth > 0 => {
                if let Ok(text) = t.decode() {
                    line.push_str(&text);
                }
            }
            Ok(Event::GeneralRef(r)) if depth > 0 => push_xml_entity(&mut line, &r),
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    flush_line(&mut line, &mut out);
    out.trim().to_string()
}

fn flush_line(line: &mut String, out: &mut String) {
    let text = line.trim();
    if !text.is_empty() {
        out.push_str(text);
        out.push('\n');
    }
    line.clear();
}

fn push_xml_entity(out: &mut String, r: &BytesRef<'_>) {
    if let Ok(Some(ch)) = r.resolve_char_ref() {
        out.push(ch);
    } else if let Ok(name) = r.decode()
        && let Some(value) = quick_xml::escape::resolve_predefined_entity(&name)
    {
        out.push_str(value);
    }
}

/// Decode HTML bytes: a BOM wins, then a declared `charset` / XML `encoding`, then UTF-8
/// when the bytes are valid UTF-8. Anything else is taken as windows-1250, the same
/// fallback ragtime uses for plain-text uploads.
fn decode_html(bytes: &[u8]) -> String {
    if let Some((encoding, bom_length)) = Encoding::for_bom(bytes) {
        return encoding
            .decode_without_bom_handling(&bytes[bom_length..])
            .0
            .into_owned();
    }
    let encoding = declared_charset(bytes)
        .or_else(|| {
            std::str::from_utf8(bytes)
                .is_ok()
                .then_some(encoding_rs::UTF_8)
        })
        .unwrap_or(encoding_rs::WINDOWS_1250);
    encoding.decode_without_bom_handling(bytes).0.into_owned()
}

fn declared_charset(bytes: &[u8]) -> Option<&'static Encoding> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).to_ascii_lowercase();
    ["charset=", "encoding="].iter().find_map(|key| {
        let start = head.find(key)? + key.len();
        let label: String = head[start..]
            .trim_start_matches(['"', '\'', ' '])
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
            .collect();
        Encoding::for_label(label.as_bytes())
    })
}

/// Elements whose content is never readable text.
const SKIPPED_ELEMENTS: &[&str] = &["script", "style", "noscript", "template", "svg"];

/// Elements that start a new line.
const BLOCK_ELEMENTS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "caption",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "legend",
    "li",
    "main",
    "nav",
    "ol",
    "option",
    "p",
    "section",
    "summary",
    "table",
    "tbody",
    "tfoot",
    "thead",
    "title",
    "tr",
    "ul",
];

/// Readable text of an HTML document: tags dropped, `script`/`style`/... contents skipped,
/// block elements on their own lines, table cells tab-separated, entities decoded.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    let mut preformatted = 0usize;

    while let Some(lt) = rest.find('<') {
        push_html_text(&mut out, &rest[..lt], preformatted > 0);
        rest = &rest[lt..];

        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |end| &after[end + 3..]);
            continue;
        }
        // `<` not followed by a tag name is just text ("a < b").
        if !rest[1..].starts_with(|c: char| c.is_ascii_alphabetic() || matches!(c, '/' | '!' | '?'))
        {
            push_html_text(&mut out, "<", preformatted > 0);
            rest = &rest[1..];
            continue;
        }
        let Some(end) = tag_end(rest) else {
            break;
        };
        let tag = &rest[1..end];
        rest = &rest[end + 1..];

        let closing = tag.starts_with('/');
        let name = tag
            .trim_start_matches('/')
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();
        if !closing && SKIPPED_ELEMENTS.contains(&name.as_str()) {
            rest = skip_element(rest, &name);
            continue;
        }
        match name.as_str() {
            "pre" => {
                preformatted = if closing {
                    preformatted.saturating_sub(1)
                } else {
                    preformatted + 1
                };
                start_line(&mut out);
            }
            "br" => out.push('\n'),
            "td" | "th" if !closing => out.push('\t'),
            other if BLOCK_ELEMENTS.contains(&other) => start_line(&mut out),
            _ => {}
        }
    }
    push_html_text(&mut out, rest, preformatted > 0);
    tidy_lines(&out)
}

/// End the current line unless it already ended, so `</p><p>` gives one line break,
/// not an empty line.
fn start_line(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Byte offset of the `>` closing the tag that starts `s`, ignoring `>` inside quoted
/// attribute values.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (index, c) in s.char_indices().skip(1) {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), _) if c == open => quote = None,
            (None, '>') => return Some(index),
            _ => {}
        }
    }
    None
}

/// The rest of the document after the end tag of `name` (case-insensitive).
fn skip_element<'a>(rest: &'a str, name: &str) -> &'a str {
    let lower = rest.to_ascii_lowercase();
    match lower.find(&format!("</{name}")) {
        Some(start) => rest[start..]
            .find('>')
            .map_or("", |end| &rest[start + end + 1..]),
        None => "",
    }
}

fn push_html_text(out: &mut String, raw: &str, preformatted: bool) {
    let decoded = decode_html_entities(raw);
    if preformatted {
        out.push_str(&decoded);
        return;
    }
    let mut last_was_space = out.ends_with(char::is_whitespace);
    for c in decoded.chars() {
        if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
}

/// Decode `&name;`, `&#123;` and `&#x7b;` references; anything unrecognised stays as is.
fn decode_html_entities(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let resolved = rest[1..]
            .find(';')
            .filter(|&semi| semi <= 32)
            .and_then(|semi| {
                let name = &rest[1..=semi];
                let value = match name.strip_prefix('#') {
                    Some(number) => {
                        let code = match number.strip_prefix(['x', 'X']) {
                            Some(hex) => u32::from_str_radix(hex, 16).ok(),
                            None => number.parse().ok(),
                        };
                        code.and_then(char::from_u32).map(String::from)
                    }
                    None => quick_xml::escape::resolve_html5_entity(name).map(String::from),
                };
                value.map(|value| (value, semi + 2))
            });
        match resolved {
            Some((value, consumed)) => {
                out.push_str(&value);
                rest = &rest[consumed..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Trim every line and collapse runs of blank lines into one.
fn tidy_lines(text: &str) -> String {
    let mut out = String::new();
    let mut blank = false;
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            blank = !out.is_empty();
            continue;
        }
        if blank {
            out.push('\n');
            blank = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// An EPUB's chapters in reading order (`container.xml` -> OPF -> spine), each one
/// through `html_to_text`.
fn extract_epub(bytes: &[u8]) -> Result<String, String> {
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("not a valid archive: {e}"))?;
    let container =
        String::from_utf8_lossy(&read_entry(&mut archive, "META-INF/container.xml")?).into_owned();
    let opf_path = first_attribute(&container, b"rootfile", b"full-path")
        .ok_or_else(|| "container.xml names no rootfile".to_string())?;
    let opf = String::from_utf8_lossy(&read_entry(&mut archive, &opf_path)?).into_owned();
    let base = opf_path.rsplit_once('/').map_or("", |(dir, _)| dir);

    let mut out = String::new();
    for href in spine_documents(&opf) {
        let path = join_path(base, &percent_decode(href.split('#').next().unwrap_or("")));
        // A chapter the manifest names but the archive lacks is skipped, not fatal.
        let Ok(html) = read_entry(&mut archive, &path) else {
            continue;
        };
        let text = html_to_text(&decode_html(&html));
        if !text.is_empty() {
            out.push_str(&text);
            out.push_str("\n\n");
        }
    }
    Ok(out.trim_end().to_string())
}

/// Value of `attribute` on the first `element` in `xml` (namespace prefixes ignored).
fn first_attribute(xml: &str, element: &[u8], attribute: &[u8]) -> Option<String> {
    let mut reader = XmlReader::from_str(xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == element => {
                return e
                    .attributes()
                    .flatten()
                    .find(|attr| attr.key.local_name().as_ref() == attribute)
                    .and_then(|attr| attr.normalized_value(XmlVersion::Implicit1_0).ok())
                    .map(|value| value.into_owned());
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

/// `href`s of the (X)HTML spine items of an OPF package document, in reading order.
fn spine_documents(opf: &str) -> Vec<String> {
    let mut reader = XmlReader::from_str(opf);
    let mut buf = Vec::new();
    let mut manifest: HashMap<String, (String, String)> = HashMap::new();
    let mut spine = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                let attribute = |name: &[u8]| {
                    e.attributes()
                        .flatten()
                        .find(|attr| attr.key.local_name().as_ref() == name)
                        .and_then(|attr| attr.normalized_value(XmlVersion::Implicit1_0).ok())
                        .map(|value| value.into_owned())
                };
                match e.local_name().as_ref() {
                    b"item" => {
                        if let (Some(id), Some(href)) = (attribute(b"id"), attribute(b"href")) {
                            manifest
                                .insert(id, (href, attribute(b"media-type").unwrap_or_default()));
                        }
                    }
                    b"itemref" => spine.extend(attribute(b"idref")),
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    spine
        .iter()
        .filter_map(|idref| manifest.get(idref))
        .filter(|(_, media_type)| media_type.contains("html"))
        .map(|(href, _)| href.clone())
        .collect()
}

/// Resolve `href` (relative, possibly with `..`) against the archive directory `base`.
fn join_path(base: &str, href: &str) -> String {
    let mut parts: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
    for segment in href.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes
            .get(index + 1..index + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[index], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                index += 3;
            }
            (byte, _) => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// RTF destinations whose content is never document text.
const RTF_SKIPPED_DESTINATIONS: &[&str] = &[
    "bkmkend",
    "bkmkstart",
    "colorschememapping",
    "colortbl",
    "datastore",
    "fldinst",
    "filetbl",
    "fonttbl",
    "generator",
    "info",
    "latentstyles",
    "listoverridetable",
    "listtable",
    "mmathPr",
    "nonshppict",
    "object",
    "objdata",
    "pgdsctbl",
    "pict",
    "revtbl",
    "rsidtbl",
    "stylesheet",
    "themedata",
    "xmlnstbl",
];

/// Plain text of an RTF document: groups and skipped destinations, `\par`/`\line`/`\tab`,
/// `\'hh` bytes in the document's ANSI code page (`\ansicpgN`), and `\uN` with its
/// `\ucN` fallback characters skipped.
fn rtf_to_text(rtf: &[u8]) -> String {
    #[derive(Clone, Copy)]
    struct Group {
        skip: bool,
        fallback_chars: usize,
    }

    let mut stack = Vec::new();
    let mut group = Group {
        skip: false,
        fallback_chars: 1,
    };
    let mut encoding = encoding_rs::WINDOWS_1252;
    let mut out = String::new();
    // `\'hh` bytes (and stray 8-bit bytes) waiting to be decoded together, so double-byte
    // code pages decode correctly.
    let mut pending: Vec<u8> = Vec::new();
    let mut to_skip = 0usize;
    let mut group_start = false;
    let mut index = 0;

    let flush = |pending: &mut Vec<u8>, out: &mut String, encoding: &'static Encoding| {
        if !pending.is_empty() {
            out.push_str(&encoding.decode_without_bom_handling(pending).0);
            pending.clear();
        }
    };

    while index < rtf.len() {
        let byte = rtf[index];
        index += 1;
        match byte {
            b'{' => {
                flush(&mut pending, &mut out, encoding);
                stack.push(group);
                group_start = true;
            }
            b'}' => {
                flush(&mut pending, &mut out, encoding);
                group = stack.pop().unwrap_or(group);
                group_start = false;
            }
            b'\r' | b'\n' => {}
            b'\\' => {
                let Some(&next) = rtf.get(index) else {
                    break;
                };
                if next.is_ascii_alphabetic() {
                    let start = index;
                    while rtf.get(index).is_some_and(u8::is_ascii_alphabetic) {
                        index += 1;
                    }
                    let word = std::str::from_utf8(&rtf[start..index]).unwrap_or("");
                    let number_start = index;
                    if rtf.get(index) == Some(&b'-') {
                        index += 1;
                    }
                    while rtf.get(index).is_some_and(u8::is_ascii_digit) {
                        index += 1;
                    }
                    let param: Option<i32> = std::str::from_utf8(&rtf[number_start..index])
                        .ok()
                        .and_then(|digits| digits.parse().ok());
                    if rtf.get(index) == Some(&b' ') {
                        index += 1;
                    }

                    if group_start && RTF_SKIPPED_DESTINATIONS.contains(&word) {
                        group.skip = true;
                    }
                    group_start = false;
                    if group.skip {
                        continue;
                    }
                    match word {
                        "par" | "line" | "sect" | "page" | "row" => {
                            flush(&mut pending, &mut out, encoding);
                            out.push('\n');
                        }
                        "tab" | "cell" => {
                            flush(&mut pending, &mut out, encoding);
                            out.push('\t');
                        }
                        "ansicpg" => {
                            if let Some(page) = param.and_then(ansi_code_page) {
                                encoding = page;
                            }
                        }
                        "uc" => group.fallback_chars = param.unwrap_or(1).max(0) as usize,
                        "u" => {
                            if let Some(code) = param {
                                flush(&mut pending, &mut out, encoding);
                                let code = if code < 0 { code + 65536 } else { code };
                                out.extend(u32::try_from(code).ok().and_then(char::from_u32));
                                to_skip = group.fallback_chars;
                            }
                        }
                        "emdash" => out.push('\u{2014}'),
                        "endash" => out.push('\u{2013}'),
                        "bullet" => out.push('\u{2022}'),
                        "lquote" => out.push('\u{2018}'),
                        "rquote" => out.push('\u{2019}'),
                        "ldblquote" => out.push('\u{201c}'),
                        "rdblquote" => out.push('\u{201d}'),
                        _ => {}
                    }
                    continue;
                }

                index += 1;
                match next {
                    b'*' => group.skip = true,
                    b'\'' => {
                        let hex = rtf
                            .get(index..index + 2)
                            .and_then(|hex| std::str::from_utf8(hex).ok())
                            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                        if let Some(value) = hex {
                            index += 2;
                            match (group.skip, to_skip) {
                                (true, _) => {}
                                (false, 0) => pending.push(value),
                                (false, _) => to_skip -= 1,
                            }
                        }
                    }
                    _ if group.skip => {}
                    b'~' => out.push('\u{a0}'),
                    b'_' => out.push('-'),
                    b'\\' | b'{' | b'}' => {
                        flush(&mut pending, &mut out, encoding);
                        out.push(char::from(next));
                    }
                    b'\r' | b'\n' => {
                        flush(&mut pending, &mut out, encoding);
                        out.push('\n');
                    }
                    _ => {}
                }
                group_start = false;
            }
            _ => {
                group_start = false;
                if group.skip {
                    continue;
                }
                if to_skip > 0 {
                    to_skip -= 1;
                    continue;
                }
                if byte.is_ascii() {
                    flush(&mut pending, &mut out, encoding);
                    out.push(char::from(byte));
                } else {
                    pending.push(byte);
                }
            }
        }
    }
    flush(&mut pending, &mut out, encoding);
    tidy_lines(&out)
}

fn ansi_code_page(page: i32) -> Option<&'static Encoding> {
    match page {
        932 => Some(encoding_rs::SHIFT_JIS),
        936 => Some(encoding_rs::GBK),
        949 => Some(encoding_rs::EUC_KR),
        950 => Some(encoding_rs::BIG5),
        _ => Encoding::for_label(format!("windows-{page}").as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::write::{SimpleFileOptions, ZipWriter};

    use super::*;

    fn zip_of(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(content.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn text_of(bytes: &[u8], file: &str) -> String {
        extract(bytes, format_from_extension(file).unwrap(), file).unwrap()
    }

    #[test]
    fn office_variants_use_the_core_readers() {
        assert_eq!(
            format_from_extension("a.DOCM"),
            Some(Format::Native(DocumentFormat::Docx))
        );
        assert_eq!(
            format_from_extension("a.xltx"),
            Some(Format::Native(DocumentFormat::Xlsx))
        );
        assert_eq!(
            format_from_extension("a.ppsx"),
            Some(Format::Native(DocumentFormat::Pptx))
        );
        assert_eq!(format_from_extension("no-extension"), None);
        assert_eq!(format_from_extension("notes.txt"), None);
    }

    #[test]
    fn docm_and_pptm_bytes_extract_like_docx_and_pptx() {
        let docm = zip_of(&[(
            "word/document.xml",
            r"<w:document><w:body><w:p><w:r><w:t>Macro &amp; template text</w:t></w:r></w:p></w:body></w:document>",
        )]);
        let pptm = zip_of(&[(
            "ppt/slides/slide1.xml",
            r"<p:sld><a:p><a:r><a:t>First slide</a:t></a:r></a:p></p:sld>",
        )]);

        assert_eq!(text_of(&docm, "a.docm"), "Macro & template text");
        assert_eq!(text_of(&pptm, "a.pptm"), "Slide 1:\nFirst slide");
    }

    #[test]
    fn xlsm_bytes_extract_like_xlsx() {
        let xlsm = zip_of(&[
            (
                "[Content_Types].xml",
                r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.ms-excel.sheet.macroEnabled.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
            ),
            (
                "_rels/.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Budget" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Revenue</t></is></c><c r="B1"><v>1030</v></c></row></sheetData></worksheet>"#,
            ),
        ]);

        let text = text_of(&xlsm, "a.xlsm");

        assert!(text.contains("Revenue"), "{text}");
        assert!(text.contains("1030"), "{text}");
    }

    #[test]
    fn odt_paragraphs_headings_tabs_and_entities() {
        let odt = zip_of(&[(
            "content.xml",
            r#"<office:document-content><office:body><office:text><text:h>Kapitola &amp; úvod</text:h><text:p>Cena<text:tab/>250 Kč</text:p><text:p>caf&#233;<text:s/>ok</text:p></office:text></office:body></office:document-content>"#,
        )]);

        assert_eq!(
            text_of(&odt, "a.odt"),
            "Kapitola & úvod\nCena\t250 Kč\ncafé ok"
        );
    }

    #[test]
    fn odp_pages_become_numbered_slides() {
        let odp = zip_of(&[(
            "content.xml",
            r#"<office:document-content><office:body><office:presentation><draw:page><draw:frame><draw:text-box><text:p>Úvod</text:p></draw:text-box></draw:frame></draw:page><draw:page><draw:frame><draw:text-box><text:p>Závěr</text:p></draw:text-box></draw:frame></draw:page></office:presentation></office:body></office:document-content>"#,
        )]);

        assert_eq!(text_of(&odp, "a.odp"), "Slide 1:\nÚvod\n\nSlide 2:\nZávěr");
    }

    #[test]
    fn ods_sheets_render_like_the_xls_reader() {
        let ods = zip_of(&[
            ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
            (
                "META-INF/manifest.xml",
                r#"<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>"#,
            ),
            (
                "content.xml",
                r#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.2"><office:body><office:spreadsheet><table:table table:name="Příjmy"><table:table-row><table:table-cell office:value-type="string"><text:p>Daně</text:p></table:table-cell><table:table-cell office:value-type="float" office:value="1030.7"><text:p>1030.7</text:p></table:table-cell></table:table-row></table:table></office:spreadsheet></office:body></office:document-content>"#,
            ),
        ]);

        assert_eq!(text_of(&ods, "a.ods"), "Sheet: Příjmy\nDaně\t1030.7");
    }

    #[test]
    fn html_drops_markup_scripts_and_styles() {
        let html = r#"<!DOCTYPE html><html><head><title>Ceník</title><style>p{color:red}</style>
            <script>if (a < b) { alert("x") }</script></head><body><!-- note -->
            <h1>Jízdné&nbsp;2025</h1><p>Dospělí &amp; děti:   <b>250</b> Kč</p>
            <table><tr><th>Trasa</th><th>Cena</th></tr><tr><td>Praha</td><td>&#x32;50</td></tr></table>
            <p>3 < 5</p></body></html>"#;

        assert_eq!(
            html_to_text(html),
            "Ceník\nJízdné 2025\nDospělí & děti: 250 Kč\nTrasa\tCena\nPraha\t250\n3 < 5"
        );
    }

    #[test]
    fn html_charset_declaration_picks_the_decoder() {
        let mut bytes = br#"<html><head><meta charset="windows-1250"></head><body>"#.to_vec();
        bytes.extend_from_slice(&[0x9e, 0x6c, 0x75, 0x9d, 0x6f, 0x75, 0xe8, 0x6b, 0xfd]);
        bytes.extend_from_slice(b"</body></html>");

        assert_eq!(text_of(&bytes, "a.html"), "žluťoučký");
    }

    #[test]
    fn epub_chapters_follow_the_spine_order() {
        let epub = zip_of(&[
            ("mimetype", "application/epub+zip"),
            (
                "META-INF/container.xml",
                r#"<container><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
            ),
            (
                "OEBPS/content.opf",
                r#"<package><manifest><item id="a" href="text/one.xhtml" media-type="application/xhtml+xml"/><item id="b" href="text/two%20b.xhtml" media-type="application/xhtml+xml"/><item id="css" href="style.css" media-type="text/css"/></manifest><spine><itemref idref="b"/><itemref idref="a"/></spine></package>"#,
            ),
            (
                "OEBPS/text/one.xhtml",
                "<html><body><p>Druhá kapitola</p></body></html>",
            ),
            (
                "OEBPS/text/two b.xhtml",
                "<html><body><h1>První kapitola</h1></body></html>",
            ),
        ]);

        assert_eq!(text_of(&epub, "a.epub"), "První kapitola\n\nDruhá kapitola");
    }

    #[test]
    fn rtf_code_page_unicode_and_skipped_destinations() {
        let rtf = br"{\rtf1\ansi\ansicpg1250\deff0{\fonttbl{\f0 Calibri;}}{\colortbl;\red0\green0\blue0;}{\*\generator Writer;}
{\info{\title Secret title}}\pard \'9elu\'9dou\'e8k\'fd k\'f9\'f2\par
Euro \u8364?\tab konec\par
{\field{\*\fldinst HYPERLINK x}{\fldrslt odkaz}}}";

        assert_eq!(text_of(rtf, "a.rtf"), "žluťoučký kůň\nEuro €\tkonec\nodkaz");
    }

    #[test]
    fn files_without_text_are_rejected_with_bad_params() {
        let empty = zip_of(&[("content.xml", "<office:document-content/>")]);

        let err = extract(&empty, Format::OpenDocumentText, "empty.odt").unwrap_err();

        assert_eq!(err.1, "BAD_PARAMS");
    }
}
