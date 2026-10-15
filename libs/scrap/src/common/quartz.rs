use crate::{quartz, Frame, Pixfmt};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, TryLockError};
use std::{io, mem};

pub struct Capturer {
    inner: quartz::Capturer,
    frame: Arc<Mutex<Option<quartz::Frame>>>,
    saved_raw_data: Vec<u8>, // for faster compare and copy
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        let frame = Arc::new(Mutex::new(None));
        let display_id = display.0.id();

        let f = frame.clone();
        let inner = quartz::Capturer::new(
            display.0,
            display.width(),
            display.height(),
            quartz::PixelFormat::Argb8888,
            Default::default(),
            move |inner| {
                if let Ok(mut f) = f.lock() {
                    *f = Some(inner);
                }
            },
        )
        .map_err(|_| io::Error::from(io::ErrorKind::Other))?;

        // 定制：采集器建立后立即抓一帧静态画面塞进帧缓冲（bootstrap frame）。
        // CGDisplayStream 只在屏幕内容变化时回调；显示器常亮且桌面静止时
        // （典型场景：控制端最小化恢复触发的采集重建）stream 不会有任何回调，
        // 帧缓冲保持为空，编码循环只能报 "no valid frame"，控制端表现为画面
        // 永远停在最后一帧（假死、点击无反馈）。新连接通常因唤醒屏幕/用户
        // 立即操作而侥幸出帧，恢复重建则必踩此坑。
        // CGDisplayCreateIOSurface 是半私有 API 且新 macOS 已不导出，改走
        // 公开的 CGDisplayCreateImage → DataProvider，提取 BGRA 后构造内存帧。
        // ⚠️ 必须先过 CGPreflightScreenCaptureAccess：无授权时 CGDisplayCreateImage
        // 可能提前触发系统 TCC 弹窗，打乱首页「屏幕录制→辅助功能→输入监控」的
        // 链式权限申请（2026-10-16 实测：差分装机后 ScreenCapture 授权被清、
        // 弹窗被提前消耗，用户看到"只有第一个权限申请"）；且无授权时采集器
        // 本来就拿不到内容，bootstrap 无意义。
        unsafe {
            if quartz::ffi::CGPreflightScreenCaptureAccess() {
                let image = quartz::ffi::CGDisplayCreateImage(display_id);
                if !image.is_null() {
                    let provider = quartz::ffi::CGImageGetDataProvider(image);
                    if !provider.is_null() {
                        let data = quartz::ffi::CGDataProviderCopyData(provider);
                        if !data.is_null() {
                            let ptr = quartz::ffi::CFDataGetBytePtr(data);
                            let len = quartz::ffi::CFDataGetLength(data) as usize;
                            let stride = quartz::ffi::CGImageGetBytesPerRow(image);
                            let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
                            if let Ok(mut g) = frame.lock() {
                                *g = Some(quartz::Frame::from_raw_bgra(bytes, stride));
                            }
                            quartz::ffi::CFRelease(data);
                        }
                    }
                    quartz::ffi::CFRelease(image);
                }
            }
        }

        Ok(Capturer {
            inner,
            frame,
            saved_raw_data: Vec::new(),
        })
    }

    pub fn width(&self) -> usize {
        self.inner.width()
    }

    pub fn height(&self) -> usize {
        self.inner.height()
    }
}

impl crate::TraitCapturer for Capturer {
    fn frame<'a>(&'a mut self, _timeout_ms: std::time::Duration) -> io::Result<Frame<'a>> {
        match self.frame.try_lock() {
            Ok(mut handle) => {
                let mut frame = None;
                mem::swap(&mut frame, &mut handle);

                match frame {
                    Some(mut frame) => {
                        crate::would_block_if_equal(&mut self.saved_raw_data, frame.inner())?;
                        frame.surface_to_bgra(self.height());
                        Ok(Frame::PixelBuffer(PixelBuffer {
                            frame,
                            data: PhantomData,
                            width: self.width(),
                            height: self.height(),
                        }))
                    }

                    None => Err(io::ErrorKind::WouldBlock.into()),
                }
            }

            Err(TryLockError::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),

            Err(TryLockError::Poisoned(..)) => Err(io::ErrorKind::Other.into()),
        }
    }
}

pub struct PixelBuffer<'a> {
    frame: quartz::Frame,
    data: PhantomData<&'a [u8]>,
    width: usize,
    height: usize,
}

impl<'a> crate::TraitPixelBuffer for PixelBuffer<'a> {
    fn data(&self) -> &[u8] {
        &*self.frame
    }

    fn width(&self) -> usize {
        self.width
    }

    fn height(&self) -> usize {
        self.height
    }

    fn stride(&self) -> Vec<usize> {
        let mut v = Vec::new();
        v.push(self.frame.stride());
        v
    }

    fn pixfmt(&self) -> Pixfmt {
        Pixfmt::BGRA
    }
}

pub struct Display(quartz::Display);

impl Display {
    pub fn primary() -> io::Result<Display> {
        Ok(Display(quartz::Display::primary()))
    }

    pub fn all() -> io::Result<Vec<Display>> {
        Ok(quartz::Display::online()
            .map_err(|_| io::Error::from(io::ErrorKind::Other))?
            .into_iter()
            .map(Display)
            .collect())
    }

    pub fn width(&self) -> usize {
        self.0.width()
    }

    pub fn height(&self) -> usize {
        self.0.height()
    }

    pub fn scale(&self) -> f64 {
        self.0.scale()
    }

    pub fn name(&self) -> String {
        self.0.id().to_string()
    }

    pub fn is_online(&self) -> bool {
        self.0.is_online()
    }

    pub fn origin(&self) -> (i32, i32) {
        let o = self.0.bounds().origin;
        (o.x as _, o.y as _)
    }

    pub fn is_primary(&self) -> bool {
        self.0.is_primary()
    }
}
