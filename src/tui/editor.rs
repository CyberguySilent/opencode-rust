pub struct Editor {
    pub lines: Vec<Vec<char>>,
    pub row: usize,
    pub col: usize,
    pub hscroll: usize,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            lines: vec![Vec::new()],
            row: 0,
            col: 0,
            hscroll: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.is_empty())
    }

    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn clear(&mut self) {
        self.lines = vec![Vec::new()];
        self.row = 0;
        self.col = 0;
        self.hscroll = 0;
    }

    pub fn take_text(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
    }

    pub fn insert_char(&mut self, c: char) {
        let line = &mut self.lines[self.row];
        let idx = self.col.min(line.len());
        line.insert(idx, c);
        self.col = idx + 1;
    }

    pub fn insert_str(&mut self, text: &str) {
        let mut first = true;
        for part in text.split('\n') {
            if !first {
                self.newline();
            }
            first = false;
            for c in part.chars() {
                self.insert_char(c);
            }
        }
    }

    pub fn newline(&mut self) {
        let rest: Vec<char> = self.lines[self.row].split_off(self.col);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
        self.hscroll = 0;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            line.remove(self.col - 1);
            self.col -= 1;
            return;
        }
        if self.row > 0 {
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].extend(current);
        }
    }

    pub fn delete(&mut self) {
        let len = self.lines[self.row].len();
        if self.col < len {
            self.lines[self.row].remove(self.col);
            return;
        }
        if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].extend(next);
        }
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
    }

    pub fn right(&mut self) {
        let len = self.lines[self.row].len();
        if self.col < len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].len());
        }
    }

    pub fn down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].len());
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.lines[self.row].len();
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}
