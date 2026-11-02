//! Plain aligned text tables.

pub struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(header: &[&str]) -> Self {
        Table { header: header.iter().map(|s| s.to_string()).collect(), rows: Vec::new() }
    }

    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn print(&self) {
        print!("{}", self.render());
    }

    /// The table as text, a line per row.
    pub fn render(&self) -> String {
        let n = self.header.len();
        let mut w: Vec<usize> = self.header.iter().map(|h| h.chars().count()).collect();
        for r in &self.rows {
            for (i, c) in r.iter().enumerate().take(n) {
                w[i] = w[i].max(c.chars().count());
            }
        }
        let line = |cells: &[String]| {
            let s: Vec<String> = cells
                .iter()
                .enumerate()
                .take(n)
                .map(|(i, c)| format!("{c}{}", " ".repeat(w[i] - c.chars().count())))
                .collect();
            format!("{}\n", s.join("  ").trim_end())
        };
        let mut out = line(&self.header);
        for r in &self.rows {
            out += &line(r);
        }
        out
    }
}

pub fn opt(s: &Option<String>) -> String {
    s.clone().unwrap_or_else(|| "-".into())
}

pub fn when(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| t.to_string())
}
