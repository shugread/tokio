use crate::fs::asyncify;

use std::io;
use std::path::Path;

/// Renames a file or directory to a new name, replacing the original file if
/// `to` already exists.
///
/// This will not work if the new name is on a different mount point.
///
/// This is an async version of [`std::fs::rename`].
/// 将文件或目录重命名为新名称,如果`to`已存在,则替换原始文件.
pub async fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let from = from.as_ref().to_owned();
    let to = to.as_ref().to_owned();

    asyncify(move || std::fs::rename(from, to)).await
}
