use std::io;

use ropey::Rope;

use crate::WriteContent;

pub trait Doc: Send + Sync {
    fn line_count(&self) -> usize;
    fn line(&self, idx: usize) -> String;
    fn dirty(&self) -> bool;
    fn clone_box(&self) -> Box<dyn Doc>;
}

impl Clone for Box<dyn Doc> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

#[derive(Clone)]
pub struct TextDoc {
    rope: Rope,
}

impl TextDoc {
    pub fn from_reader(reader: impl io::Read) -> io::Result<Self> {
        let rope = Rope::from_reader(reader)?;
        Ok(TextDoc { rope })
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }
}

impl Doc for TextDoc {
    fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    fn line(&self, idx: usize) -> String {
        if idx >= self.rope.len_lines() {
            return String::new();
        }
        let line = self.rope.line(idx);
        let s = line.to_string();
        s.trim_end_matches(&['\n', '\r'][..]).to_string()
    }

    fn dirty(&self) -> bool {
        false
    }

    fn clone_box(&self) -> Box<dyn Doc> {
        Box::new(TextDoc {
            rope: self.rope.clone(),
        })
    }
}

impl WriteContent for TextDoc {
    fn write_to(&self, writer: &mut dyn io::Write) -> io::Result<()> {
        for chunk in self.rope.chunks() {
            writer.write_all(chunk.as_bytes())?;
        }
        Ok(())
    }
}
