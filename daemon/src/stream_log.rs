//! Tail the live Sunshine/Apollo info log without replaying its history.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::PathBuf,
};

use rustix::fs::{Mode, OFlags, open};

const READ_LIMIT: usize = 64 * 1024;
const LINE_LIMIT: usize = 4096;

pub struct Log {
    path: PathBuf,
    file_id: Option<(u64, u64)>,
    offset: u64,
    line: Vec<u8>,
    discard_line: bool,
    initialized: bool,
    active_clients: u16,
    last_error: Option<io::ErrorKind>,
}

impl Log {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            file_id: None,
            offset: 0,
            line: Vec::new(),
            discard_line: false,
            initialized: false,
            active_clients: 0,
            last_error: None,
        }
    }

    pub fn poll(&mut self) -> bool {
        match self.read(!self.initialized) {
            Ok(()) => {
                self.initialized = true;
                self.last_error = None;
            }
            Err(error) => {
                if self.last_error != Some(error.kind()) && error.kind() != io::ErrorKind::NotFound
                {
                    eprintln!("Cannot read streaming log {}: {error}", self.path.display());
                }
                self.last_error = Some(error.kind());
            }
        }
        self.active_clients != 0
    }

    fn read(&mut self, initial: bool) -> io::Result<()> {
        let fd = open(
            &self.path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )?;
        let mut file = File::from(fd);
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Streaming log must be a regular file",
            ));
        }
        let id = (metadata.dev(), metadata.ino());
        if initial {
            self.file_id = Some(id);
            self.offset = metadata.len();
            if self.offset > 0 {
                file.seek(SeekFrom::Start(self.offset - 1))?;
                let mut byte = [0];
                file.read_exact(&mut byte)?;
                self.discard_line = byte[0] != b'\n';
            }
            return Ok(());
        }
        if self.file_id != Some(id) || metadata.len() < self.offset {
            if self.file_id == Some(id) {
                // A truncated log has lost its prior lifecycle evidence.
                self.active_clients = 0;
            }
            self.file_id = Some(id);
            self.offset = 0;
            self.line.clear();
            self.discard_line = false;
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.take(READ_LIMIT as u64).read_to_end(&mut bytes)?;
        self.offset += bytes.len() as u64;
        for byte in bytes {
            if byte == b'\n' {
                if !self.discard_line {
                    apply_line(&self.line, &mut self.active_clients);
                }
                self.line.clear();
                self.discard_line = false;
            } else if self.line.len() >= LINE_LIMIT {
                self.discard_line = true;
            } else if !self.discard_line {
                self.line.push(byte);
            }
        }
        Ok(())
    }
}

fn apply_line(line: &[u8], active: &mut u16) {
    let Ok(line) = std::str::from_utf8(line) else {
        return;
    };
    let message = line
        .trim_end()
        .split_once("Info: ")
        .map_or(line.trim_end(), |(_, msg)| msg);
    match message {
        "CLIENT CONNECTED" => *active = active.saturating_add(1),
        "CLIENT DISCONNECTED" => *active = active.saturating_sub(1),
        message
            if message.starts_with("Sunshine version: ")
                || message.starts_with("Apollo version: ") =>
        {
            *active = 0
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    #[test]
    fn log_skips_history_and_tracks_clients_across_partial_lines_and_rotation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("host.log");
        fs::write(&path, b"Info: CLIENT CONNECTED\n").unwrap();
        let mut log = Log::new(path.clone());
        assert!(!log.poll());
        let append = |text: &[u8]| {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(text)
                .unwrap();
        };
        append(b"Info: CLIENT CONNE");
        assert!(!log.poll());
        append(b"CTED\nInfo: CLIENT CONNECTED\nInfo: CLIENT DISCONNECTED\n");
        assert!(log.poll());
        fs::rename(&path, temp.path().join("host.log.1")).unwrap();
        assert!(log.poll());
        append(b"Info: CLIENT DISCONNECTED\n");
        assert!(!log.poll());
        append(b"Info: CLIENT CONNECTED\n");
        assert!(log.poll());
        fs::write(&path, b"").unwrap();
        assert!(!log.poll());
    }
}
