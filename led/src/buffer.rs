use std::fs;
use std::io;
use std::path::PathBuf;

pub struct Buffer {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub scroll_offset: usize,
}

impl Buffer {
    pub fn empty() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            path: None,
            dirty: false,
            scroll_offset: 0,
        }
    }

    pub fn from_file(path: &str) -> io::Result<Self> {
        let content = fs::read_to_string(path)?;
        let mut lines: Vec<String> = content.lines().map(String::from).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Ok(Self {
            lines,
            cursor_row: 0,
            cursor_col: 0,
            path: Some(PathBuf::from(path)),
            dirty: false,
            scroll_offset: 0,
        })
    }

    pub fn save(&mut self) -> io::Result<()> {
        if let Some(ref path) = self.path {
            let content = self.lines.join("\n") + "\n";
            fs::write(path, content)?;
            self.dirty = false;
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::Other, "No file path set"))
        }
    }

    pub fn filename(&self) -> &str {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("[scratch]")
    }

    // --- Cursor movement ---

    pub fn move_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.clamp_cursor_col();
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.clamp_cursor_col();
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.current_line_len();
        }
    }

    pub fn move_right(&mut self) {
        let len = self.current_line_len();
        if self.cursor_col < len {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    pub fn move_to_line_start(&mut self) {
        self.cursor_col = 0;
    }

    pub fn move_to_line_end(&mut self) {
        self.cursor_col = self.current_line_len();
    }

    // --- Text editing ---

    pub fn insert_char(&mut self, ch: char) {
        let col = self.cursor_col;
        self.lines[self.cursor_row].insert(col, ch);
        self.cursor_col += 1;
        self.dirty = true;
    }

    pub fn insert_newline(&mut self) {
        let col = self.cursor_col;
        let rest = self.lines[self.cursor_row][col..].to_string();
        self.lines[self.cursor_row].truncate(col);
        self.cursor_row += 1;
        self.lines.insert(self.cursor_row, rest);
        self.cursor_col = 0;
        self.dirty = true;
    }

    pub fn delete_char_backward(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
            self.lines[self.cursor_row].remove(self.cursor_col);
            self.dirty = true;
        } else if self.cursor_row > 0 {
            let removed = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
            self.lines[self.cursor_row].push_str(&removed);
            self.dirty = true;
        }
    }

    pub fn delete_char_forward(&mut self) {
        let len = self.current_line_len();
        if self.cursor_col < len {
            self.lines[self.cursor_row].remove(self.cursor_col);
            self.dirty = true;
        } else if self.cursor_row + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next);
            self.dirty = true;
        }
    }

    pub fn kill_line(&mut self) {
        let col = self.cursor_col;
        let len = self.current_line_len();
        if col < len {
            self.lines[self.cursor_row].truncate(col);
            self.dirty = true;
        } else if self.cursor_row + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next);
            self.dirty = true;
        }
    }

    // --- Helpers ---

    fn current_line_len(&self) -> usize {
        self.lines[self.cursor_row].len()
    }

    fn clamp_cursor_col(&mut self) {
        let len = self.current_line_len();
        if self.cursor_col > len {
            self.cursor_col = len;
        }
    }
}
