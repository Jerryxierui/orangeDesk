//! WASAPI loopback 采集（§13 F4，cfg windows）。
//!
//! 抓取默认输出设备的混音（系统正在播放的一切声音），事件驱动等待
//! （AUDCLNT_STREAMFLAGS_EVENTCALLBACK，零轮询，P4），累积到分析窗口后
//! 交给回调。支持混音格式 float32 / int16 交错；其他格式报错降级
//! （模块明确提示而非静默无数据，P3）。

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::analysis::ANALYSIS_SAMPLES;

const KSDATAFORMAT_SUBTYPE_PCM: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// 采集会话（own 线程内使用；COM MTA 初始化由调用方负责）
pub struct LoopbackCapture {
    client: IAudioClient,
    #[allow(dead_code)] // 持有引用保证事件存活至 client 释放
    event: HANDLE,
    capture: IAudioCaptureClient,
    channels: usize,
    is_float: bool,
    accumulated: Vec<f32>,
}

impl LoopbackCapture {
    /// 打开默认输出设备的 loopback 会话（共享模式 + 事件回调）
    pub fn open() -> Result<Self, String> {
        unsafe {
            let enumerator: windows::Win32::Media::Audio::IMMDeviceEnumerator =
                windows::Win32::System::Com::CoCreateInstance(
                    &MMDeviceEnumerator,
                    None,
                    windows::Win32::System::Com::CLSCTX_ALL,
                )
                .map_err(|err| format!("MMDeviceEnumerator create: {err}"))?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|err| format!("GetDefaultAudioEndpoint: {err}"))?;
            let client: IAudioClient = device
                .Activate::<IAudioClient>(windows::Win32::System::Com::CLSCTX_ALL, None)
                .map_err(|err| format!("IMMDevice::Activate: {err}"))?;

            // 混音格式（共享模式必须使用）
            let format_ptr = client
                .GetMixFormat()
                .map_err(|err| format!("GetMixFormat: {err}"))?;
            let format = &*format_ptr;
            let (channels, is_float) = inspect_format(format).ok_or_else(|| {
                "unsupported mix format (need 16-bit PCM or 32-bit float)".to_string()
            })?;

            // 1 秒缓冲 + 事件回调
            let buffer_hns: i64 = 10_000_000;
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    buffer_hns,
                    0,
                    format,
                    None,
                )
                .map_err(|err| format!("IAudioClient::Initialize: {err}"))?;
            let event = CreateEventW(None, false, false, None)
                .map_err(|err| format!("CreateEventW: {err}"))?;
            client
                .SetEventHandle(event)
                .map_err(|err| format!("SetEventHandle: {err}"))?;
            let capture: IAudioCaptureClient = client
                .GetService()
                .map_err(|err| format!("GetService(IAudioCaptureClient): {err}"))?;
            client
                .Start()
                .map_err(|err| format!("IAudioClient::Start: {err}"))?;
            windows::Win32::System::Com::CoTaskMemFree(Some(format_ptr.cast()));

            Ok(Self {
                client,
                event,
                capture,
                channels,
                is_float,
                accumulated: Vec::with_capacity(ANALYSIS_SAMPLES * 2),
            })
        }
    }

    /// 阻塞等待下一批样本（事件驱动；超时安静期正常返回），
    /// 累积满分析窗口时调用 `on_window`（mono f32）。
    pub fn pump(
        &mut self,
        timeout_ms: u32,
        on_window: &mut dyn FnMut(&[f32]),
    ) -> Result<(), String> {
        unsafe {
            let wait = WaitForSingleObject(self.event, timeout_ms);
            if wait != WAIT_OBJECT_0 {
                return Ok(()); // 超时（安静期）：无数据正常
            }
            loop {
                let packet_frames = match self.capture.GetNextPacketSize() {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(err) => return Err(format!("GetNextPacketSize: {err}")),
                };
                if packet_frames == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if let Err(err) =
                    self.capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                {
                    return Err(format!("GetBuffer: {err}"));
                }
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 && frames > 0 {
                    self.absorb(data, frames as usize);
                }
                let _ = self.capture.ReleaseBuffer(frames);
            }
            if self.accumulated.len() >= ANALYSIS_SAMPLES {
                let window: Vec<f32> = self.accumulated.drain(..ANALYSIS_SAMPLES).collect();
                on_window(&window);
            }
            Ok(())
        }
    }

    /// 停止采集（COM 对象释放前调用）
    pub fn stop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }

    unsafe fn absorb(&mut self, data: *mut u8, frames: usize) {
        let channels = self.channels.max(1);
        let sample_count = frames * channels;
        if self.is_float {
            let samples = std::slice::from_raw_parts(data as *const f32, sample_count);
            for frame in samples.chunks(channels) {
                self.accumulated
                    .push(frame.iter().sum::<f32>() / channels as f32);
            }
        } else {
            let samples = std::slice::from_raw_parts(data as *const i16, sample_count);
            for frame in samples.chunks(channels) {
                let mono = frame.iter().map(|s| *s as f32 / 32768.0).sum::<f32>() / channels as f32;
                self.accumulated.push(mono);
            }
        }
        // 防御：极端积压时丢弃旧样本（不无限增长）
        if self.accumulated.len() > ANALYSIS_SAMPLES * 4 {
            let excess = self.accumulated.len() - ANALYSIS_SAMPLES;
            self.accumulated.drain(..excess);
        }
    }
}

/// 解析混音格式：返回 (声道数, 是否 float32)；不支持的格式 None
fn inspect_format(format: &WAVEFORMATEX) -> Option<(usize, bool)> {
    let channels = format.nChannels as usize;
    const WAVE_FORMAT_PCM: u16 = 1;
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    match format.wFormatTag {
        WAVE_FORMAT_IEEE_FLOAT if format.wBitsPerSample == 32 => Some((channels, true)),
        WAVE_FORMAT_PCM if format.wBitsPerSample == 16 => Some((channels, false)),
        WAVE_FORMAT_EXTENSIBLE => {
            // WAVEFORMATEXTENSIBLE 是 packed：字段先 copy 到局部再比较（避免未对齐引用）
            let ext = unsafe { &*(format as *const WAVEFORMATEX as *const WAVEFORMATEXTENSIBLE) };
            let sub_format = { ext.SubFormat };
            if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && format.wBitsPerSample == 32 {
                Some((channels, true))
            } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM && format.wBitsPerSample == 16 {
                Some((channels, false))
            } else {
                None
            }
        }
        _ => None,
    }
}
