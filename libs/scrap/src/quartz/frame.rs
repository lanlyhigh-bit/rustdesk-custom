use std::{ops, ptr, slice};

use super::ffi::*;

pub struct Frame {
    surface: IOSurfaceRef,
    inner: &'static [u8],
    bgra: Vec<u8>,
    bgra_stride: usize,
}

impl Frame {
    pub unsafe fn new(surface: IOSurfaceRef) -> Frame {
        CFRetain(surface);
        IOSurfaceIncrementUseCount(surface);

        IOSurfaceLock(surface, SURFACE_LOCK_READ_ONLY, ptr::null_mut());

        let inner = slice::from_raw_parts(
            IOSurfaceGetBaseAddress(surface) as *const u8,
            IOSurfaceGetAllocSize(surface),
        );

        Frame {
            surface,
            inner,
            bgra: Vec::new(),
            bgra_stride: 0,
        }
    }

    #[inline]
    pub fn inner(&self) -> &[u8] {
        // 定制：bootstrap 帧没有 IOSurface，用已转换的 bgra 参与变化检测，
        // 否则空 inner 会被 would_block_if_equal 判为"与上次相同"而吞掉首帧。
        if self.surface.is_null() {
            return &self.bgra;
        }
        self.inner
    }

    pub fn stride(&self) -> usize {
        self.bgra_stride
    }

    pub fn surface_to_bgra<'a>(&'a mut self, h: usize) {
        unsafe {
            if self.surface.is_null() {
                return;
            }
            let plane0 = IOSurfaceGetBaseAddressOfPlane(self.surface, 0);
            self.bgra_stride = IOSurfaceGetBytesPerRowOfPlane(self.surface, 0);
            self.bgra.resize(self.bgra_stride * h, 0);
            std::ptr::copy_nonoverlapping(
                plane0 as _,
                self.bgra.as_mut_ptr(),
                self.bgra_stride * h,
            );
        }
    }
}

impl ops::Deref for Frame {
    type Target = [u8];
    fn deref<'a>(&'a self) -> &'a [u8] {
        &self.bgra
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe {
            if self.surface.is_null() {
                return;
            }
            IOSurfaceUnlock(self.surface, SURFACE_LOCK_READ_ONLY, ptr::null_mut());

            IOSurfaceDecrementUseCount(self.surface);
            CFRelease(self.surface);
        }
    }
}

impl Frame {
    // 定制：从纯内存像素构造的 bootstrap 帧（surface 为 null）。数据来自
    // CGDisplayCreateImage 的 DataProvider 拷贝，已是 BGRA，无需再转换。
    pub fn from_raw_bgra(data: Vec<u8>, stride: usize) -> Frame {
        Frame {
            surface: ptr::null_mut(),
            inner: &[],
            bgra: data,
            bgra_stride: stride,
        }
    }
}
