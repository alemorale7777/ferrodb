//! `ferrodb` — an interactive SQL shell over the ferrodb engine.
//!
//! Type SQL statements (optionally `;`-terminated). Transactions: `BEGIN`,
//! `COMMIT`, `ROLLBACK`. Dot-commands: `.tables`, `.vacuum`, `.checkpoint`,
//! `.exit`.

use engine::{Database, Output, SqlValue as Value, TxnId};
use rustyline::DefaultEditor;

fn render_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(x) => x.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Text(s) => s.clone(),
        Value::Vector(v) => {
            let inner: Vec<String> = v.iter().map(|x| x.to_string()).collect();
            format!("[{}]", inner.join(", "))
        }
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

fn print_output(out: Output) {
    match out {
        Output::Rows { columns, rows } => print_table(&columns, &rows),
        Output::Affected(n) => println!("{n} row{} affected", if n == 1 { "" } else { "s" }),
        Output::Ack(msg) => println!("{msg}"),
    }
}

/// Run one SQL statement, inside the open transaction if there is one.
fn run_line(db: &mut Database, txn: Option<TxnId>, line: &str) {
    let result = match txn {
        Some(t) => db.execute_in(t, line),
        None => db.execute(line),
    };
    match result {
        Ok(out) => print_output(out),
        Err(e) => eprintln!("Error: {e}"),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ferrodb.db".into());
    let mut db = Database::open(&path)?;
    let mut rl = DefaultEditor::new()?;
    let mut txn: Option<TxnId> = None;
    println!("ferrodb SQL shell — {path}");
    println!("Enter SQL, BEGIN/COMMIT/ROLLBACK, or .tables / .vacuum / .checkpoint / .exit");
    while let Ok(line) = rl.readline(if txn.is_some() { "sql*> " } else { "sql> " }) {
        let trimmed = line.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(line.as_str());
        match trimmed.to_ascii_uppercase().as_str() {
            ".EXIT" => break,
            "BEGIN" => {
                if txn.is_some() {
                    eprintln!("Error: a transaction is already open");
                } else {
                    txn = Some(db.begin());
                    println!("BEGIN");
                }
            }
            "COMMIT" => match txn.take() {
                Some(t) => match db.commit_txn(t) {
                    Ok(()) => println!("COMMIT"),
                    Err(e) => eprintln!("Error: {e}"),
                },
                None => eprintln!("Error: no transaction open"),
            },
            "ROLLBACK" => match txn.take() {
                Some(t) => match db.rollback_txn(t) {
                    Ok(()) => println!("ROLLBACK"),
                    Err(e) => eprintln!("Error: {e}"),
                },
                None => eprintln!("Error: no transaction open"),
            },
            ".VACUUM" => match db.vacuum() {
                Ok(out) => print_output(out),
                Err(e) => eprintln!("Error: {e}"),
            },
            ".CHECKPOINT" => match db.checkpoint() {
                Ok(()) => println!("ok"),
                Err(e) => eprintln!("Error: {e}"),
            },
            ".TABLES" => match db.list_tables() {
                Ok(names) => {
                    for n in names {
                        println!("{n}");
                    }
                }
                Err(e) => eprintln!("Error: {e}"),
            },
            _ => run_line(&mut db, txn, trimmed),
        }
    }
    if let Some(t) = txn.take() {
        db.rollback_txn(t)?;
    }
    db.checkpoint()?;
    Ok(())
}
