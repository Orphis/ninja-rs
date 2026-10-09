//! Content-defined chunking: REAPI's `FAST_CDC_2020` chunking function, and
//! what a server supports of split blobs (`SplitBlob` / `GetChunkMapping`)
//! and spliced ones (`SpliceBlob` / `RegisterChunkMapping`).

use bazel_remote_apis::build::bazel::remote::execution::v2 as re;
use fastcdc::v2020::{FastCDC, Normalization, StreamCDC};

/// FastCDC 2020 parameters, as REAPI specifies them: chunks of `avg / 4` to
/// `avg * 4` bytes, normalization level 2. The `fastcdc` crate's `v2020`
/// module is REAPI's reference implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FastCdc {
    pub avg: u32,
    pub seed: u32,
}

impl FastCdc {
    /// REAPI's recommended parameters.
    pub const RECOMMENDED: FastCdc = FastCdc { avg: 512 * 1024, seed: 0 };

    /// A server's parameters, if supported here (REAPI allows averages from
    /// 1 KiB to 8 MiB, the `fastcdc` crate up to 4 MiB).
    pub fn from_params(p: &re::FastCdc2020Params) -> Option<Self> {
        let avg = if p.avg_chunk_size_bytes == 0 { u64::from(Self::RECOMMENDED.avg) } else { p.avg_chunk_size_bytes };
        (1024..=u64::from(fastcdc::v2020::AVERAGE_MAX)).contains(&avg).then_some(Self { avg: avg as u32, seed: p.seed })
    }

    pub fn min(&self) -> u32 {
        self.avg / 4
    }

    pub fn max(&self) -> u32 {
        self.avg * 4
    }

    /// Whether a blob of this size is worth splitting: REAPI advises against
    /// splitting blobs no larger than a chunk can be.
    pub fn splits(&self, size: i64) -> bool {
        size > i64::from(self.max())
    }

    /// The chunks of `data`, in order.
    pub fn chunks<'a>(&self, data: &'a [u8]) -> impl Iterator<Item = &'a [u8]> + 'a {
        FastCDC::with_level_and_seed(data, self.min(), self.avg, self.max(), Normalization::Level2, u64::from(self.seed))
            .map(move |c| &data[c.offset..c.offset + c.length])
    }

    /// The chunks of a stream, read as they're produced.
    pub fn stream<R: std::io::Read>(&self, source: R) -> StreamCDC<R> {
        StreamCDC::with_level_and_seed(source, self.min(), self.avg, self.max(), Normalization::Level2, u64::from(self.seed))
    }
}

/// What a server supports of blob splitting and splicing, from its
/// capabilities.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chunking {
    /// `SplitBlob` / `GetChunkMapping`: the chunks of a blob it holds.
    pub split: bool,
    /// `SpliceBlob` / `RegisterChunkMapping`: a blob made of chunks it holds.
    pub splice: bool,
    /// RE API v2.13 or newer: the streaming `GetChunkMapping` and
    /// `RegisterChunkMapping` replace `SplitBlob` and `SpliceBlob`.
    pub v2_13: bool,
    /// The server's FastCDC 2020 parameters, when it advertises them (and
    /// they are supported here).
    pub fast_cdc: Option<FastCdc>,
}

impl Chunking {
    pub fn from_capabilities(caps: &re::ServerCapabilities) -> Self {
        let Some(cache) = &caps.cache_capabilities else { return Self::default() };
        let v2_13 = caps.high_api_version.as_ref().is_some_and(|v| (v.major, v.minor) >= (2, 13));
        Self {
            split: cache.split_blob_support,
            splice: cache.splice_blob_support,
            v2_13,
            fast_cdc: cache.fast_cdc_2020_params.as_ref().and_then(FastCdc::from_params),
        }
    }

    /// The chunking function requests name: FastCDC when the server uses
    /// it, else none in particular (the server picks one for splits, and
    /// only checks the concatenation of splices).
    pub fn function(&self) -> i32 {
        if self.fast_cdc.is_some() { re::chunking_function::Value::FastCdc2020 as i32 } else { re::chunking_function::Value::Unknown as i32 }
    }

    /// How to chunk a blob for a splice, if it is worth one: with the
    /// server's FastCDC parameters, or REAPI's recommended ones.
    pub fn splice_chunker(&self, d: &re::Digest) -> Option<FastCdc> {
        let cdc = self.fast_cdc.unwrap_or(FastCdc::RECOMMENDED);
        (self.splice && cdc.splits(d.size_bytes)).then_some(cdc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_cover_the_data() {
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let cdc = FastCdc::RECOMMENDED;
        let chunks: Vec<&[u8]> = cdc.chunks(&data).collect();
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.len() <= cdc.max() as usize));
        assert_eq!(chunks.concat(), data);
        let streamed: Vec<Vec<u8>> = cdc.stream(data.as_slice()).map(|c| c.unwrap().data).collect();
        assert_eq!(streamed, chunks.iter().map(|c| c.to_vec()).collect::<Vec<_>>());
    }

    #[test]
    fn server_parameters() {
        let p = |avg| re::FastCdc2020Params { avg_chunk_size_bytes: avg, seed: 0 };
        assert_eq!(FastCdc::from_params(&p(0)), Some(FastCdc::RECOMMENDED));
        assert_eq!(FastCdc::from_params(&p(64 * 1024)).map(|c| c.max()), Some(256 * 1024));
        assert_eq!(FastCdc::from_params(&p(8 << 20)), None);
    }

    #[test]
    fn capabilities() {
        use bazel_remote_apis::build::bazel::semver::SemVer;
        let mut caps = re::ServerCapabilities {
            cache_capabilities: Some(re::CacheCapabilities {
                split_blob_support: true,
                splice_blob_support: true,
                fast_cdc_2020_params: Some(re::FastCdc2020Params { avg_chunk_size_bytes: 64 * 1024, seed: 0 }),
                ..Default::default()
            }),
            high_api_version: Some(SemVer { major: 2, minor: 13, patch: 0, prerelease: String::new() }),
            ..Default::default()
        };
        let c = Chunking::from_capabilities(&caps);
        assert!(c.split && c.splice && c.v2_13);
        assert_eq!(c.fast_cdc, Some(FastCdc { avg: 64 * 1024, seed: 0 }));
        assert_eq!(c.function(), re::chunking_function::Value::FastCdc2020 as i32);
        assert!(c.splice_chunker(&re::Digest { hash: String::new(), size_bytes: 256 * 1024 }).is_none());
        assert!(c.splice_chunker(&re::Digest { hash: String::new(), size_bytes: 256 * 1024 + 1 }).is_some());
        // Unsupported parameters: splices use the recommended ones.
        caps.cache_capabilities.as_mut().unwrap().fast_cdc_2020_params = Some(re::FastCdc2020Params { avg_chunk_size_bytes: 8 << 20, seed: 0 });
        caps.high_api_version.as_mut().unwrap().minor = 12;
        let c = Chunking::from_capabilities(&caps);
        assert!(!c.v2_13 && c.fast_cdc.is_none());
        assert_eq!(c.function(), re::chunking_function::Value::Unknown as i32);
        assert_eq!(c.splice_chunker(&re::Digest { hash: String::new(), size_bytes: 3 << 20 }), Some(FastCdc::RECOMMENDED));
    }
}
