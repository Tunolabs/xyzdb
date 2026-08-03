use xyzdb_core::record::Record;
use xyzdb_core::result::QueryResult;

/// Format a QueryResult as a human-readable string for the wire.
pub fn format_result(result: &QueryResult) -> String {
    match result {
        QueryResult::Ok { lid, message } => {
            if let Some(lid) = lid {
                format!("OK: {message}\nLID: {lid}")
            } else {
                format!("OK: {message}")
            }
        }
        QueryResult::BatchOk {
            count,
            first_lid,
            last_lid,
        } => {
            // "written", not "inserted": under ON CONFLICT UPDATE a batch record
            // whose anchor collides is updated rather than inserted, and `count`
            // is the number of records the batch applied either way. Text output
            // carries no compatibility guarantee (PROTOCOL.md §6).
            format!(
                "OK: {count} records written (batch)\nFirst LID: {first_lid}\nLast LID: {last_lid}"
            )
        }
        QueryResult::Records(records) => {
            if records.is_empty() {
                return "0 records found".into();
            }
            let mut out = String::new();
            for (i, rec) in records.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                out.push_str(&format_record(rec));
            }
            out.push_str(&format!("\n{} record(s) found", records.len()));
            out
        }
        QueryResult::Aggregation(map) => {
            let mut out = String::new();
            for (k, v) in map {
                out.push_str(&format!("{k}: {v}\n"));
            }
            out
        }
        QueryResult::Info(lines) => lines.join("\n"),
        QueryResult::GroupedAggregation(groups) => {
            let mut out = String::new();
            for (i, group) in groups.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                let parts: Vec<String> = group.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                out.push_str(&parts.join(", "));
            }
            out.push_str(&format!("\n{} group(s)", groups.len()));
            out
        }
        QueryResult::PaginatedRecords {
            records,
            cursor,
            has_more,
            budget_stop,
        } => {
            // v0.2.5.1: paginated SCAN. Render the page with the same record
            // box format used by `Records`, then append a footer line so a
            // human at the REPL can spot the pagination state and copy the
            // cursor token directly into the next query.
            let mut out = if records.is_empty() {
                "0 records found".to_string()
            } else {
                let mut s = String::new();
                for (i, rec) in records.iter().enumerate() {
                    if i > 0 {
                        s.push('\n');
                    }
                    s.push_str(&format_record(rec));
                }
                s.push_str(&format!("\n{} record(s) on this page", records.len()));
                s
            };
            out.push_str(&format!("\nhas_more: {has_more}"));
            if let Some(bs) = budget_stop {
                out.push_str(&format!(
                    "\nbudget_stop: examined {} of {} candidates, found {}",
                    bs.examined, bs.candidates, bs.found
                ));
            }
            if let Some(token) = cursor {
                out.push_str(&format!("\ncursor: {token}"));
            }
            out
        }
    }
}

fn format_record(rec: &Record) -> String {
    // Size the box to its widest content line so the right border always aligns.
    // A fixed field width left the border 1 column short of the content rows and
    // broke entirely when a LID or value overflowed the hard-coded width.
    let mut lines = Vec::with_capacity(rec.fields.len() + 2);
    lines.push(format!("LID: {}", rec.lid));
    lines.push(format!("Lobe: {}", rec.lobe_name));
    for (k, v) in &rec.fields {
        lines.push(format!("{k}: {v}"));
    }
    let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let border = "─".repeat(width + 2);
    let mut out = String::new();
    out.push_str(&format!("┌{border}┐\n"));
    for line in &lines {
        let pad = " ".repeat(width - line.chars().count());
        out.push_str(&format!("│ {line}{pad} │\n"));
    }
    out.push_str(&format!("└{border}┘"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use xyzdb_core::lid::LID;
    use xyzdb_core::value::Value;

    /// Every rendered row — top border, each field, bottom border — must span
    /// the same number of columns, even when a value is far longer than any
    /// legacy fixed width. Guards the record-box alignment the quickstart shows.
    #[test]
    fn record_box_rows_are_equal_width() {
        let mut fields = BTreeMap::new();
        fields.insert("short".to_string(), Value::Int(7));
        fields.insert(
            "very_long_field_name".to_string(),
            Value::Text("a value that comfortably overflows the old 48-column cap".to_string()),
        );
        let rec = Record {
            lid: LID::new(1),
            lobe_name: "workspace".to_string(),
            fields,
            created_at: 0,
            updated_at: 0,
        };

        let widths: Vec<usize> = format_record(&rec)
            .lines()
            .map(|l| l.chars().count())
            .collect();
        assert!(widths.len() >= 4, "expected border + rows, got {widths:?}");
        assert!(
            widths.iter().all(|&w| w == widths[0]),
            "box rows misaligned: {widths:?}"
        );
    }
}
