use std::fs::{File, OpenOptions};
use std::io::{self, Write}; 
use std::path::Path;
use std::time::SystemTime;



pub struct Logger {
    file: Option<File>
}

impl Logger {
    pub fn null() ->Self {
        Self { file: None }
    }

    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self { file: Some(file) })
    }

    pub fn log(&mut self, msg: &str) {
        let Some(f) = self.file.as_mut() else { return; };
        let time_s = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{time_s}]: {msg}");
        let _ = f.flush();
    }
}
