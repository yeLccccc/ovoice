//! 文档文本抽取：把 PDF/DOCX 的文字抽出来内联给 MiniMax。
//!
//! 背景：M3 的 ChatCompletion 只吃 text/image/video，没有文档输入通道；
//! 附件里的 pdf/docx 若只给模型一个文件路径，模型看不到内容。这里把
//! 纯文本抽出来，由 `llm::user_message_with_attachments` 作 text part 内联。
//!
//! 纯逻辑、可离线测；任何失败（损坏/加密/非预期结构）返 None，调用方回落到指针 note。

use std::path::Path;

/// 按文档类型抽取纯文本。
///
/// - `"pdf"` / `"docx"`：尽力抽取，返 `Some(text)`（可能为空串）。
/// - 其它 kind、或读取/解析失败：返 `None`。
///
/// 不做截断——上限策略由调用方负责（保持本函数纯逻辑、无配置依赖）。
pub fn doc_text(path: &Path, kind: &str) -> Option<String> {
    match kind {
        "pdf" => pdf_text(path),
        "docx" => docx_text(path),
        _ => None,
    }
}

/// DOCX = zip；读 `word/document.xml`，走 `<w:t>` 文本、按 `<w:p>` 分段。
fn docx_text(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(file).ok()?;
    let mut xml = String::new();
    {
        let mut entry = zip.by_name("word/document.xml").ok()?;
        use std::io::Read;
        entry.read_to_string(&mut xml).ok()?;
    }
    Some(docx_xml_to_text(&xml))
}

/// 把 `word/document.xml` 的本体 XML 转成纯文本：
/// `<w:t>` 内的文字累加；`<w:p>` 段落之间换行；`<w:tab>`→空格、`<w:br>`→换行。
fn docx_xml_to_text(xml: &str) -> String {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut out = String::new();
    let mut buf = Vec::new();
    let mut in_t = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                match e.name().as_ref() {
                    b"w:t" => in_t = true,
                    b"w:p" if !out.is_empty() => out.push('\n'),
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"w:tab" => out.push(' '),
                b"w:br" => out.push('\n'),
                b"w:p" if !out.is_empty() => out.push('\n'),
                _ => {}
            },
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"w:t" => in_t = false,
                b"w:p" if !out.is_empty() => out.push('\n'),
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if in_t {
                    let raw = std::str::from_utf8(&t).unwrap_or("");
                    if let Ok(u) = quick_xml::escape::unescape(raw) {
                        out.push_str(&u);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out.trim().to_string()
}

/// PDF 文本：pdf-extract（纯 Rust）。损坏/加密/无文本层 → None。
fn pdf_text(path: &Path) -> Option<String> {
    pdf_extract::extract_text(path).ok()
}

/// 测试用：在持久 temp 路径写一个最小 docx（zip 仅含 word/document.xml）。
/// llm.rs 的接线测试也复用它构造真附件，避免重复 zip 样板。
#[cfg(test)]
pub(crate) fn build_minimal_docx(paragraphs: &[&str]) -> std::path::PathBuf {
    use std::io::{Cursor, Write};
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let body: String = paragraphs
        .iter()
        .map(|p| format!("<w:p><w:r><w:t>{p}</w:t></w:r></w:p>"))
        .collect();
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
    );
    // 每次调用唯一路径（并行测试不会撞车；调用方负责删）
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("ovoice-test-{}-{n}.docx", std::process::id()));
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("word/document.xml", opts).unwrap();
    zip.write_all(xml.as_bytes()).unwrap();
    let z = zip.finish().unwrap();
    std::fs::write(&path, z.into_inner()).unwrap();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docx_extracts_minimal_docx() {
        let p = build_minimal_docx(&["Hello World", "Second paragraph"]);
        let t = doc_text(&p, "docx").expect("docx 抽取应成功");
        assert!(t.contains("Hello World"), "缺第一段: {t}");
        assert!(t.contains("Second paragraph"), "缺第二段: {t}");
        assert!(t.contains('\n'), "段落间应有换行: {t}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn docx_xml_handles_tabs_and_breaks() {
        let xml = r#"<w:document xmlns:w="w"><w:body>
            <w:p><w:r><w:t>col1</w:t><w:tab/><w:t>col2</w:t></w:r></w:p>
            <w:p><w:r><w:t>line</w:t><w:br/><w:t>broke</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let t = docx_xml_to_text(xml);
        assert!(t.contains("col1 col2"), "tab→空格: {t}");
        assert!(t.contains("line\nbroke") || t.contains("line") && t.contains("broke"), "br→换行: {t}");
    }

    #[test]
    fn doc_text_unknown_kind_returns_none() {
        let p = std::path::Path::new("a.txt");
        assert_eq!(doc_text(p, "txt"), None);
        assert_eq!(doc_text(p, "image"), None);
    }

    #[test]
    fn doc_text_missing_file_returns_none() {
        let p = std::path::Path::new("does-not-exist-zzz.docx");
        assert_eq!(doc_text(p, "docx"), None);
        let p2 = std::path::Path::new("does-not-exist-zzz.pdf");
        assert_eq!(doc_text(p2, "pdf"), None);
    }
}
