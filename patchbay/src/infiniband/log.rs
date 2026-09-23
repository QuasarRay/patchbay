//! Bounded prefixes, continuously drained. A fabric shares one retained-byte budget.
use std::{
    fs::File,
    io::{self, Read, Write},
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;

const PER_LOG: usize = 16 * 1024 * 1024;
const PER_FABRIC: usize = 64 * 1024 * 1024;
#[derive(Clone)]
pub(super) struct Budget(Arc<Mutex<usize>>);
impl Budget {
    pub(super) fn new() -> Self {
        Self(Arc::new(Mutex::new(PER_FABRIC)))
    }
    pub(super) fn file(&self, path: &Path) -> io::Result<CappedFile> {
        Ok(CappedFile {
            file: File::create(path)?,
            remaining: PER_LOG,
            budget: self.clone(),
        })
    }
    pub(super) fn capture(&self, path: &Path) -> io::Result<(Stdio, Completion)> {
        let file = self.file(path)?;
        let (reader, writer) = io::pipe()?;
        let (done, receiver) = oneshot::channel();
        std::thread::Builder::new()
            .name("ib-log-drain".into())
            .spawn(move || {
                let _ = done.send(drain(reader, file));
            })?;
        Ok((writer.into(), Completion(receiver)))
    }
}
pub(super) struct CappedFile {
    file: File,
    remaining: usize,
    budget: Budget,
}
impl Write for CappedFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut remaining = self
            .budget
            .0
            .lock()
            .map_err(|_| io::Error::other("log budget poisoned"))?;
        let keep = bytes.len().min(self.remaining).min(*remaining);
        if keep == 0 && !bytes.is_empty() {
            return Err(io::Error::other("IB retained log budget exhausted"));
        }
        // Reserve before writing, even if a later I/O error loses this allowance.
        *remaining -= keep;
        self.remaining -= keep;
        self.file.write_all(&bytes[..keep])?;
        Ok(keep)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
fn drain(mut reader: impl Read, mut file: impl Write) -> io::Result<()> {
    let mut buffer = [0u8; 8192];
    let mut error = None;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if error.is_none() {
            error = file.write_all(&buffer[..count]).err();
        }
        // Keep consuming after truncation or disk error so writers cannot deadlock.
    }
    if let Some(error) = error {
        Err(error)
    } else {
        file.flush()
    }
}
pub(super) struct Completion(oneshot::Receiver<io::Result<()>>);
impl Completion {
    pub(super) async fn finish(self) -> io::Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(2), self.0)
            .await
            .map_err(|_| io::Error::other("IB log drain timed out"))?
            .map_err(io::Error::other)?
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn noisy_streams_share_a_hard_budget_and_are_fully_drained() {
        let dir = testdir::testdir!();
        let budget = Budget(Arc::new(Mutex::new(100)));
        let mut input = io::Cursor::new(vec![42u8; 200]);
        assert!(drain(&mut input, budget.file(&dir.join("one")).unwrap()).is_err());
        assert_eq!(input.position(), 200);
        assert_eq!(std::fs::metadata(dir.join("one")).unwrap().len(), 100);
        assert!(
            drain(
                io::Cursor::new(vec![1; 10]),
                budget.file(&dir.join("two")).unwrap()
            )
            .is_err()
        );
        assert_eq!(std::fs::metadata(dir.join("two")).unwrap().len(), 0);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn pipe_is_bounded_before_writer_exits() {
        let dir = testdir::testdir!();
        let budget = Budget(Arc::new(Mutex::new(100)));
        let (out, done) = budget.capture(&dir.join("live")).unwrap();
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "head -c 1048576 /dev/zero; sleep 0.2"])
            .stdout(out)
            .spawn()
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(child.try_wait().unwrap().is_none());
        assert!(std::fs::metadata(dir.join("live")).unwrap().len() <= 100);
        child.wait().await.unwrap();
        assert!(done.finish().await.is_err());
    }
}
