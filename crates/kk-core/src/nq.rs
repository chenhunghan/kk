//! nq file-based queue operations (Protocol §2)
//!
//! Queue convention: each message is a file named `,{timestamp}.{unique_id}.nq`
//! in a queue directory. Writes are atomic (write to .tmp, rename to final name).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// List pending nq files in a queue directory, sorted by filename (FIFO order).
pub fn list_pending(queue_dir: &Path) -> Result<Vec<PathBuf>> {
    if !queue_dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<PathBuf> = std::fs::read_dir(queue_dir)
        .with_context(|| format!("failed to read queue dir: {}", queue_dir.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name();
            let name_str = name.to_str()?;
            if name_str.starts_with(',') && name_str.ends_with(".nq") {
                Some(entry.path())
            } else {
                None
            }
        })
        .collect();

    entries.sort();
    Ok(entries)
}

/// Atomically write a message to a queue directory.
/// Returns the path of the created nq file.
pub fn enqueue(queue_dir: &Path, timestamp: u64, payload: &[u8]) -> Result<PathBuf> {
    std::fs::create_dir_all(queue_dir)
        .with_context(|| format!("failed to create queue dir: {}", queue_dir.display()))?;

    let unique_id = &Uuid::new_v4().to_string()[..8];
    let tmp_name = format!(".tmp-{unique_id}");
    let nq_name = format!(",{timestamp}.{unique_id}.nq");

    let tmp_path = queue_dir.join(&tmp_name);
    let nq_path = queue_dir.join(&nq_name);

    // Write to temp file
    std::fs::write(&tmp_path, payload)
        .with_context(|| format!("failed to write temp file: {}", tmp_path.display()))?;

    // Atomic rename
    std::fs::rename(&tmp_path, &nq_path).with_context(|| {
        format!(
            "failed to rename {} -> {}",
            tmp_path.display(),
            nq_path.display()
        )
    })?;

    Ok(nq_path)
}

/// Read and parse a nq file's contents.
pub fn read_message(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("failed to read nq file: {}", path.display()))
}

/// Delete a processed nq file.
pub fn delete(path: &Path) -> Result<()> {
    std::fs::remove_file(path)
        .with_context(|| format!("failed to delete nq file: {}", path.display()))
}

/// Get the age of a file in seconds.
pub fn file_age_secs(path: &Path) -> Result<u64> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("failed to stat file: {}", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| "failed to get modified time")?;
    let age = modified.elapsed().unwrap_or_default();
    Ok(age.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_enqueue_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let queue = dir.path().join("test-queue");

        // Empty queue
        assert!(list_pending(&queue).unwrap().is_empty());

        // Enqueue two messages
        let path1 = enqueue(&queue, 1000, b"msg1").unwrap();
        let path2 = enqueue(&queue, 1001, b"msg2").unwrap();

        let pending = list_pending(&queue).unwrap();
        assert_eq!(pending.len(), 2);
        // Should be sorted by timestamp (FIFO)
        assert_eq!(pending[0], path1);
        assert_eq!(pending[1], path2);

        // Read contents
        assert_eq!(read_message(&path1).unwrap(), b"msg1");
        assert_eq!(read_message(&path2).unwrap(), b"msg2");

        // Delete first
        delete(&path1).unwrap();
        assert_eq!(list_pending(&queue).unwrap().len(), 1);
    }

    #[test]
    fn test_no_tmp_files_visible() {
        let dir = tempfile::tempdir().unwrap();
        let queue = dir.path().join("queue");
        fs::create_dir_all(&queue).unwrap();

        // Write a .tmp file manually
        fs::write(queue.join(".tmp-abc123"), b"partial").unwrap();

        // Should not appear in listing
        assert!(list_pending(&queue).unwrap().is_empty());
    }
}
