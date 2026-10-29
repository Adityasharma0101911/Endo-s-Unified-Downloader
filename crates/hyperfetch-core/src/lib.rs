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
pub mod hosts;
pub mod ingest;
pub mod folders;
pub mod feeds;
pub mod updater;
pub mod p2p;
pub mod debrid;
pub mod netif;
pub mod mega;
pub mod shares;
pub mod cloud;
pub mod repos;
pub mod gallery;
pub mod postprocess;
pub mod paste;
pub mod ftp;
pub mod digest;
pub mod zipdir;
pub mod sniff;
pub mod stream_spec;
pub mod stream_filter;
pub mod custom_range;
pub mod dash;
pub mod mss;
pub mod iptv;
pub mod decrypt;
pub mod concat_server;
pub mod save_pattern;
pub mod subtitle_fix;
pub mod muxer;
pub mod config_file;
pub mod doctor;

pub use range::{ByteRange, RangeError};
pub use chunk::{Chunk, ChunkManager, ChunkStatus, ChunkError};
pub use mirror::{Mirror, MirrorRacer};
pub use storage::{DiskWriter, StorageError};
pub use state::{DownloadState, StateError};
pub use engine::{
    build_client, claim_target, discard_partial, stream_to, ClientKey, DownloadEngine, DownloadOptions, EngineSnapshot, IpFamily,
    SharedLimits, TargetClaim,
};
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
pub use hosts::{HostKey, HostProfile};

pub use stream_spec::{StreamSpec, MediaSegment, MediaType, DrmKeyInfo};
pub use stream_filter::{StreamFilter, ForTarget, filter_keep, filter_drop, auto_select};
pub use custom_range::CustomRange;
pub use dash::parse_dash_manifest;
pub use mss::parse_mss_manifest;
pub use iptv::{is_iptv_playlist, parse_iptv_playlist};
pub use decrypt::{DecryptionConfig, DecryptEngine, CustomHlsMethod, CustomHlsScope, DrmKeyPair};
pub use concat_server::ConcatInputServer;
pub use save_pattern::render_save_pattern;
pub use subtitle_fix::{fix_and_convert_subtitles, SubtitleFormat};
pub use muxer::{
    binary_concat_files, ffmpeg_merge_segments, mux_tracks, FFmpegConcatMode, MuxAfterDoneOptions,
    MuxContainerFormat, MuxImportTrack, MuxerTool, TrackToMux,
};
pub use config_file::{default_config_path, parse_config_file};
pub use doctor::{DoctorReport, ToolStatus};
