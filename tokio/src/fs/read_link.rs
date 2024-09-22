use crate::fs::asyncify;

use std::io;
use std::path::{Path, PathBuf};

/// Reads a symbolic link, returning the file that the link points to.
///
/// This is an async version of [`std::fs::read_link`].
/// 读取符号链接,返回该链接指向的文件.
pub async fn read_link(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let path = path.as_ref().to_owned();
    asyncify(move || std::fs::read_link(path)).await
}
