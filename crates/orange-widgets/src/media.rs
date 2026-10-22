//! 媒体控制组件数据层（§3 B-3：媒体控制 SMTC）。
//!
//! SMTC（System Media Transport Controls）是 WinRT API
//! （`Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager`），
//! 真机阶段接入。本模块定义与平台无关的播放状态模型与控制契约，
//! 并提供 [`StaticMediaSource`]（离线 / 测试源，不假装有数据）。

/// 播放状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackStatus {
    Stopped,
    Playing,
    Paused,
    /// 正在切换曲目（SMTC 的 Changing）
    Changing,
}

/// 当前曲目快照
#[derive(Debug, Clone, PartialEq)]
pub struct MediaTrack {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub status: PlaybackStatus,
}

/// 媒体源契约：读取当前会话状态 + 基础控制命令。
/// 真机实现包装 SMTC（含无会话时 current() = None 的语义）。
pub trait MediaSource: Send {
    /// 当前媒体会话快照；无活跃会话返回 None
    fn current(&mut self) -> Option<MediaTrack>;
    fn play_pause(&mut self) -> Result<(), String>;
    fn next(&mut self) -> Result<(), String>;
    fn previous(&mut self) -> Result<(), String>;
}

/// 离线静态源（测试用：固定曲目与可断言的命令记录）
pub struct StaticMediaSource {
    track: Option<MediaTrack>,
    pub commands: Vec<&'static str>,
}

impl StaticMediaSource {
    pub fn with_track(track: MediaTrack) -> Self {
        Self {
            track: Some(track),
            commands: Vec::new(),
        }
    }

    pub fn empty() -> Self {
        Self {
            track: None,
            commands: Vec::new(),
        }
    }
}

impl MediaSource for StaticMediaSource {
    fn current(&mut self) -> Option<MediaTrack> {
        self.track.clone()
    }

    fn play_pause(&mut self) -> Result<(), String> {
        self.commands.push("play_pause");
        Ok(())
    }

    fn next(&mut self) -> Result<(), String> {
        self.commands.push("next");
        Ok(())
    }

    fn previous(&mut self) -> Result<(), String> {
        self.commands.push("previous");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> MediaTrack {
        MediaTrack {
            title: "Nocturne".into(),
            artist: "Someone".into(),
            album: "Pieces".into(),
            position_ms: 42_000,
            duration_ms: 200_000,
            status: PlaybackStatus::Playing,
        }
    }

    #[test]
    fn static_source_reports_track_and_records_commands() {
        let mut source = StaticMediaSource::with_track(track());
        let current = source.current().unwrap();
        assert_eq!(current.title, "Nocturne");
        assert_eq!(current.status, PlaybackStatus::Playing);

        source.next().unwrap();
        source.play_pause().unwrap();
        source.previous().unwrap();
        assert_eq!(source.commands, vec!["next", "play_pause", "previous"]);
    }

    #[test]
    fn empty_source_reports_no_session() {
        let mut source = StaticMediaSource::empty();
        assert!(
            source.current().is_none(),
            "no active session = None, not fake data"
        );
    }
}
