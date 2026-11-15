//! Scripts stored in an xlsx file.
//!
//! They live in a Custom XML data-store item, which Excel and LibreOffice
//! keep but otherwise ignore:
//!
//! ```text
//! customXml/item1.xml               <xlScripts xmlns="urn:x-xl:scripts:1" version="1">
//!                                     <script name="Module1" lang="lua">…source…</script>
//!                                   </xlScripts>
//! customXml/itemProps1.xml          <ds:datastoreItem ds:itemID="{…}"> (schema reference)
//! customXml/_rels/item1.xml.rels    item → itemProps (customXmlProps relationship)
//! xl/_rels/workbook.xml.rels        workbook → ../customXml/item1.xml (customXml relationship)
//! [Content_Types].xml               Overrides for both parts
//! ```
//!
//! Sources are plain escaped text (carriage returns as `&#13;` so they come
//! back exactly; control characters XML can't hold become U+FFFD).

use std::fs::File;
use std::io::{BufWriter, Cursor, Read, Seek, Write};
use std::path::Path;

use quick_xml::XmlVersion;
use quick_xml::events::Event;
use xl_engine::ScriptModule;
use zip::ZipArchive;

use crate::xlsx_read::{attr, local, push_entity, read_entry, xml};

pub(crate) const NAMESPACE: &str = "urn:x-xl:scripts:1";
pub(crate) const REL_CUSTOM_XML: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml";
const REL_CUSTOM_XML_PROPS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXmlProps";
pub(crate) const CT_CUSTOM_XML_PROPS: &str = "application/vnd.openxmlformats-officedocument.customXmlProperties+xml";
pub(crate) const CT_XML: &str = "application/xml";
/// Fixed so files written twice are identical.
const ITEM_ID: &str = "{6F1C3B2A-58D4-4E4B-9C1E-786C5C726970}";

/// The scripts found in a package, and the parts holding them.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ScriptsPart {
    /// "customXml/item1.xml".
    pub item: String,
    /// The item's properties and relationships parts, if present.
    pub related: Vec<String>,
    pub scripts: Vec<ScriptModule>,
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            '\t' | '\n' => out.push(c),
            c if (c as u32) < 0x20 || matches!(c, '\u{FFFE}' | '\u{FFFF}') => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

/// The data-store item for these scripts.
pub(crate) fn scripts_xml(scripts: &[ScriptModule]) -> String {
    let mut out = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<xlScripts xmlns=\"{NAMESPACE}\" version=\"1\">");
    for s in scripts {
        out.push_str(&format!("<script name=\"{}\" lang=\"lua\">{}</script>", escape(&s.name), escape(&s.source)));
    }
    out.push_str("</xlScripts>");
    out
}

fn item_props_xml() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"no\"?>\n<ds:datastoreItem ds:itemID=\"{ITEM_ID}\" xmlns:ds=\"http://schemas.openxmlformats.org/officeDocument/2006/customXml\"><ds:schemaRefs><ds:schemaRef ds:uri=\"{NAMESPACE}\"/></ds:schemaRefs></ds:datastoreItem>"
    )
}

fn item_rels_xml(props_target: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL_CUSTOM_XML_PROPS}\" Target=\"{props_target}\"/></Relationships>"
    )
}

/// Scripts in a data-store item, or None if the item isn't ours.
pub(crate) fn parse_scripts(bytes: &[u8]) -> Option<Vec<ScriptModule>> {
    let mut r = xml(bytes);
    let mut ours = false;
    let mut out = Vec::new();
    let mut cur: Option<ScriptModule> = None;
    loop {
        match r.read_event().ok()? {
            Event::Start(ref e) | Event::Empty(ref e) if !ours => {
                // The first element is the root: ours, or not.
                if local(e.name().as_ref()) != "xlScripts" || attr(e, "xmlns").as_deref() != Some(NAMESPACE) {
                    return None;
                }
                ours = true;
            }
            Event::Start(ref e) if local(e.name().as_ref()) == "script" => {
                cur = Some(ScriptModule { name: attr(e, "name").unwrap_or_default(), source: String::new() });
            }
            Event::Empty(ref e) if local(e.name().as_ref()) == "script" => {
                out.push(ScriptModule { name: attr(e, "name").unwrap_or_default(), source: String::new() });
            }
            Event::Text(t) => {
                if let Some(c) = cur.as_mut() {
                    c.source.push_str(&t.xml_content(XmlVersion::Implicit1_0));
                }
            }
            Event::GeneralRef(g) => {
                if let Some(c) = cur.as_mut() {
                    push_entity(&mut c.source, &g);
                }
            }
            Event::CData(t) => {
                if let Some(c) = cur.as_mut() {
                    c.source.push_str(&t.into_inner());
                }
            }
            Event::End(ref e) if local(e.name().as_ref()) == "script" => out.extend(cur.take()),
            Event::Eof => break,
            _ => {}
        }
    }
    ours.then_some(out)
}

/// (type, target) of every relationship in a .rels part.
fn rels_with_types(bytes: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut r = xml(bytes);
    while let Ok(ev) = r.read_event() {
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) if local(e.name().as_ref()) == "Relationship" => {
                if let (Some(ty), Some(target)) = (attr(e, "Type"), attr(e, "Target")) {
                    out.push((ty, target));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    out
}

/// Resolve a relationship target against the directory of its source part.
fn resolve(base_dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut parts: Vec<&str> = base_dir.split('/').filter(|p| !p.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// The item's relationships part: "customXml/_rels/item1.xml.rels".
fn rels_path(item: &str) -> String {
    match item.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{item}.rels"),
    }
}

/// Find xl's scripts in a package (the workbook's Custom XML items).
pub(crate) fn find_scripts<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Option<ScriptsPart> {
    let rels = read_entry(zip, "xl/_rels/workbook.xml.rels").ok()??;
    for (ty, target) in rels_with_types(&rels) {
        if ty != REL_CUSTOM_XML {
            continue;
        }
        let item = resolve("xl", &target);
        let Some(bytes) = read_entry(zip, &item).ok().flatten() else { continue };
        let Some(scripts) = parse_scripts(&bytes) else { continue };
        let mut related = Vec::new();
        let rp = rels_path(&item);
        if let Some(rb) = read_entry(zip, &rp).ok().flatten() {
            let dir = item.rsplit_once('/').map_or("", |(d, _)| d).to_string();
            related.extend(rels_with_types(&rb).into_iter().filter(|(t, _)| t == REL_CUSTOM_XML_PROPS).map(|(_, t)| resolve(&dir, &t)));
            related.push(rp);
        }
        return Some(ScriptsPart { item, related, scripts });
    }
    None
}

/// New parts to add for scripts in a package that has none: (parts,
/// workbook relationship target, content-type overrides).
pub(crate) struct NewScriptParts {
    pub parts: Vec<(String, Vec<u8>)>,
    pub rel_target: String,
    pub content_types: Vec<(String, &'static str)>,
}

pub(crate) fn new_parts(scripts: &[ScriptModule], taken: &dyn Fn(&str) -> bool) -> NewScriptParts {
    let mut n = 1;
    while taken(&format!("customXml/item{n}.xml")) || taken(&format!("customXml/itemProps{n}.xml")) {
        n += 1;
    }
    let item = format!("customXml/item{n}.xml");
    let props = format!("customXml/itemProps{n}.xml");
    NewScriptParts {
        parts: vec![
            (item.clone(), scripts_xml(scripts).into_bytes()),
            (props.clone(), item_props_xml().into_bytes()),
            (rels_path(&item), item_rels_xml(&format!("itemProps{n}.xml")).into_bytes()),
        ],
        rel_target: format!("../{item}"),
        content_types: vec![(format!("/{item}"), CT_XML), (format!("/{props}"), CT_CUSTOM_XML_PROPS)],
    }
}

/// Insert `snippet` before the closing tag of the document's root.
fn before_root_close(doc: &[u8], snippet: &str) -> Result<Vec<u8>, String> {
    let text = String::from_utf8_lossy(doc);
    let close = text.rfind("</").ok_or("malformed XML part")?;
    Ok(format!("{}{snippet}{}", &text[..close], &text[close..]).into_bytes())
}

/// Add the scripts parts to a freshly written xlsx file.
pub(crate) fn add_to_file(path: &Path, scripts: &[ScriptModule]) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut zip = ZipArchive::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let names: Vec<String> = zip.file_names().flatten().map(|n| n.into_owned()).collect();
    let new = new_parts(scripts, &|p| names.iter().any(|n| n == p));
    let tmp = path.with_extension("xl-saving");
    {
        let file = File::create(&tmp).map_err(|e| e.to_string())?;
        let mut out = zip::ZipWriter::new(BufWriter::new(file));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for i in 0..zip.len() {
            let name = zip.by_index_raw(i).map_err(|e| e.to_string())?.name().map_err(|e| e.to_string())?.into_owned();
            let replacement = match name.as_str() {
                "[Content_Types].xml" => {
                    let doc = read_entry(&mut zip, &name)?.unwrap_or_default();
                    let o: String = new.content_types.iter().map(|(p, ct)| format!("<Override PartName=\"{p}\" ContentType=\"{ct}\"/>")).collect();
                    Some(before_root_close(&doc, &o)?)
                }
                "xl/_rels/workbook.xml.rels" => {
                    let doc = read_entry(&mut zip, &name)?.unwrap_or_default();
                    Some(before_root_close(&doc, &format!("<Relationship Id=\"rIdXlScripts\" Type=\"{REL_CUSTOM_XML}\" Target=\"{}\"/>", new.rel_target))?)
                }
                _ => None,
            };
            match replacement {
                Some(b) => {
                    out.start_file(name.as_str(), opts).map_err(|e| e.to_string())?;
                    out.write_all(&b).map_err(|e| e.to_string())?;
                }
                None => out.raw_copy_file(zip.by_index_raw(i).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
            }
        }
        for (name, b) in &new.parts {
            out.start_file(name.as_str(), opts).map_err(|e| e.to_string())?;
            out.write_all(b).map_err(|e| e.to_string())?;
        }
        out.finish().map_err(|e| e.to_string())?.flush().map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_round_trip_exactly() {
        let scripts = vec![
            ScriptModule { name: "Module \"1\" & <co>".into(), source: "-- a < b && c > d\r\nlocal s = \"]]>\"\n\tif x then end\n".into() },
            ScriptModule { name: "Empty".into(), source: String::new() },
        ];
        let xml = scripts_xml(&scripts);
        assert_eq!(parse_scripts(xml.as_bytes()).unwrap(), scripts);
        // Other Custom XML items aren't ours.
        assert_eq!(parse_scripts(b"<root xmlns=\"urn:other\"/>"), None);
        assert_eq!(parse_scripts(b"<xlScripts/>"), None);
        assert_eq!(parse_scripts(scripts_xml(&[]).as_bytes()), Some(vec![]));
        // Characters XML can't hold don't break the file.
        let odd = [ScriptModule { name: "x".into(), source: "a\u{1}b".into() }];
        assert_eq!(parse_scripts(scripts_xml(&odd).as_bytes()).unwrap()[0].source, "a\u{FFFD}b");
    }

    #[test]
    fn targets_resolve() {
        assert_eq!(resolve("xl", "../customXml/item1.xml"), "customXml/item1.xml");
        assert_eq!(resolve("xl", "/customXml/item2.xml"), "customXml/item2.xml");
        assert_eq!(resolve("customXml", "itemProps1.xml"), "customXml/itemProps1.xml");
        assert_eq!(rels_path("customXml/item1.xml"), "customXml/_rels/item1.xml.rels");
    }
}
