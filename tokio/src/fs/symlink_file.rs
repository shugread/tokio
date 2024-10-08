use crate::fs::asyncify;

use std::io;
use std::path::Path;

/// Creates a new file symbolic link on the filesystem.
///
/// The `dst` path will be a file symbolic link pointing to the `src`
/// path.
///
/// This is an async version of [`std::os::windows::fs::symlink_file`][std]
///
/// [std]: https://doc.rust-lang.org/std/os/windows/fs/fn.symlink_file.html
/// 在文件系统上创建一个新的文件符号链接.
pub async fn symlink_file(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    let src = src.as_ref().to_owned();
    let dst = dst.as_ref().to_owned();

    asyncify(move || std::os::windows::fs::symlink_file(src, dst)).await
}
