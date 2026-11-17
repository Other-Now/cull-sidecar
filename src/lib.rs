pub mod dataset;
pub mod db;
pub mod imaging;
pub mod import;
pub mod server;
pub mod shm;
pub mod sys;
pub mod transport;

/// pHash Hamming distance at or below which two photos count as the same
/// burst. Chosen from the threshold sweep in results/grouping.json.
pub const DEFAULT_GROUP_THRESHOLD: u32 = 10;
