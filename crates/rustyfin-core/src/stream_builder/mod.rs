//! Stream building: the facts about a media source and the profile conditions that test them.
//! Ports the parts of Jellyfin's `StreamBuilder` and `ConditionProcessor` that the builder needs.

pub mod conditions;
mod facts;

use std::ops::{BitOr, BitOrAssign};

pub use facts::{SourceFacts, StreamFacts};

/// How a stream is delivered. The order matches Jellyfin's values 0, 1 and 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum PlayMethod {
    /// The stream is transcoded.
    #[default]
    Transcode,
    /// The streams are copied into a new container.
    DirectStream,
    /// The file is played as it is.
    DirectPlay,
}

/// Whether an item is video or audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaKind {
    /// A video item.
    #[default]
    Video,
    /// An audio item.
    Audio,
}

/// The kind of a stream in a media source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamKind {
    /// Any other stream, such as an attachment.
    #[default]
    Other,
    /// A video stream.
    Video,
    /// An audio stream.
    Audio,
    /// A subtitle stream.
    Subtitle,
}

/// Why a stream can't be copied as it is. Each reason is one bit, as in Jellyfin's
/// `TranscodeReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum TranscodeReason {
    /// The container isn't supported.
    ContainerNotSupported = 1 << 0,
    /// The video codec isn't supported.
    VideoCodecNotSupported = 1 << 1,
    /// The audio codec isn't supported.
    AudioCodecNotSupported = 1 << 2,
    /// The subtitle codec isn't supported.
    SubtitleCodecNotSupported = 1 << 3,
    /// The audio is in an external file.
    AudioIsExternal = 1 << 4,
    /// The secondary audio track isn't supported.
    SecondaryAudioNotSupported = 1 << 5,
    /// The video profile isn't supported.
    VideoProfileNotSupported = 1 << 6,
    /// The video level isn't supported.
    VideoLevelNotSupported = 1 << 7,
    /// The video resolution isn't supported.
    VideoResolutionNotSupported = 1 << 8,
    /// The video bit depth isn't supported.
    VideoBitDepthNotSupported = 1 << 9,
    /// The video frame rate isn't supported.
    VideoFramerateNotSupported = 1 << 10,
    /// The reference frame count isn't supported.
    RefFramesNotSupported = 1 << 11,
    /// The anamorphic video isn't supported.
    AnamorphicVideoNotSupported = 1 << 12,
    /// The interlaced video isn't supported.
    InterlacedVideoNotSupported = 1 << 13,
    /// The audio channel count isn't supported.
    AudioChannelsNotSupported = 1 << 14,
    /// The audio profile isn't supported.
    AudioProfileNotSupported = 1 << 15,
    /// The audio sample rate isn't supported.
    AudioSampleRateNotSupported = 1 << 16,
    /// The audio bit depth isn't supported.
    AudioBitDepthNotSupported = 1 << 17,
    /// The container bit rate is over the limit.
    ContainerBitrateExceedsLimit = 1 << 18,
    /// The video bit rate isn't supported.
    VideoBitrateNotSupported = 1 << 19,
    /// The audio bit rate isn't supported.
    AudioBitrateNotSupported = 1 << 20,
    /// The video stream's information is unknown.
    UnknownVideoStreamInfo = 1 << 21,
    /// The audio stream's information is unknown.
    UnknownAudioStreamInfo = 1 << 22,
    /// Direct play failed.
    DirectPlayError = 1 << 23,
    /// The video range type isn't supported.
    VideoRangeTypeNotSupported = 1 << 24,
    /// The video codec tag isn't supported.
    VideoCodecTagNotSupported = 1 << 25,
    /// There are more streams than the device allows.
    StreamCountExceedsLimit = 1 << 26,
    /// The video rotation isn't supported.
    VideoRotationNotSupported = 1 << 27,
}

impl TranscodeReason {
    /// Every reason, in ascending bit order.
    pub const ALL: [TranscodeReason; 28] = [
        TranscodeReason::ContainerNotSupported,
        TranscodeReason::VideoCodecNotSupported,
        TranscodeReason::AudioCodecNotSupported,
        TranscodeReason::SubtitleCodecNotSupported,
        TranscodeReason::AudioIsExternal,
        TranscodeReason::SecondaryAudioNotSupported,
        TranscodeReason::VideoProfileNotSupported,
        TranscodeReason::VideoLevelNotSupported,
        TranscodeReason::VideoResolutionNotSupported,
        TranscodeReason::VideoBitDepthNotSupported,
        TranscodeReason::VideoFramerateNotSupported,
        TranscodeReason::RefFramesNotSupported,
        TranscodeReason::AnamorphicVideoNotSupported,
        TranscodeReason::InterlacedVideoNotSupported,
        TranscodeReason::AudioChannelsNotSupported,
        TranscodeReason::AudioProfileNotSupported,
        TranscodeReason::AudioSampleRateNotSupported,
        TranscodeReason::AudioBitDepthNotSupported,
        TranscodeReason::ContainerBitrateExceedsLimit,
        TranscodeReason::VideoBitrateNotSupported,
        TranscodeReason::AudioBitrateNotSupported,
        TranscodeReason::UnknownVideoStreamInfo,
        TranscodeReason::UnknownAudioStreamInfo,
        TranscodeReason::DirectPlayError,
        TranscodeReason::VideoRangeTypeNotSupported,
        TranscodeReason::VideoCodecTagNotSupported,
        TranscodeReason::StreamCountExceedsLimit,
        TranscodeReason::VideoRotationNotSupported,
    ];

    /// Jellyfin's name for the reason, e.g. "AudioCodecNotSupported".
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContainerNotSupported => "ContainerNotSupported",
            Self::VideoCodecNotSupported => "VideoCodecNotSupported",
            Self::AudioCodecNotSupported => "AudioCodecNotSupported",
            Self::SubtitleCodecNotSupported => "SubtitleCodecNotSupported",
            Self::AudioIsExternal => "AudioIsExternal",
            Self::SecondaryAudioNotSupported => "SecondaryAudioNotSupported",
            Self::VideoProfileNotSupported => "VideoProfileNotSupported",
            Self::VideoLevelNotSupported => "VideoLevelNotSupported",
            Self::VideoResolutionNotSupported => "VideoResolutionNotSupported",
            Self::VideoBitDepthNotSupported => "VideoBitDepthNotSupported",
            Self::VideoFramerateNotSupported => "VideoFramerateNotSupported",
            Self::RefFramesNotSupported => "RefFramesNotSupported",
            Self::AnamorphicVideoNotSupported => "AnamorphicVideoNotSupported",
            Self::InterlacedVideoNotSupported => "InterlacedVideoNotSupported",
            Self::AudioChannelsNotSupported => "AudioChannelsNotSupported",
            Self::AudioProfileNotSupported => "AudioProfileNotSupported",
            Self::AudioSampleRateNotSupported => "AudioSampleRateNotSupported",
            Self::AudioBitDepthNotSupported => "AudioBitDepthNotSupported",
            Self::ContainerBitrateExceedsLimit => "ContainerBitrateExceedsLimit",
            Self::VideoBitrateNotSupported => "VideoBitrateNotSupported",
            Self::AudioBitrateNotSupported => "AudioBitrateNotSupported",
            Self::UnknownVideoStreamInfo => "UnknownVideoStreamInfo",
            Self::UnknownAudioStreamInfo => "UnknownAudioStreamInfo",
            Self::DirectPlayError => "DirectPlayError",
            Self::VideoRangeTypeNotSupported => "VideoRangeTypeNotSupported",
            Self::VideoCodecTagNotSupported => "VideoCodecTagNotSupported",
            Self::StreamCountExceedsLimit => "StreamCountExceedsLimit",
            Self::VideoRotationNotSupported => "VideoRotationNotSupported",
        }
    }

    /// The reason's bit in a [`TranscodeReasons`] set.
    pub fn bit(self) -> u32 {
        self as u32
    }
}

/// A set of [`TranscodeReason`]s, one bit each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TranscodeReasons(
    /// The reason bits, one per [`TranscodeReason`].
    pub u32,
);

impl TranscodeReasons {
    /// Adds a reason to the set.
    pub fn insert(&mut self, r: TranscodeReason) {
        self.0 |= r.bit();
    }

    /// Whether the set has the reason.
    pub fn contains(self, r: TranscodeReason) -> bool {
        (self.0 & r.bit()) != 0
    }

    /// Whether the two sets have a reason in common.
    pub fn intersects(self, other: TranscodeReasons) -> bool {
        (self.0 & other.0) != 0
    }

    /// Whether the set has no reasons.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The names of the reasons in the set, in ascending bit order.
    pub fn names(self) -> Vec<&'static str> {
        TranscodeReason::ALL
            .into_iter()
            .filter(|reason| self.contains(*reason))
            .map(TranscodeReason::as_str)
            .collect()
    }
}

impl From<TranscodeReason> for TranscodeReasons {
    fn from(reason: TranscodeReason) -> Self {
        Self(reason.bit())
    }
}

impl BitOr for TranscodeReasons {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOr<TranscodeReason> for TranscodeReasons {
    type Output = Self;

    fn bitor(self, rhs: TranscodeReason) -> Self {
        Self(self.0 | rhs.bit())
    }
}

impl BitOrAssign for TranscodeReasons {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitOrAssign<TranscodeReason> for TranscodeReasons {
    fn bitor_assign(&mut self, rhs: TranscodeReason) {
        self.0 |= rhs.bit();
    }
}
