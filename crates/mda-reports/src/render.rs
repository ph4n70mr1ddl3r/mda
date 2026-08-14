//! Report renderers (PLAN Phase 7): HTML, XLSX and PDF alongside the CSV
//! writer in `lib`. All renderers share one contract: escape/sanitize per
//! target format (HTML entity-escapes; PDF text runs are latin-1-safe and
//! backslash/paren-escaped; XLSX goes through `rust_xlsxwriter`, which handles
//! its own escaping), and never lose row/column fidelity.

use crate::ReportResult;
use mda_core::Result;
use serde_json::Value;

/// Render a result as a self-contained HTML table (no external assets).
pub fn to_html(res: &ReportResult, title: &str) -> String {
    let mut s = String::new();
    s.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    s.push_str("<title>");
    s.push_str(&html_escape(title));
    s.push_str("</title>\n<style>\n");
    s.push_str("body{font-family:system-ui,sans-serif;margin:2rem;color:#111}\n");
    s.push_str("table{border-collapse:collapse}\n");
    s.push_str("th,td{border:1px solid #ccc;padding:4px 10px;text-align:left}\n");
    s.push_str("th{background:#f4f4f4}\n");
    s.push_str("caption{caption-side:top;text-align:left;font-weight:600;padding-bottom:.5rem}\n");
    s.push_str("</style>\n</head>\n<body>\n<table>\n<caption>");
    s.push_str(&html_escape(title));
    s.push_str("</caption>\n<thead>\n<tr>");
    for c in &res.columns {
        s.push_str("<th>");
        s.push_str(&html_escape(c));
        s.push_str("</th>");
    }
    s.push_str("</tr>\n</thead>\n<tbody>\n");
    for row in &res.rows {
        s.push_str("<tr>");
        for c in &res.columns {
            s.push_str("<td>");
            s.push_str(&html_escape(&cell_text(row.get(c))));
            s.push_str("</td>");
        }
        s.push_str("</tr>\n");
    }
    s.push_str("</tbody>\n</table>\n</body>\n</html>\n");
    s
}

/// Render a result as an XLSX workbook (one sheet, header row bolded, columns
/// auto-filtered; numbers stay numbers so downstream analysis keeps typing).
pub fn to_xlsx(res: &ReportResult, title: &str) -> Result<Vec<u8>> {
    use rust_xlsxwriter::{Format, Workbook};
    let mut wb = Workbook::new();
    let sheet_name = sanitize_sheet_name(title);
    let ws = wb
        .add_worksheet()
        .set_name(&sheet_name)
        .map_err(mda_core::Error::internal)?;
    let bold = Format::new().set_bold();
    for (i, c) in res.columns.iter().enumerate() {
        ws.write_string_with_format(0, i as u16, c, &bold)
            .map_err(mda_core::Error::internal)?;
    }
    for (r, row) in res.rows.iter().enumerate() {
        for (c, name) in res.columns.iter().enumerate() {
            let cell = row.get(name).cloned().unwrap_or(Value::Null);
            let r = (r + 1) as u32;
            let c = c as u16;
            match cell {
                Value::Null => {}
                Value::Bool(b) => {
                    ws.write_boolean(r, c, b)
                        .map_err(mda_core::Error::internal)?;
                }
                Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        ws.write_number(r, c, i as f64)
                    } else {
                        ws.write_number(r, c, n.as_f64().unwrap_or(0.0))
                    }
                    .map_err(mda_core::Error::internal)?;
                }
                Value::String(s) => {
                    ws.write_string(r, c, s)
                        .map_err(mda_core::Error::internal)?;
                }
                other => {
                    ws.write_string(r, c, other.to_string())
                        .map_err(mda_core::Error::internal)?;
                }
            }
        }
    }
    let _ = ws.autofilter(
        0,
        0,
        res.rows.len() as u32,
        (res.columns.len().max(1) - 1) as u16,
    );
    wb.save_to_buffer().map_err(mda_core::Error::internal)
}

/// Render a result as a paginated PDF (letter, landscape-friendly column
/// layout). A **self-contained minimal PDF 1.4 writer**: base-14 Courier text
/// (its 0.6em fixed advance makes column layout exact — no font metrics table
/// needed), hairline rules, bold- Courier headers, one page per
/// [`PDF_ROWS_PER_PAGE`] rows.
pub fn to_pdf(res: &ReportResult, title: &str) -> Result<Vec<u8>> {
    let doc = PdfTable::new(res, title).render();
    Ok(doc)
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn cell_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn sanitize_sheet_name(s: &str) -> String {
    let mut name: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' || c == '_' || c == '-' {
                c
            } else {
                ' '
            }
        })
        .collect();
    name.truncate(31);
    if name.trim().is_empty() {
        name = "Report".to_string();
    }
    name
}

// ===== minimal PDF writer =====
//
// Object model: 1 catalog, 2 pages-tree, 3 font Courier, 4 font Courier-Bold,
// then per page a Page object and a content stream. Text is laid out on a
// fixed grid; every glyph advance in the base-14 Courier family is exactly
// 0.6 x fontsize, so column width in points = 0.6 * size * chars.

const PAGE_W: f64 = 612.0;
const PAGE_H: f64 = 792.0;
const MARGIN: f64 = 36.0;
const FONT_SIZE: f64 = 8.0;
const LINE_H: f64 = 12.0;
const HEADER_H: f64 = 30.0; // title block
const PDF_ROWS_PER_PAGE: usize =
    (((PAGE_H - 2.0 * MARGIN - HEADER_H) / LINE_H) as usize).saturating_sub(2);
const CHAR_W: f64 = FONT_SIZE * 0.6;
const MAX_COL_CHARS: usize = 30;

struct PdfTable<'a> {
    res: &'a ReportResult,
    title: &'a str,
    col_chars: Vec<usize>,
}

impl<'a> PdfTable<'a> {
    fn new(res: &'a ReportResult, title: &'a str) -> Self {
        // Column width in chars: max of header/cells, capped, then scaled down
        // proportionally if the total overflows the printable width.
        let mut widths: Vec<usize> = res
            .columns
            .iter()
            .map(|c| {
                let w = res
                    .rows
                    .iter()
                    .map(|r| cell_text(r.get(c)).chars().count())
                    .max()
                    .unwrap_or(0)
                    .max(c.chars().count());
                w.clamp(4, MAX_COL_CHARS)
            })
            .collect();
        let avail = ((PAGE_W - 2.0 * MARGIN) / CHAR_W).floor() as usize;
        let total: usize = widths.iter().sum::<usize>() + widths.len().max(1) * 2;
        if total > avail {
            let scale = avail as f64 / total as f64;
            for w in &mut widths {
                *w = (*w as f64 * scale).floor().max(3.0) as usize;
            }
        }
        Self {
            res,
            title,
            col_chars: widths,
        }
    }

    fn render(&self) -> Vec<u8> {
        let mut pages: Vec<String> = Vec::new();
        let mut chunk_start = 0usize;
        while chunk_start == 0 || chunk_start < self.res.rows.len() {
            let end = (chunk_start + PDF_ROWS_PER_PAGE).min(self.res.rows.len());
            pages.push(self.page_content(chunk_start, end, pages.len() + 1));
            if end >= self.res.rows.len() {
                break;
            }
            chunk_start = end;
        }
        if self.res.rows.is_empty() {
            pages.push(self.page_content(0, 0, 1));
        }
        assemble_pdf(pages)
    }

    /// One page's content stream (rows [start,end) plus the header grid; every
    /// page repeats the column header row so torn-off pages stay readable).
    fn page_content(&self, start: usize, end: usize, page_no: usize) -> String {
        let mut ops = String::new();
        // title + rule
        ops.push_str(&text(
            MARGIN,
            PAGE_H - MARGIN - 10.0,
            FONT_SIZE + 2.0,
            true,
            self.title,
        ));
        ops.push_str(&format!(
            "0.6 w {} {} m {} {} l S\n",
            MARGIN,
            PAGE_H - MARGIN - 16.0,
            PAGE_W - MARGIN,
            PAGE_H - MARGIN - 16.0
        ));
        let table_top = PAGE_H - MARGIN - HEADER_H;
        // column header row (bold, light-gray band)
        ops.push_str(&format!(
            "0.92 0.92 0.92 rg {} {} {} {} re f 0 0 0 rg\n",
            MARGIN,
            table_top - LINE_H,
            PAGE_W - 2.0 * MARGIN,
            LINE_H
        ));
        ops.push_str("0 0 0 rg\n");
        let mut x = MARGIN;
        for (i, c) in self.res.columns.iter().enumerate() {
            let w = self.col_chars[i];
            ops.push_str(&text(
                x,
                table_top - LINE_H + 3.0,
                FONT_SIZE,
                true,
                &truncate(c, w),
            ));
            x += (w as f64 + 2.0) * CHAR_W;
        }
        // rows
        let mut y = table_top - LINE_H;
        for (ri, row) in self.res.rows[start..end].iter().enumerate() {
            y -= LINE_H;
            if ri % 2 == 1 {
                ops.push_str(&format!(
                    "0.97 0.97 0.97 rg {} {} {} {} re f 0 0 0 rg\n",
                    MARGIN,
                    y,
                    PAGE_W - 2.0 * MARGIN,
                    LINE_H
                ));
            }
            let mut x = MARGIN;
            for (i, c) in self.res.columns.iter().enumerate() {
                let w = self.col_chars[i];
                let v = cell_text(row.get(c));
                ops.push_str(&text(x, y + 3.0, FONT_SIZE, false, &truncate(&v, w)));
                x += (w as f64 + 2.0) * CHAR_W;
            }
        }
        // column separators (hairline)
        ops.push_str("0.85 G 0.3 w\n");
        let bottom = y;
        let mut x = MARGIN;
        for (i, _) in self.res.columns.iter().enumerate() {
            ops.push_str(&format!("{} {} m {} {} l S\n", x, table_top, x, bottom));
            x += (self.col_chars[i] as f64 + 2.0) * CHAR_W;
        }
        ops.push_str("0 G\n");
        // outer frame
        ops.push_str(&format!(
            "0.5 w {} {} {} {} re S\n",
            MARGIN,
            bottom,
            PAGE_W - 2.0 * MARGIN,
            table_top - bottom
        ));
        // footer: page number + row range
        ops.push_str(&text(
            MARGIN,
            MARGIN / 2.0,
            FONT_SIZE - 1.0,
            false,
            &format!(
                "page {page_no} · rows {}-{} of {}",
                start + 1,
                end.max(start + 1),
                self.res.rows.len()
            ),
        ));
        ops
    }
}

/// `BT /F<n> <size> Tf x y Td (..) Tj ET` — coordinates translated to PDF's
/// bottom-left origin by the caller (we pass PDF-space y directly).
fn text(x: f64, y: f64, size: f64, bold: bool, s: &str) -> String {
    let f = if bold { "/F2" } else { "/F1" };
    format!("BT {f} {size} Tf {x} {y} Td ({}) Tj ET\n", pdf_escape(s))
}

fn pdf_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '(' => out.push_str("\\("),
            ')' => out.push_str("\\)"),
            '\r' | '\n' => out.push(' '),
            c if (c as u32) < 256 => out.push(c),
            _ => out.push('?'),
        }
    }
    out
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        t.push('…');
        t.replace('…', "-")
    }
}

/// Assemble the object graph + xref table. Layout:
/// 1 catalog, 2 pages, 3 F1 (Courier), 4 F2 (Courier-Bold), then for page i:
/// 5 + 2i = page dict, 6 + 2i = content stream.
fn assemble_pdf(pages: Vec<String>) -> Vec<u8> {
    let n_pages = pages.len();
    let kids: Vec<String> = (0..n_pages).map(|i| format!("{} 0 R", 5 + 2 * i)).collect();
    let mut objs: Vec<(usize, String)> = Vec::new();
    objs.push((1, "<< /Type /Catalog /Pages 2 0 R >>".to_string()));
    objs.push((
        2,
        format!(
            "<< /Type /Pages /Kids [{}] /Count {n_pages} >>",
            kids.join(" ")
        ),
    ));
    objs.push((
        3,
        "<< /Type /Font /Subtype /Type1 /BaseFont /Courier /Encoding /WinAnsiEncoding >>"
            .to_string(),
    ));
    objs.push((
        4,
        "<< /Type /Font /Subtype /Type1 /BaseFont /Courier-Bold /Encoding /WinAnsiEncoding >>"
            .to_string(),
    ));
    for (i, content) in pages.iter().enumerate() {
        let page_no = 5 + 2 * i;
        let content_no = page_no + 1;
        objs.push((
            page_no,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W} {PAGE_H}] \
                 /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {content_no} 0 R >>"
            ),
        ));
        objs.push((
            content_no,
            format!(
                "<< /Length {} >>\nstream\n{content}endstream",
                content.len()
            ),
        ));
    }

    let mut out = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
    let mut offsets = vec![0u32; objs.len() + 1];
    for (no, body) in &objs {
        offsets[*no] = out.len() as u32;
        out.extend_from_slice(format!("{no} 0 obj\n{body}\nendobj\n").as_bytes());
    }
    let xref = out.len() as u32;
    out.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets[1..] {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    fn sample() -> ReportResult {
        let mut r1 = Map::new();
        r1.insert("name".into(), Value::String("Acme, <Inc.>".into()));
        r1.insert("total".into(), Value::from(1250.5));
        let mut r2 = Map::new();
        r2.insert("name".into(), Value::String("Zeta (long \\ name)".into()));
        r2.insert("total".into(), Value::Null);
        ReportResult {
            columns: vec!["name".into(), "total".into()],
            rows: vec![r1, r2],
        }
    }

    #[test]
    fn html_escapes_cells() {
        let html = to_html(&sample(), "Sales <Q3>");
        assert!(html.contains("&lt;Q3&gt;"));
        assert!(html.contains("Acme, &lt;Inc.&gt;"));
        assert!(!html.contains("<Inc.>"));
    }

    #[test]
    fn xlsx_is_a_zip_with_the_sheet() {
        let bytes = to_xlsx(&sample(), "Sales by Month").unwrap();
        // XLSX is a ZIP: local file header magic PK\x03\x04 + the workbook part.
        assert_eq!(&bytes[..2], b"PK");
        assert!(bytes.windows(6).any(|w| w == b"sheet1"));
        assert!(bytes.windows(30).any(|w| w.starts_with(b"xl/workbook.xml")));
    }

    #[test]
    fn pdf_is_structurally_valid() {
        let bytes = to_pdf(&sample(), "Sales by Month").unwrap();
        assert!(bytes.starts_with(b"%PDF-1.4"));
        assert!(bytes.ends_with(b"%%EOF\n"));
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("/Type /Catalog"));
        assert!(s.contains("/Count 1"));
        // escaped cell text never breaks the literal
        assert!(s.contains(r"Zeta \(long \\ name\)"));
    }

    #[test]
    fn pdf_paginates_long_results() {
        let mut rows = Vec::new();
        for i in 0..120 {
            let mut m = Map::new();
            m.insert("n".into(), Value::from(i));
            rows.push(m);
        }
        let res = ReportResult {
            columns: vec!["n".into()],
            rows,
        };
        let bytes = to_pdf(&res, "big").unwrap();
        let s = String::from_utf8_lossy(&bytes);
        // PDF_ROWS_PER_PAGE is 55 on a letter page: 120 rows -> 3 pages.
        assert!(s.contains("/Count 3"), "120 rows split across 3 pages");
    }

    #[test]
    fn pdf_cell_values_are_escaped() {
        let mut m = Map::new();
        m.insert("v".into(), Value::String("a(b)c\\d".into()));
        let res = ReportResult {
            columns: vec!["v".into()],
            rows: vec![m],
        };
        let bytes = to_pdf(&res, "t").unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains(r"a\(b\)c\\d"));
    }

    #[test]
    fn empty_result_renders_all_formats() {
        let res = ReportResult {
            columns: vec!["a".into()],
            rows: vec![],
        };
        // headers only — no phantom rows
        assert!(to_html(&res, "empty").contains("<th>a</th>"));
        assert!(!to_html(&res, "empty").contains("<td>"));
        assert!(to_xlsx(&res, "empty").is_ok());
        assert!(to_pdf(&res, "empty").is_ok());
    }
}
