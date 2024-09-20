use std::fmt;

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Pack {
    mask: usize,
    shift: u32,
}

impl Pack {
    /// Value is packed in the `width` least-significant bits.
    /// 值被打包在`width`的最低有效位中.
    pub(crate) const fn least_significant(width: u32) -> Pack {
        let mask = mask_for(width);

        Pack { mask, shift: 0 }
    }

    /// Value is packed in the `width` more-significant bits.
    /// 值被打包在`width`的更高有效位中.
    pub(crate) const fn then(&self, width: u32) -> Pack {
        let shift = usize::BITS - self.mask.leading_zeros();
        let mask = mask_for(width) << shift;

        Pack { mask, shift }
    }

    /// Width, in bits, dedicated to storing the value.
    /// 宽度(以位为单位)用于存储值.
    pub(crate) const fn width(&self) -> u32 {
        usize::BITS - (self.mask >> self.shift).leading_zeros()
    }

    /// Max representable value.
    /// 最大可表示值.
    pub(crate) const fn max_value(&self) -> usize {
        (1 << self.width()) - 1
    }

    // 打包成一个值
    pub(crate) fn pack(&self, value: usize, base: usize) -> usize {
        assert!(value <= self.max_value());
        (base & !self.mask) | (value << self.shift)
    }

    // 获取表示部分的值
    pub(crate) fn unpack(&self, src: usize) -> usize {
        unpack(src, self.mask, self.shift)
    }
}

impl fmt::Debug for Pack {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            fmt,
            "Pack {{ mask: {:b}, shift: {} }}",
            self.mask, self.shift
        )
    }
}

/// Returns a `usize` with the right-most `n` bits set.
/// 返回设置了最右边`n`位的`usize`.
pub(crate) const fn mask_for(n: u32) -> usize {
    let shift = 1usize.wrapping_shl(n - 1);
    shift | (shift - 1)
}

/// Unpacks a value using a mask & shift.
pub(crate) const fn unpack(src: usize, mask: usize, shift: u32) -> usize {
    (src & mask) >> shift
}
