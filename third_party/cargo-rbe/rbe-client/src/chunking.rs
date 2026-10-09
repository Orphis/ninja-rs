//! Content-defined chunking: REAPI's `FAST_CDC_2020` chunking function.

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
}
