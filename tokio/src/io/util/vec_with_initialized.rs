use crate::io::ReadBuf;
use std::mem::MaybeUninit;

/// Something that looks like a `Vec<u8>`.
/// 看起来像 `Vec<u8>`
///
/// # Safety
///
/// The implementor must guarantee that the vector returned by the
/// `as_mut` and `as_mut` methods do not change from one call to
/// another.
pub(crate) unsafe trait VecU8: AsRef<Vec<u8>> + AsMut<Vec<u8>> {}

unsafe impl VecU8 for Vec<u8> {}
unsafe impl VecU8 for &mut Vec<u8> {}

/// This struct wraps a `Vec<u8>` or `&mut Vec<u8>`, combining it with a
/// `num_initialized`, which keeps track of the number of initialized bytes
/// in the unused capacity.
///
/// The purpose of this struct is to remember how many bytes were initialized
/// through a `ReadBuf` from call to call.
///
/// This struct has the safety invariant that the first `num_initialized` of the
/// vector's allocation must be initialized at any time.
/// 该结构包装了`Vec<u8>`或`&mut Vec<u8>`,并将其与`num_initialized`相结合,以跟踪未使用容量中已初始化字节的数量.
#[derive(Debug)]
pub(crate) struct VecWithInitialized<V> {
    vec: V,
    // The number of initialized bytes in the vector.
    // Always between `vec.len()` and `vec.capacity()`.
    // 向量中初始化字节的数量。始终介于 `vec.len()` 和 `vec.capacity()` 之间.
    num_initialized: usize,
    starting_capacity: usize,
}

impl VecWithInitialized<Vec<u8>> {
    #[cfg(feature = "io-util")]
    pub(crate) fn take(&mut self) -> Vec<u8> {
        self.num_initialized = 0;
        std::mem::take(&mut self.vec)
    }
}

impl<V> VecWithInitialized<V>
where
    V: VecU8,
{
    pub(crate) fn new(mut vec: V) -> Self {
        // SAFETY: The safety invariants of vector guarantee that the bytes up
        // to its length are initialized.
        Self {
            num_initialized: vec.as_mut().len(),
            starting_capacity: vec.as_ref().capacity(),
            vec,
        }
    }

    pub(crate) fn reserve(&mut self, num_bytes: usize) {
        let vec = self.vec.as_mut();
        // 容量足够
        if vec.capacity() - vec.len() >= num_bytes {
            return;
        }
        // SAFETY: Setting num_initialized to `vec.len()` is correct as
        // `reserve` does not change the length of the vector.
        self.num_initialized = vec.len();
        vec.reserve(num_bytes);
    }

    #[cfg(feature = "io-util")]
    pub(crate) fn is_empty(&self) -> bool {
        self.vec.as_ref().is_empty()
    }

    pub(crate) fn get_read_buf<'a>(&'a mut self) -> ReadBuf<'a> {
        let num_initialized = self.num_initialized;

        // SAFETY: Creating the slice is safe because of the safety invariants
        // on Vec<u8>. The safety invariants of `ReadBuf` will further guarantee
        // that no bytes in the slice are de-initialized.
        let vec = self.vec.as_mut();
        let len = vec.len();
        let cap = vec.capacity();
        let ptr = vec.as_mut_ptr().cast::<MaybeUninit<u8>>();
        let slice = unsafe { std::slice::from_raw_parts_mut::<'a, MaybeUninit<u8>>(ptr, cap) };

        // SAFETY: This is safe because the safety invariants of
        // VecWithInitialized say that the first num_initialized bytes must be
        // initialized.
        let mut read_buf = ReadBuf::uninit(slice);
        unsafe {
            // 修改已经初始化的长度
            read_buf.assume_init(num_initialized);
        }
        // 已填充的长度
        read_buf.set_filled(len);

        read_buf
    }

    // 应用
    pub(crate) fn apply_read_buf(&mut self, parts: ReadBufParts) {
        let vec = self.vec.as_mut();
        assert_eq!(vec.as_ptr(), parts.ptr);

        // SAFETY:
        // The ReadBufParts really does point inside `self.vec` due to the above
        // check, and the safety invariants of `ReadBuf` guarantee that the
        // first `parts.initialized` bytes of `self.vec` really have been
        // initialized. Additionally, `ReadBuf` guarantees that `parts.len` is
        // at most `parts.initialized`, so the first `parts.len` bytes are also
        // initialized.
        //
        // Note that this relies on the fact that `V` is either `Vec<u8>` or
        // `&mut Vec<u8>`, so the vector returned by `self.vec.as_mut()` cannot
        // change from call to call.
        unsafe {
            self.num_initialized = parts.initialized;
            vec.set_len(parts.len);
        }
    }

    // Returns a boolean telling the caller to try reading into a small local buffer first if true.
    // Doing so would avoid overallocating when vec is filled to capacity and we reached EOF.
    // 如果为真,则返回一个布尔值,告知调用者首先尝试读取一个小的本地缓冲区.这样做可以避免在 vec 已满且达到 EOF 时过度分配.
    pub(crate) fn try_small_read_first(&self, num_bytes: usize) -> bool {
        let vec = self.vec.as_ref();
        vec.capacity() - vec.len() < num_bytes
            && self.starting_capacity == vec.capacity()
            && self.starting_capacity >= num_bytes
    }
}

pub(crate) struct ReadBufParts {
    // Pointer is only used to check that the ReadBuf actually came from the
    // right VecWithInitialized.
    //  指针仅用于检查 ReadBuf 是否真正来自
    // 正确的 VecWithInitialized.
    ptr: *const u8,
    len: usize,
    initialized: usize,
}

// This is needed to release the borrow on `VecWithInitialized<V>`.
pub(crate) fn into_read_buf_parts(rb: ReadBuf<'_>) -> ReadBufParts {
    ReadBufParts {
        ptr: rb.filled().as_ptr(),
        len: rb.filled().len(),
        initialized: rb.initialized().len(),
    }
}
