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

pub use range::{ByteRange, RangeError};
pub use chunk::{Chunk, ChunkManager, ChunkStatus, ChunkError};
pub use mirror::{Mirror, MirrorRacer};
pub use storage::{DiskWriter, StorageError, ConcurrentMmap};
pub use state::{DownloadState, StateError};
pub use engine::{DownloadEngine, DownloadOptions, EngineSnapshot};
pub use resolver::SmartResolver;
pub use hls::{HlsEngine, HlsSegment, HlsError, parse_hls_playlist};
pub use metalink::{parse_metalink, MetalinkFile};
pub use torrent::{parse_magnet_uri, parse_torrent_bytes, is_magnet_uri, MagnetInfo, TorrentInfo};
pub use queue::{DownloadQueue, QueueItem, QueueItemStatus};

