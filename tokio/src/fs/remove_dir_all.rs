use crate::fs::asyncify;

use std::io;
use std::path::Path;

/// Removes a directory at this path, after removing all its contents. Use carefully!
///
/// This is an async version of [`std::fs::remove_dir_all`][std]
///
/// [std]: fn@std::fs::remove_dir_all
///  删除此路径上的目录,然后删除其所有内容.请谨慎使用.
pub async fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref().to_owned();
    asyncify(move || std::fs::remove_dir_all(path)).await
}
