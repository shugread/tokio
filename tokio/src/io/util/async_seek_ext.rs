use crate::io::seek::{seek, Seek};
use crate::io::AsyncSeek;
use std::io::SeekFrom;

cfg_io_util! {
    /// An extension trait that adds utility methods to [`AsyncSeek`] types.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::io::{self, Cursor, SeekFrom};
    /// use tokio::io::{AsyncSeekExt, AsyncReadExt};
    ///
    /// #[tokio::main]
    /// async fn main() -> io::Result<()> {
    ///     let mut cursor = Cursor::new(b"abcdefg");
    ///
    ///     // the `seek` method is defined by this trait
    ///     cursor.seek(SeekFrom::Start(3)).await?;
    ///
    ///     let mut buf = [0; 1];
    ///     let n = cursor.read(&mut buf).await?;
    ///     assert_eq!(n, 1);
    ///     assert_eq!(buf, [b'd']);
    ///
    ///     Ok(())
    /// }
    /// ```
    ///
    /// See [module][crate::io] documentation for more details.
    ///
    /// [`AsyncSeek`]: AsyncSeek
    /// 拓展[`AsyncSeek`]
    pub trait AsyncSeekExt: AsyncSeek {
        /// Creates a future which will seek an IO object, and then yield the
        /// new position in the object and the object itself.
        ///
        /// Equivalent to:
        ///
        /// ```ignore
        /// async fn seek(&mut self, pos: SeekFrom) -> io::Result<u64>;
        /// ```
        ///
        /// In the case of an error the buffer and the object will be discarded, with
        /// the error yielded.
        ///
        /// # Examples
        ///
        /// ```no_run
        /// use tokio::fs::File;
        /// use tokio::io::{AsyncSeekExt, AsyncReadExt};
        ///
        /// use std::io::SeekFrom;
        ///
        /// # async fn dox() -> std::io::Result<()> {
        /// let mut file = File::open("foo.txt").await?;
        /// file.seek(SeekFrom::Start(6)).await?;
        ///
        /// let mut contents = vec![0u8; 10];
        /// file.read_exact(&mut contents).await?;
        /// # Ok(())
        /// # }
        /// ```
        /// 创建一个未来,它将寻找一个 IO 对象,然后产生对象中的新位置和对象本身.
        fn seek(&mut self, pos: SeekFrom) -> Seek<'_, Self>
        where
            Self: Unpin,
        {
            seek(self, pos)
        }

        /// Creates a future which will rewind to the beginning of the stream.
        ///
        /// This is convenience method, equivalent to `self.seek(SeekFrom::Start(0))`.
        /// 倒回到流开头的未来.
        fn rewind(&mut self) -> Seek<'_, Self>
        where
            Self: Unpin,
        {
            self.seek(SeekFrom::Start(0))
        }

        /// Creates a future which will return the current seek position from the
        /// start of the stream.
        ///
        /// This is equivalent to `self.seek(SeekFrom::Current(0))`.
        /// 返回从流的开始处开始的当前搜索位置
        fn stream_position(&mut self) -> Seek<'_, Self>
        where
            Self: Unpin,
        {
            self.seek(SeekFrom::Current(0))
        }
    }
}

impl<S: AsyncSeek + ?Sized> AsyncSeekExt for S {}
