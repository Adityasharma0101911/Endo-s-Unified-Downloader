pub mod range;
pub mod chunk;
pub mod mirror;
pub mod storage;
pub mod state;
pub mod worker;
pub mod engine;
pub mod resolver;
pub mod hls;
pub mod metalink;
pub mod torrent;
pub mod queue;
pub mod media;
pub mod history;
pub mod verify;

pub use range::{ByteRange, RangeError};
pub use chunk::{Chunk, ChunkManager, ChunkStatus, ChunkError};
pub use mirror::{Mirror, MirrorRacer};
pub use storage::{DiskWriter, StorageError};
pub use state::{DownloadState, StateError};
pub use engine::{DownloadEngine, DownloadOptions, EngineSnapshot};
pub use resolver::SmartResolver;
pub use hls::{HlsEngine, HlsSegment, HlsError, parse_hls_playlist};
pub use metalink::{parse_metalink, MetalinkFile};
pub use torrent::{parse_magnet_uri, parse_torrent_bytes, is_magnet_uri, MagnetInfo, TorrentInfo};
pub use queue::{DownloadQueue, QueueItem, QueueItemStatus};
pub use media::{
    MediaQualityPreset, BrowserCookieSource, MediaDownloadOptions,
    is_supported_media_site, download_media, find_ytdlp_path, find_js_runtime, find_ffmpeg_path,
};
pub use history::{DownloadHistoryManager, HistoryEntry, HistoryStatus};
pub use verify::{BuildVerificationResult, verify_build_file, repair_missing_ranges};

