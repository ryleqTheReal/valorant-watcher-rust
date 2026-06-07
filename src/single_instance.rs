use std::fs::OpenOptions;

use fs2::FileExt;

use crate::error::Result;
use crate::paths;

pub struct InstanceLock {
    _file: std::fs::File,
}

// returns Some(lock) if this is the only instance, None if another holds it.
// the lock is released automatically when the process exits.
pub fn acquire() -> Result<Option<InstanceLock>> {
    let dir = paths::data_dir()?;
    std::fs::create_dir_all(&dir)?;

    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join("instance.lock"))?;

    if file.try_lock_exclusive().is_err() {
        return Ok(None);
    }
    Ok(Some(InstanceLock { _file: file }))
}
