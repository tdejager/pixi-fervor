//! On-disk format of the pack device: a header followed by the SquashFS layer
//! images at `PACK_ALIGN`-aligned offsets.
//!
//! ```text
//! 0   magic      [u8; 8]  "FERVPACK"
//! 8   version    u16 LE
//! 10  reserved   u16
//! 12  count      u32 LE   number of layer entries
//! 16  config_len u32 LE   length of the JSON guest config
//! 20  reserved   u32
//! 24  entries    count × { digest [u8; 32], offset u64 LE, len u64 LE }
//! ..  config     JSON GuestConfig
//! ..  zero padding up to the next PACK_ALIGN boundary
//! ```
//!
//! Entries are ordered bottom → top of the overlay stack.

use std::io::Read;

use crate::GuestConfig;

pub const PACK_MAGIC: [u8; 8] = *b"FERVPACK";
pub const PACK_VERSION: u16 = 1;
/// Alignment of every layer inside the pack (and of the header length).
pub const PACK_ALIGN: u64 = 4096;

const FIXED_LEN: usize = 24;
const ENTRY_LEN: usize = 48;
/// Upper bound that keeps a corrupt header from triggering huge allocations.
const MAX_ENTRIES: u32 = 4096;
const MAX_CONFIG_LEN: u32 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackEntry {
    /// sha256 of the layer image bytes.
    pub digest: [u8; 32],
    pub offset: u64,
    pub len: u64,
}

impl PackEntry {
    fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.digest);
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
    }

    fn decode(bytes: &[u8; ENTRY_LEN]) -> Self {
        let (digest, rest) = bytes.split_first_chunk::<32>().expect("entry holds a digest");
        let (offset, len) = rest.split_first_chunk::<8>().expect("entry holds an offset");
        Self {
            digest: *digest,
            offset: u64::from_le_bytes(*offset),
            len: u64::from_le_bytes(len.try_into().expect("entry holds a length")),
        }
    }

    /// First byte after this layer that the next one may use.
    fn aligned_end(&self) -> u64 {
        PackHeader::align_up(self.offset + self.len)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackHeader {
    layers: Vec<PackEntry>,
    config: GuestConfig,
}

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("not a fervor pack (bad magic)")]
    BadMagic,
    #[error("unsupported pack version {0}, expected {PACK_VERSION}")]
    UnsupportedVersion(u16),
    #[error("pack header declares {0} entries, more than the supported {MAX_ENTRIES}")]
    TooManyEntries(u32),
    #[error("pack header declares a {0} byte guest config, more than the supported {MAX_CONFIG_LEN}")]
    ConfigTooLarge(u32),
    #[error("layer {index} is not {PACK_ALIGN}-byte aligned or overlaps its predecessor")]
    BadLayout { index: usize },
    #[error("guest config is not valid JSON: {0}")]
    Config(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl PackHeader {
    /// Lays out layers (bottom → top, given as digest + length) directly after
    /// the header, each at the next aligned offset.
    pub fn layout(
        layers: impl IntoIterator<Item = ([u8; 32], u64)>,
        config: GuestConfig,
    ) -> Result<Self, PackError> {
        let layers: Vec<_> = layers.into_iter().collect();
        let config_len = serde_json::to_vec(&config)?.len();
        let mut offset = Self::len_for(layers.len(), config_len);
        let layers = layers
            .into_iter()
            .map(|(digest, len)| {
                let entry = PackEntry { digest, offset, len };
                offset = entry.aligned_end();
                entry
            })
            .collect();
        Ok(Self { layers, config })
    }

    pub fn layers(&self) -> &[PackEntry] {
        &self.layers
    }

    pub fn config(&self) -> &GuestConfig {
        &self.config
    }

    /// Total length of the pack device: header plus every aligned layer.
    pub fn pack_len(&self) -> u64 {
        match self.layers.last() {
            Some(last) => last.aligned_end(),
            None => self.header_len(),
        }
    }

    pub fn header_len(&self) -> u64 {
        // Infallible for a constructed header: the config serialized once already.
        let config_len = serde_json::to_vec(&self.config).map_or(0, |c| c.len());
        Self::len_for(self.layers.len(), config_len)
    }

    /// Serializes the header, zero-padded to `PACK_ALIGN`.
    pub fn encode(&self) -> Result<Vec<u8>, PackError> {
        let config = serde_json::to_vec(&self.config)?;
        let total = Self::len_for(self.layers.len(), config.len()) as usize;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&PACK_MAGIC);
        out.extend_from_slice(&PACK_VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(self.layers.len() as u32).to_le_bytes());
        out.extend_from_slice(&(config.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        for entry in &self.layers {
            entry.encode_into(&mut out);
        }
        out.extend_from_slice(&config);
        out.resize(total, 0);
        Ok(out)
    }

    /// Reads and validates a header from the start of a pack device.
    pub fn read_from(mut reader: impl Read) -> Result<Self, PackError> {
        let mut fixed = [0u8; FIXED_LEN];
        reader.read_exact(&mut fixed)?;
        if fixed[0..8] != PACK_MAGIC {
            return Err(PackError::BadMagic);
        }
        let version = u16::from_le_bytes([fixed[8], fixed[9]]);
        if version != PACK_VERSION {
            return Err(PackError::UnsupportedVersion(version));
        }
        let count = u32::from_le_bytes(fixed[12..16].try_into().expect("4 bytes"));
        if count > MAX_ENTRIES {
            return Err(PackError::TooManyEntries(count));
        }
        let config_len = u32::from_le_bytes(fixed[16..20].try_into().expect("4 bytes"));
        if config_len > MAX_CONFIG_LEN {
            return Err(PackError::ConfigTooLarge(config_len));
        }

        let mut entries = vec![0u8; count as usize * ENTRY_LEN];
        reader.read_exact(&mut entries)?;
        let (chunks, _) = entries.as_chunks::<ENTRY_LEN>();
        let layers: Vec<PackEntry> = chunks.iter().map(PackEntry::decode).collect();

        let mut config = vec![0u8; config_len as usize];
        reader.read_exact(&mut config)?;
        let config: GuestConfig = serde_json::from_slice(&config)?;

        let mut min_offset = Self::len_for(layers.len(), config_len as usize);
        for (index, entry) in layers.iter().enumerate() {
            if entry.offset % PACK_ALIGN != 0 || entry.offset < min_offset {
                return Err(PackError::BadLayout { index });
            }
            min_offset = entry
                .offset
                .checked_add(entry.len)
                .ok_or(PackError::BadLayout { index })?;
        }
        Ok(Self { layers, config })
    }

    /// Header length for `entries` layers and a `config_len` byte config.
    fn len_for(entries: usize, config_len: usize) -> u64 {
        Self::align_up((FIXED_LEN + entries * ENTRY_LEN + config_len) as u64)
    }

    fn align_up(value: u64) -> u64 {
        value.div_ceil(PACK_ALIGN) * PACK_ALIGN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> GuestConfig {
        GuestConfig {
            argv: vec!["python".into(), "-c".into(), "print(42)".into()],
            env: vec![("PATH".into(), "/opt/env/bin".into())],
            workdir: "/".into(),
            scratch_size_mib: 256,
        }
    }

    #[test]
    fn layout_aligns_layers_after_header_and_round_trips() {
        let header =
            PackHeader::layout([([1; 32], 5000), ([2; 32], 4096), ([3; 32], 1)], config()).unwrap();
        let offsets: Vec<u64> = header.layers().iter().map(|e| e.offset).collect();
        assert_eq!(offsets, vec![4096, 12288, 16384]);
        assert_eq!(header.pack_len(), 20480);

        let encoded = header.encode().unwrap();
        assert_eq!(encoded.len() as u64, header.header_len());
        assert_eq!(PackHeader::read_from(encoded.as_slice()).unwrap(), header);
    }

    #[test]
    fn rejects_overlapping_layers() {
        let header = PackHeader::layout([([1; 32], 8192), ([2; 32], 10)], config()).unwrap();
        let mut encoded = header.encode().unwrap();
        // Move the second layer's offset back into the first layer.
        let second_offset = FIXED_LEN + ENTRY_LEN + 32;
        encoded[second_offset..second_offset + 8].copy_from_slice(&8192u64.to_le_bytes());
        assert!(matches!(
            PackHeader::read_from(encoded.as_slice()),
            Err(PackError::BadLayout { index: 1 })
        ));
    }

    #[test]
    fn rejects_foreign_data() {
        let zeros = vec![0u8; 4096];
        assert!(matches!(PackHeader::read_from(zeros.as_slice()), Err(PackError::BadMagic)));
    }
}
