//! `ferrodb` — an interactive SQL shell over the ferrodb engine.
//!
//! Type SQL statements (optionally `;`-terminated). Dot-commands: `.tables`,
//! `.checkpoint`, `.exit`.

use engine::{Database, Output, SqlValue as Value};
use rustyline::DefaultEditor;

fn render_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(x) => x.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Text(s) => s.clone(),
    }
}

/// Render a result set as an aligned ASCII table.
fn print_table(columns: &[String], rows: &[Vec<Value>]) {
    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(render_value).collect())
        .collect();
    for row in &cells {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let sep: String = widths
        .iter()
        .map(|w| format!("+-{}-", "-".repeat(*w)))
        .collect::<String>()
        + "+";
    let fmt_row = |vals: &[String]| -> String {
        let mut line = String::new();
        for (i, v) in vals.iter().enumerate() {
            line += &format!("| {:<width$} ", v, width = widths[i]);
        }
        line + "|"
    };
    println!("{sep}");
    println!("{}", fmt_row(columns));
    println!("{sep}");
    for row in &cells {
        println!("{}", fmt_row(row));
    }
    println!("{sep}");
    println!(
        "({} row{})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
}

fn run_line(db: &mut Database, line: &str) {
    match db.execute(line) {
        Ok(Output::Rows { columns, rows }) => print_table(&columns, &rows),
        Ok(Output::Affected(n)) => println!("{n} row{} affected", if n == 1 { "" } else { "s" }),
        Ok(Output::Ack(msg)) => println!("{msg}"),
        Err(e) => eprintln!("Error: {e}"),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ferrodb.db".into());
    let mut db = Database::open(&path)?;
    let mut rl = DefaultEditor::new()?;
    println!("ferrodb SQL shell — {path}");
    println!("Enter SQL, or .tables / .checkpoint / .exit");
    while let Ok(line) = rl.readline("sql> ") {
        let trimmed = line.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(line.as_str());
        match trimmed {
            ".exit" => break,
            ".checkpoint" => match db.checkpoint() {
                Ok(()) => println!("ok"),
                Err(e) => eprintln!("Error: {e}"),
            },
            ".tables" => match db.list_tables() {
                Ok(names) => {
                    for n in names {
                        println!("{n}");
                    }
                }
                Err(e) => eprintln!("Error: {e}"),
            },
            sql => run_line(&mut db, sql),
        }
    }
    db.checkpoint()?;
    Ok(())
}
