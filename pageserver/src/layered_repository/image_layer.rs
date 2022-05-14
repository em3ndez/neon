//! An ImageLayer represents an image or a snapshot of a key-range at
//! one particular LSN. It contains an image of all key-value pairs
//! in its key-range. Any key that falls into the image layer's range
//! but does not exist in the layer, does not exist.
//!
//! An image layer is stored in a file on disk. The file is stored in
//! timelines/<timelineid> directory.  Currently, there are no
//! subdirectories, and each image layer file is named like this:
//!
//!    <key start>-<key end>__<LSN>
//!
//! For example:
//!
//!    000000067F000032BE0000400000000070B6-000000067F000032BE0000400000000080B6__00000000346BC568
//!
//! Every image layer file consists of three parts: "summary",
//! "index", and "values".  The summary is a fixed size header at the
//! beginning of the file, and it contains basic information about the
//! layer, and offsets to the other parts. The "index" is a B-tree,
//! mapping from Key to an offset in the "values" part.  The
//! actual page images are stored in the "values" part.
//!
//! Each page image is compressed with ZStandard. See Compression section
//! in the delta_layer.rs for more discussion. Difference from a delta
//! layer is that we don't currently use a dictionary for image layers.
use crate::config;
use crate::config::PageServerConf;
use crate::layered_repository::blob_io::{BlobCursor, BlobWriter, WriteBlobWriter};
use crate::layered_repository::block_io::{BlockBuf, BlockReader, FileBlockReader};
use crate::layered_repository::disk_btree::{DiskBtreeBuilder, DiskBtreeReader, VisitDirection};
use crate::layered_repository::filename::{ImageFileName, PathOrConf};
use crate::layered_repository::storage_layer::{
    Layer, ValueReconstructResult, ValueReconstructState,
};
use crate::page_cache::PAGE_SZ;
use crate::repository::{Key, Value, KEY_SIZE};
use crate::virtual_file::VirtualFile;
use crate::{IMAGE_FILE_MAGIC, STORAGE_FORMAT_VERSION};
use anyhow::{bail, ensure, Context, Result};
use bytes::Bytes;
use hex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::io::{Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{RwLock, RwLockReadGuard};
use tracing::*;

use utils::{
    bin_ser::BeSer,
    lsn::Lsn,
    zid::{ZTenantId, ZTimelineId},
};

///
/// Header stored in the beginning of the file
///
/// After this comes the 'values' part, starting on block 1. After that,
/// the 'index' starts at the block indicated by 'index_start_blk'
///
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Summary {
    /// Magic value to identify this as a zenith image file. Always IMAGE_FILE_MAGIC.
    magic: u16,
    format_version: u16,

    tenantid: ZTenantId,
    timelineid: ZTimelineId,
    key_range: Range<Key>,
    lsn: Lsn,

    /// Block number where the 'index' part of the file begins.
    index_start_blk: u32,
    /// Block within the 'index', where the B-tree root page is stored
    index_root_blk: u32,
    // the 'values' part starts after the summary header, on block 1.
}

impl From<&ImageLayer> for Summary {
    fn from(layer: &ImageLayer) -> Self {
        Self {
            magic: IMAGE_FILE_MAGIC,
            format_version: STORAGE_FORMAT_VERSION,
            tenantid: layer.tenantid,
            timelineid: layer.timelineid,
            key_range: layer.key_range.clone(),
            lsn: layer.lsn,

            index_start_blk: 0,
            index_root_blk: 0,
        }
    }
}

///
/// Struct representing reference to BLOB in the file. In an image layer,
/// each blob is an image of the page. It can be compressed or not, and
/// that is stored in low bit of the BlobRef.
///
#[derive(Debug, Serialize, Deserialize, Copy, Clone)]
struct BlobRef(u64);

/// Flag indicating that this blob is compressed
const BLOB_COMPRESSED: u64 = 1;

impl BlobRef {
    pub fn compressed(&self) -> bool {
        (self.0 & BLOB_COMPRESSED) != 0
    }

    pub fn pos(&self) -> u64 {
        self.0 >> 1
    }

    pub fn new(pos: u64, compressed: bool) -> BlobRef {
        let mut blob_ref = pos << 1;
        if compressed {
            blob_ref |= BLOB_COMPRESSED;
        }
        BlobRef(blob_ref)
    }
}

///
/// ImageLayer is the in-memory data structure associated with an on-disk image
/// file.  We keep an ImageLayer in memory for each file, in the LayerMap. If a
/// layer is in "loaded" state, we have a copy of the index in memory, in 'inner'.
/// Otherwise the struct is just a placeholder for a file that exists on disk,
/// and it needs to be loaded before using it in queries.
///
pub struct ImageLayer {
    path_or_conf: PathOrConf,
    pub tenantid: ZTenantId,
    pub timelineid: ZTimelineId,
    pub key_range: Range<Key>,

    // This entry contains an image of all pages as of this LSN
    pub lsn: Lsn,

    inner: RwLock<ImageLayerInner>,
}

pub struct ImageLayerInner {
    /// If false, the 'index' has not been loaded into memory yet.
    loaded: bool,

    // values copied from summary
    index_start_blk: u32,
    index_root_blk: u32,

    /// Reader object for reading blocks from the file. (None if not loaded yet)
    file: Option<FileBlockReader<VirtualFile>>,
}

impl ImageLayerInner {
    fn create_decompressor(&self) -> Result<zstd::bulk::Decompressor<'_>> {
        let decompressor = zstd::bulk::Decompressor::new()?;
        Ok(decompressor)
    }
}

impl Layer for ImageLayer {
    fn filename(&self) -> PathBuf {
        PathBuf::from(self.layer_name().to_string())
    }

    fn local_path(&self) -> Option<PathBuf> {
        Some(self.path())
    }

    fn get_tenant_id(&self) -> ZTenantId {
        self.tenantid
    }

    fn get_timeline_id(&self) -> ZTimelineId {
        self.timelineid
    }

    fn get_key_range(&self) -> Range<Key> {
        self.key_range.clone()
    }

    fn get_lsn_range(&self) -> Range<Lsn> {
        // End-bound is exclusive
        self.lsn..(self.lsn + 1)
    }

    /// Look up given page in the file
    fn get_value_reconstruct_data(
        &self,
        key: Key,
        lsn_range: Range<Lsn>,
        reconstruct_state: &mut ValueReconstructState,
    ) -> anyhow::Result<ValueReconstructResult> {
        assert!(self.key_range.contains(&key));
        assert!(lsn_range.start >= self.lsn);
        assert!(lsn_range.end >= self.lsn);

        let inner = self.load()?;

        let mut decompressor = inner.create_decompressor()?;

        let file = inner.file.as_ref().unwrap();
        let tree_reader = DiskBtreeReader::new(inner.index_start_blk, inner.index_root_blk, file);

        let mut keybuf: [u8; KEY_SIZE] = [0u8; KEY_SIZE];
        key.write_to_byte_slice(&mut keybuf);
        if let Some(value) = tree_reader.get(&keybuf)? {
            let blob_ref = BlobRef(value);
            let blob_content =
                file.block_cursor()
                    .read_blob(blob_ref.pos())
                    .with_context(|| {
                        format!(
                            "failed to read value from data file {} at offset {}",
                            self.filename().display(),
                            blob_ref.pos()
                        )
                    })?;

            let uncompressed_bytes = if blob_ref.compressed() {
                decompressor.decompress(&blob_content, blob_content.len())?
            } else {
                blob_content
            };

            let value = Bytes::from(uncompressed_bytes);

            reconstruct_state.img = Some((self.lsn, value));
            Ok(ValueReconstructResult::Complete)
        } else {
            Ok(ValueReconstructResult::Missing)
        }
    }

    fn iter(&self) -> Box<dyn Iterator<Item = Result<(Key, Lsn, Value)>>> {
        todo!();
    }

    fn delete(&self) -> Result<()> {
        // delete underlying file
        fs::remove_file(self.path())?;
        Ok(())
    }

    fn is_incremental(&self) -> bool {
        false
    }

    fn is_in_memory(&self) -> bool {
        false
    }

    /// debugging function to print out the contents of the layer
    fn dump(&self, verbose: bool) -> Result<()> {
        println!(
            "----- image layer for ten {} tli {} key {}-{} at {} ----",
            self.tenantid, self.timelineid, self.key_range.start, self.key_range.end, self.lsn
        );

        if !verbose {
            return Ok(());
        }

        let inner = self.load()?;
        let file = inner.file.as_ref().unwrap();
        let tree_reader =
            DiskBtreeReader::<_, KEY_SIZE>::new(inner.index_start_blk, inner.index_root_blk, file);

        tree_reader.dump()?;

        tree_reader.visit(&[0u8; KEY_SIZE], VisitDirection::Forwards, |key, value| {
            let blob_ref = BlobRef(value);
            println!(
                "key: {} offset {}{}",
                hex::encode(key),
                blob_ref.pos(),
                if blob_ref.compressed() {
                    " (compressed)"
                } else {
                    ""
                }
            );
            true
        })?;

        Ok(())
    }
}

impl ImageLayer {
    fn path_for(
        path_or_conf: &PathOrConf,
        timelineid: ZTimelineId,
        tenantid: ZTenantId,
        fname: &ImageFileName,
    ) -> PathBuf {
        match path_or_conf {
            PathOrConf::Path(path) => path.to_path_buf(),
            PathOrConf::Conf(conf) => conf
                .timeline_path(&timelineid, &tenantid)
                .join(fname.to_string()),
        }
    }

    ///
    /// Open the underlying file and read the metadata into memory, if it's
    /// not loaded already.
    ///
    fn load(&self) -> Result<RwLockReadGuard<ImageLayerInner>> {
        loop {
            // Quick exit if already loaded
            let inner = self.inner.read().unwrap();
            if inner.loaded {
                return Ok(inner);
            }

            // Need to open the file and load the metadata. Upgrade our lock to
            // a write lock. (Or rather, release and re-lock in write mode.)
            drop(inner);
            let mut inner = self.inner.write().unwrap();
            if !inner.loaded {
                self.load_inner(&mut inner).with_context(|| {
                    format!("Failed to load image layer {}", self.path().display())
                })?
            } else {
                // Another thread loaded it while we were not holding the lock.
            }

            // We now have the file open and loaded. There's no function to do
            // that in the std library RwLock, so we have to release and re-lock
            // in read mode. (To be precise, the lock guard was moved in the
            // above call to `load_inner`, so it's already been released). And
            // while we do that, another thread could unload again, so we have
            // to re-check and retry if that happens.
            drop(inner);
        }
    }

    fn load_inner(&self, inner: &mut ImageLayerInner) -> Result<()> {
        let path = self.path();

        // Open the file if it's not open already.
        if inner.file.is_none() {
            let file = VirtualFile::open(&path)
                .with_context(|| format!("Failed to open file '{}'", path.display()))?;
            inner.file = Some(FileBlockReader::new(file));
        }
        let file = inner.file.as_mut().unwrap();
        let summary_blk = file.read_blk(0)?;
        let actual_summary = Summary::des_prefix(summary_blk.as_ref())?;

        match &self.path_or_conf {
            PathOrConf::Conf(_) => {
                let mut expected_summary = Summary::from(self);
                expected_summary.index_start_blk = actual_summary.index_start_blk;
                expected_summary.index_root_blk = actual_summary.index_root_blk;

                if actual_summary != expected_summary {
                    bail!("in-file summary does not match expected summary. actual = {:?} expected = {:?}", actual_summary, expected_summary);
                }
            }
            PathOrConf::Path(path) => {
                let actual_filename = Path::new(path.file_name().unwrap());
                let expected_filename = self.filename();

                if actual_filename != expected_filename {
                    println!(
                        "warning: filename does not match what is expected from in-file summary"
                    );
                    println!("actual: {:?}", actual_filename);
                    println!("expected: {:?}", expected_filename);
                }
            }
        }

        inner.index_start_blk = actual_summary.index_start_blk;
        inner.index_root_blk = actual_summary.index_root_blk;
        inner.loaded = true;
        Ok(())
    }

    /// Create an ImageLayer struct representing an existing file on disk
    pub fn new(
        conf: &'static PageServerConf,
        timelineid: ZTimelineId,
        tenantid: ZTenantId,
        filename: &ImageFileName,
    ) -> ImageLayer {
        ImageLayer {
            path_or_conf: PathOrConf::Conf(conf),
            timelineid,
            tenantid,
            key_range: filename.key_range.clone(),
            lsn: filename.lsn,
            inner: RwLock::new(ImageLayerInner {
                loaded: false,
                file: None,
                index_start_blk: 0,
                index_root_blk: 0,
            }),
        }
    }

    /// Create an ImageLayer struct representing an existing file on disk.
    ///
    /// This variant is only used for debugging purposes, by the 'dump_layerfile' binary.
    pub fn new_for_path<F>(path: &Path, file: F) -> Result<ImageLayer>
    where
        F: std::os::unix::prelude::FileExt,
    {
        let mut summary_buf = Vec::new();
        summary_buf.resize(PAGE_SZ, 0);
        file.read_exact_at(&mut summary_buf, 0)?;
        let summary = Summary::des_prefix(&summary_buf)?;

        Ok(ImageLayer {
            path_or_conf: PathOrConf::Path(path.to_path_buf()),
            timelineid: summary.timelineid,
            tenantid: summary.tenantid,
            key_range: summary.key_range,
            lsn: summary.lsn,
            inner: RwLock::new(ImageLayerInner {
                file: None,
                loaded: false,
                index_start_blk: 0,
                index_root_blk: 0,
            }),
        })
    }

    fn layer_name(&self) -> ImageFileName {
        ImageFileName {
            key_range: self.key_range.clone(),
            lsn: self.lsn,
        }
    }

    /// Path to the layer file in pageserver workdir.
    pub fn path(&self) -> PathBuf {
        Self::path_for(
            &self.path_or_conf,
            self.timelineid,
            self.tenantid,
            &self.layer_name(),
        )
    }
}

/// A builder object for constructing a new image layer.
///
/// Usage:
///
/// 1. Create the ImageLayerWriter by calling ImageLayerWriter::new(...)
///
/// 2. Write the contents by calling `put_page_image` for every key-value
///    pair in the key range.
///
/// 3. Call `finish`.
///
pub struct ImageLayerWriter {
    conf: &'static PageServerConf,
    _path: PathBuf,
    timelineid: ZTimelineId,
    tenantid: ZTenantId,
    key_range: Range<Key>,
    lsn: Lsn,

    blob_writer: WriteBlobWriter<VirtualFile>,
    tree: DiskBtreeBuilder<BlockBuf, KEY_SIZE>,

    compressor: Option<zstd::bulk::Compressor<'static>>,
}

impl ImageLayerWriter {
    pub fn new(
        conf: &'static PageServerConf,
        timelineid: ZTimelineId,
        tenantid: ZTenantId,
        key_range: &Range<Key>,
        lsn: Lsn,
    ) -> anyhow::Result<ImageLayerWriter> {
        // Create the file
        //
        // Note: This overwrites any existing file. There shouldn't be any.
        // FIXME: throw an error instead?
        let path = ImageLayer::path_for(
            &PathOrConf::Conf(conf),
            timelineid,
            tenantid,
            &ImageFileName {
                key_range: key_range.clone(),
                lsn,
            },
        );
        info!("new image layer {}", path.display());
        let mut file = VirtualFile::create(&path)?;
        // make room for the header block
        file.seek(SeekFrom::Start(PAGE_SZ as u64))?;
        let blob_writer = WriteBlobWriter::new(file, PAGE_SZ as u64);

        // Initialize the b-tree index builder
        let block_buf = BlockBuf::new();
        let tree_builder = DiskBtreeBuilder::new(block_buf);

        // TODO: use a dictionary
        let compressor = {
            let compressor = zstd::bulk::Compressor::new(config::ZSTD_COMPRESSION_LEVEL)?;
            Some(compressor)
        };

        let writer = ImageLayerWriter {
            conf,
            _path: path,
            timelineid,
            tenantid,
            key_range: key_range.clone(),
            lsn,
            tree: tree_builder,
            blob_writer,
            compressor,
        };

        Ok(writer)
    }

    ///
    /// Write next value to the file.
    ///
    /// The page versions must be appended in blknum order.
    ///
    pub fn put_image(&mut self, key: Key, img: &[u8]) -> Result<()> {
        ensure!(self.key_range.contains(&key));

        let mut blob_content = img;
        let mut compressed = false;

        // Try to compress the blob
        let compressed_bytes;
        if let Some(ref mut compressor) = self.compressor {
            compressed_bytes = compressor.compress(blob_content)?;

            // If compressed version is not any smaller than the original,
            // store it uncompressed. This not just an optimization, the
            // the decompression assumes that too. That simplifies the
            // decompression, because you don't need to jump through any
            // hoops to determine how large a buffer you need to hold the
            // decompression result.
            if compressed_bytes.len() < blob_content.len() {
                blob_content = &compressed_bytes;
                compressed = true;
            }
        }

        // Write it to the file
        let off = self.blob_writer.write_blob(blob_content)?;
        let blob_ref = BlobRef::new(off, compressed);

        // And store the reference in the B-tree
        let mut keybuf: [u8; KEY_SIZE] = [0u8; KEY_SIZE];
        key.write_to_byte_slice(&mut keybuf);
        self.tree.append(&keybuf, blob_ref.0)?;

        Ok(())
    }

    pub fn finish(self) -> anyhow::Result<ImageLayer> {
        let index_start_blk =
            ((self.blob_writer.size() + PAGE_SZ as u64 - 1) / PAGE_SZ as u64) as u32;

        let mut file = self.blob_writer.into_inner();

        // Write out the index
        file.seek(SeekFrom::Start(index_start_blk as u64 * PAGE_SZ as u64))?;
        let (index_root_blk, block_buf) = self.tree.finish()?;
        for buf in block_buf.blocks {
            file.write_all(buf.as_ref())?;
        }

        // Fill in the summary on blk 0
        let summary = Summary {
            magic: IMAGE_FILE_MAGIC,
            format_version: STORAGE_FORMAT_VERSION,
            tenantid: self.tenantid,
            timelineid: self.timelineid,
            key_range: self.key_range.clone(),
            lsn: self.lsn,
            index_start_blk,
            index_root_blk,
        };
        file.seek(SeekFrom::Start(0))?;
        Summary::ser_into(&summary, &mut file)?;

        // Note: Because we open the file in write-only mode, we cannot
        // reuse the same VirtualFile for reading later. That's why we don't
        // set inner.file here. The first read will have to re-open it.
        let layer = ImageLayer {
            path_or_conf: PathOrConf::Conf(self.conf),
            timelineid: self.timelineid,
            tenantid: self.tenantid,
            key_range: self.key_range.clone(),
            lsn: self.lsn,
            inner: RwLock::new(ImageLayerInner {
                loaded: false,
                file: None,
                index_start_blk,
                index_root_blk,
            }),
        };
        trace!("created image layer {}", layer.path().display());

        Ok(layer)
    }
}
