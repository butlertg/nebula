//! A task run's output, on disk.
//!
//! The scrollback ring holds a megabyte in memory and dies with the daemon,
//! which is fine for a session somebody is watching and useless for one that
//! ran at 3am. A run's transcript is the raw PTY byte stream written straight
//! through to a file: escape sequences and all, because that is what the
//! child actually printed and anything cleverer would be a guess at what an
//! agent CLI's redraws meant.
//!
//! It is capped. An agent CLI repaints its whole screen constantly, so an
//! overnight loop can print hundreds of megabytes of cursor moves; past
//! [`CAP_BYTES`] the file stops growing and says so, once, at the end. The
//! head is kept rather than the tail: what a run started doing explains what
//! went wrong far more often than its last repaint, and the outcome, the
//! diffstat, and the agent's own summary already cover the ending.

use std::fs::File;
use std::io::Write;
use std::path::Path;

/// Where a transcript stops growing. Generous enough to hold a real night's
/// work in full, small enough that a runaway child cannot fill the disk.
pub const CAP_BYTES: u64 = 32 * 1024 * 1024;

pub struct Transcript {
    file: File,
    written: u64,
    /// Set once the cap notice has been appended, so it is written exactly
    /// once no matter how much more the child prints.
    capped: bool,
}

impl Transcript {
    /// Create the file, and the run directory it lives in.
    pub fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            file: File::create(path)?,
            written: 0,
            capped: false,
        })
    }

    /// Append output. Errors are swallowed deliberately: a full disk or a
    /// deleted run directory must not take down the session whose work this
    /// is only a record of.
    pub fn write(&mut self, data: &[u8]) {
        if self.capped {
            return;
        }
        let room = CAP_BYTES.saturating_sub(self.written);
        if room == 0 {
            self.cap();
            return;
        }
        let take = (data.len() as u64).min(room) as usize;
        if self.file.write_all(&data[..take]).is_err() {
            self.capped = true;
            return;
        }
        self.written += take as u64;
        if self.written >= CAP_BYTES {
            self.cap();
        }
    }

    fn cap(&mut self) {
        self.capped = true;
        let note = format!(
            "\n\n[nebula] transcript capped at {} MiB — the session kept running, \
             this file stopped growing.\n",
            CAP_BYTES / (1024 * 1024)
        );
        let _ = self.file.write_all(note.as_bytes());
        let _ = self.file.flush();
    }

    pub fn bytes_written(&self) -> u64 {
        self.written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_bytes_it_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runs/one/transcript.log");
        let mut t = Transcript::create(&path).unwrap();
        t.write(b"hello ");
        t.write(b"\x1b[2Kworld");
        drop(t);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"hello \x1b[2Kworld".to_vec(),
            "the escape sequence is kept: this is the raw stream, not a render"
        );
    }

    #[test]
    fn creates_the_run_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a/b/c/transcript.log");
        Transcript::create(&path).unwrap();
        assert!(path.exists());
    }

    /// The cap is the whole point of the type — a repainting TUI would
    /// otherwise write until the disk was full.
    #[test]
    fn stops_at_the_cap_and_says_so_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("transcript.log");
        let mut t = Transcript::create(&path).unwrap();
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..40 {
            t.write(&chunk);
        }
        assert_eq!(t.bytes_written(), CAP_BYTES);
        drop(t);

        let written = std::fs::read(&path).unwrap();
        let text = String::from_utf8_lossy(&written);
        assert_eq!(
            text.matches("transcript capped").count(),
            1,
            "the notice is appended once, not on every write after the cap"
        );
        assert!(
            written.len() as u64 > CAP_BYTES && (written.len() as u64) < CAP_BYTES + 4096,
            "the file is the cap plus the notice, not 40 MiB: {}",
            written.len()
        );
    }
}
