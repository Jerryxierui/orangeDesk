//! 视频壁纸管线（§3 A-1 / A-2）：MF SourceReader 硬解 → GPU VideoProcessor blit。
//!
//! 纪律（A-1）：
//! - 解码输出（DXVA NV12 纹理）全程留在 GPU；`VideoProcessorBlt` 直写
//!   flip-model 后缓冲的 output view —— 任何时刻不发生 CPU 回读 / CPU 回写
//! - 循环播放：EOF 后重建 SourceReader（Seek 在 SourceReader 上精度不可靠；
//!   循环重播间隔通常是分钟级，重建开销可忽略）
//! - 壁纸视频静音：只选择视频流，音频流一律不选
//! - 显存主动回收由 renderer 的暂停路径负责（IDXGIDevice3::Trim）
//!
//! v0.1 已知留白（真机验证后补）：色彩空间显式设置
//! （`VideoProcessorSetStreamColorSpace1`，NV12 的 601/709 判定依赖容器元数据）。

use windows::core::{Interface, PCWSTR};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain1;
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFSourceReader, MFCreateAttributes,
    MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video,
    MFShutdown, MFStartup, MFVideoFormat_NV12, MFSTARTUP_LITE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_D3D_MANAGER,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
};

/// 视频流索引（SourceReader 特殊常量：第一个视频流）
const VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// 视频解码 + 处理管线（一次 `blt_next_frame` = 解码一帧 + VP blt）
pub struct VideoPipeline {
    reader: IMFSourceReader,
    /// 打开的文件路径（EOF 重建 reader 用）
    path: String,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// VPBlt 递增输出帧号
    output_frame: u32,
    frame_w: u32,
    frame_h: u32,
}

impl VideoPipeline {
    /// 初始化管线：MF 启动、DXVA manager 绑定壁纸 D3D 设备、打开文件、
    /// 只选视频流（静音）、目标类型 NV12、建 VideoProcessor。
    ///
    /// `screen_w/h` 为输出尺寸（VP 内完成 cover 拉伸由调用方给出的 desc 决定；
    /// v0.1 输出 = 屏幕尺寸拉伸，宽高比保持靠 VP 默认行为，真机验证后细化）。
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        path: &str,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<Self, String> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_LITE)
                .map_err(|err| format!("MFStartup failed: {err}"))?;

            let (reader, frame_w, frame_h) = open_reader(device, path)?;

            let video_device: ID3D11VideoDevice = device
                .cast()
                .map_err(|err| format!("cast ID3D11VideoDevice failed: {err}"))?;
            let video_context: ID3D11VideoContext = context
                .cast()
                .map_err(|err| format!("cast ID3D11VideoContext failed: {err}"))?;

            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 30,
                    Denominator: 1,
                },
                InputWidth: frame_w,
                InputHeight: frame_h,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                OutputWidth: screen_w,
                OutputHeight: screen_h,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&desc)
                .map_err(|err| format!("CreateVideoProcessorEnumerator failed: {err}"))?;
            let processor = video_device
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(|err| format!("CreateVideoProcessor failed: {err}"))?;
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );

            Ok(Self {
                reader,
                path: path.to_string(),
                video_device,
                video_context,
                enumerator,
                processor,
                output_frame: 0,
                frame_w,
                frame_h,
            })
        }
    }

    /// 解码并 blt 下一帧到交换链后缓冲。
    ///
    /// 返回：
    /// - `Ok(true)`：一帧已写入后缓冲，调用方负责 `Present`
    /// - `Ok(false)`：EOF，调用方应 [`VideoPipeline::restart`]
    /// - `Err`：不可恢复错误，调用方降级纯色
    pub fn blt_next_frame(&mut self, swapchain: &IDXGISwapChain1) -> Result<bool, String> {
        unsafe {
            let mut stream_flags = 0u32;
            let mut sample_opt: Option<windows::Win32::Media::MediaFoundation::IMFSample> = None;
            self.reader
                .ReadSample(
                    VIDEO_STREAM,
                    0,
                    None,
                    Some(&mut stream_flags),
                    None,
                    Some(&mut sample_opt),
                )
                .map_err(|err| format!("ReadSample failed: {err}"))?;

            if stream_flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                return Err("source reader reported error".to_string());
            }
            if stream_flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Ok(false);
            }
            let Some(sample) = sample_opt else {
                // 无 EOF 标志也无样本：视为 EOF（部分容器的尾部行为）
                return Ok(false);
            };

            let buffer = sample
                .GetBufferByIndex(0)
                .map_err(|err| format!("GetBufferByIndex failed: {err}"))?;
            let dxgi_buffer: IMFDXGIBuffer = buffer.cast().map_err(|err| {
                format!("buffer cast IMFDXGIBuffer failed (CPU-side frame? DXVA not active): {err}")
            })?;
            let mut tex_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            dxgi_buffer
                .GetResource(&ID3D11Texture2D::IID, &mut tex_ptr)
                .map_err(|err| format!("GetResource failed: {err}"))?;
            let texture: ID3D11Texture2D = std::mem::transmute(tex_ptr);
            let array_slice = dxgi_buffer
                .GetSubresourceIndex()
                .map_err(|err| format!("GetSubresourceIndex failed: {err}"))?;

            // 输入视图：DXVA 输出是 texture array，每帧所在 slice 不同
            let iv_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: array_slice,
                    },
                },
            };
            let mut input_view_opt: Option<ID3D11VideoProcessorInputView> = None;
            self.video_device
                .CreateVideoProcessorInputView(
                    &texture,
                    &self.enumerator,
                    &iv_desc,
                    Some(&mut input_view_opt),
                )
                .map_err(|err| format!("CreateVideoProcessorInputView failed: {err}"))?;
            let input_view = input_view_opt.ok_or("input view out-param missing")?;

            // 输出视图：flip-model 下每帧的后缓冲由 Present 轮换，
            // view 不能缓存，每帧在当前 buffer 上重建（开销可忽略）
            let back_buffer: ID3D11Texture2D = swapchain
                .GetBuffer(0)
                .map_err(|err| format!("GetBuffer failed: {err}"))?;
            let ov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut output_view_opt: Option<ID3D11VideoProcessorOutputView> = None;
            self.video_device
                .CreateVideoProcessorOutputView(
                    &back_buffer,
                    &self.enumerator,
                    &ov_desc,
                    Some(&mut output_view_opt),
                )
                .map_err(|err| format!("CreateVideoProcessorOutputView failed: {err}"))?;
            let output_view = output_view_opt.ok_or("output view out-param missing")?;

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: windows::core::BOOL::from(true),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };
            self.video_context
                .VideoProcessorBlt(&self.processor, &output_view, self.output_frame, &[stream])
                .map_err(|err| format!("VideoProcessorBlt failed: {err}"))?;
            self.output_frame = self.output_frame.wrapping_add(1);
            Ok(true)
        }
    }

    /// EOF 后循环重播：重建 SourceReader（Seek 不可靠，见模块注释）
    pub fn restart(&mut self, device: &ID3D11Device) -> Result<(), String> {
        let (reader, frame_w, frame_h) = unsafe { open_reader(device, &self.path) }?;
        if (frame_w, frame_h) != (self.frame_w, self.frame_h) {
            return Err(format!(
                "video size changed after loop restart ({frame_w}x{frame_h} vs {}x{})",
                self.frame_w, self.frame_h
            ));
        }
        self.reader = reader;
        Ok(())
    }

    /// MF 全局关闭（渲染线程退出时调用一次，与 MFStartup 配对）
    pub fn shutdown_mf() {
        unsafe {
            let _ = MFShutdown();
        }
    }

    #[allow(dead_code)]
    pub fn frame_size(&self) -> (u32, u32) {
        (self.frame_w, self.frame_h)
    }
}

/// 打开（或重开）SourceReader：DXVA manager + 硬件变换 + NV12 目标类型。
/// 返回 (reader, 宽, 高)。
unsafe fn open_reader(
    device: &ID3D11Device,
    path: &str,
) -> Result<(IMFSourceReader, u32, u32), String> {
    unsafe {
        // DXVA manager：解码器共享壁纸的 D3D 设备（解码纹理与 VP 输入同设备，零拷贝）
        let mut reset_token = 0u32;
        let mut manager_opt: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut reset_token, &mut manager_opt)
            .map_err(|err| format!("MFCreateDXGIDeviceManager failed: {err}"))?;
        let manager = manager_opt.ok_or("device manager out-param missing")?;
        manager
            .ResetDevice(device, reset_token)
            .map_err(|err| format!("ResetDevice failed: {err}"))?;

        let mut attrs_opt: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs_opt, 2)
            .map_err(|err| format!("MFCreateAttributes failed: {err}"))?;
        let attrs = attrs_opt.ok_or("attributes out-param missing")?;
        attrs
            .SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)
            .map_err(|err| format!("SetUnknown(D3D manager) failed: {err}"))?;
        attrs
            .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
            .map_err(|err| format!("SetUINT32(hw transforms) failed: {err}"))?;

        let wide: Vec<u16> = format!("{path}\0").encode_utf16().collect();
        let reader = MFCreateSourceReaderFromURL(PCWSTR(wide.as_ptr()), &attrs)
            .map_err(|err| format!("cannot open video '{path}': {err}"))?;

        // 壁纸静音：全流取消选择后仅保留第一个视频流
        reader
            .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
            .map_err(|err| format!("SetStreamSelection(all, off) failed: {err}"))?;
        reader
            .SetStreamSelection(VIDEO_STREAM, true)
            .map_err(|err| format!("SetStreamSelection(video, on) failed: {err}"))?;

        // 目标类型 NV12：硬解原生输出，VideoProcessor 原生输入（零 CPU 转换）
        let media_type =
            MFCreateMediaType().map_err(|err| format!("MFCreateMediaType failed: {err}"))?;
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|err| format!("SetGUID(major type) failed: {err}"))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(|err| format!("SetGUID(subtype NV12) failed: {err}"))?;
        reader
            .SetCurrentMediaType(VIDEO_STREAM, None, &media_type)
            .map_err(|err| format!("SetCurrentMediaType(NV12) failed: {err}"))?;

        let current = reader
            .GetCurrentMediaType(VIDEO_STREAM)
            .map_err(|err| format!("GetCurrentMediaType failed: {err}"))?;
        let packed = current
            .GetUINT64(&MF_MT_FRAME_SIZE)
            .map_err(|err| format!("GetUINT64(frame size) failed: {err}"))?;
        let frame_h = (packed >> 32) as u32;
        let frame_w = (packed & 0xffff_ffff) as u32;
        if frame_w == 0 || frame_h == 0 {
            return Err(format!(
                "video '{path}' reports invalid size {frame_w}x{frame_h}"
            ));
        }
        Ok((reader, frame_w, frame_h))
    }
}
