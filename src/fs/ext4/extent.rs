//! ext4 extent-tree implementation
//!
//! Replaces the formerly used logical block map with indirect pointers.

use core::{cmp::Ordering, mem};

use alloc::vec::Vec;
use bytemuck::{bytes_of, cast, from_bytes, Pod, Zeroable};
use core::ops::Deref;

use crate::fs::ext4::inode::{Inode, InodeNumber, LockedInode, LockedInodeStrongRef};
use crate::fs::ext4::sb::{Ext4BlkCount, Ext4FsUuid, IncompatibleFeatureSet};
use crate::fs::ext4::LockedExt4Fs;
use crate::{
    error,
    errors::{CanFail, IOError},
    ext4_uint_field_range,
    fs::ext4::{crc32c_calc, inode::InodeGeneration, Ext4Fs, Ext4Inode},
};

/// Internal ext4 extent tree representation.
#[derive(Clone)]
pub(crate) struct ExtentTree {
    pub(crate) extents: Vec<Extent>,
    locked_inode: LockedInodeStrongRef,
}

impl core::fmt::Debug for ExtentTree {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for extent in &self.extents {
            f.write_fmt(format_args!(
                "({:?}-{:?}) -> {:?} \n",
                extent.block,
                extent.block + extent.len,
                extent.start_blk()
            ))?;
        }

        Ok(())
    }
}

/// An extent block contains the data of the extent tree.
///
/// It begins with a header that contains information about the entries in the block.
/// If the block if a leaf block (its depth is == 0), the header is followed by [`Extent`] entries.
/// Otherwise, it is followed by index nodes ([`ExtentIdx`]).
///
/// Except for the first 4 extents contained in the inode (that do not follow this structure), an
/// extent block is checksummed, and that checksum is contained in the last 4 bytes of the block,
/// that would be left unused anyway.
///
/// The general structure of an extent block is therefore:
///
/// ┌─────────────┬────────────────────┬─────────────────────┬─────────────────────────────┐
/// │Extent header│ Index node /       │         ...         │          Extent tail        │
/// │             │ Extent (leaf node) │                     │    (checksum of the block)  │
/// └─────────────┴────────────────────┴─────────────────────┴─────────────────────────────┘
///
/// Extent blocks are directly loaded from disk when parsing an [`Ext4Inode`] extent tree.
///
/// # Checksum
///
/// The checksum of the extent block is :
/// ```
/// crc32c_calc(fs_uuid + inode_id + inode_gen + extent_blk)
/// ```
pub(crate) struct ExtentBlock(pub(crate) Vec<u8>);

impl ExtentBlock {
    /// Compares the checksum of the `ExtentBlock` loaded in memory to its on-disk value.
    pub(crate) fn validate_chksum(
        &self,
        fs_uuid: Ext4FsUuid,
        inode_id: InodeNumber,
        inode_gen: InodeGeneration,
    ) -> bool {
        let on_disk_chksum: ExtentBlockChksum =
            *from_bytes(&self.0[self.0.len() - 4..self.0.len()]);

        let mut chksum_bytes: Vec<u8> = alloc::vec![];
        chksum_bytes.extend_from_slice(bytes_of(&fs_uuid));
        chksum_bytes.extend_from_slice(bytes_of(&inode_id));
        chksum_bytes.extend_from_slice(bytes_of(&inode_gen));
        chksum_bytes.extend_from_slice(&self.0[..self.0.len() - 4]);

        let comp_chksum: ExtentBlockChksum = cast(crc32c_calc(&chksum_bytes));

        if comp_chksum != on_disk_chksum {
            error!(
                "ext4",
                "invalid extent block checksum (inode {:#x})",
                cast::<InodeNumber, u32>(inode_id)
            );

            return false;
        }

        true
    }

    /// Returns the [`ExtentHeader`] for this `ExtentBlock`
    ///
    /// Every block, whether it contains leaf nodes or index nodes, begins with an `ExtentHeader`.
    pub(crate) fn get_header(&self) -> ExtentHeader {
        *from_bytes(&self.0[..mem::size_of::<ExtentHeader>()])
    }

    /// Returns the raw bytes for the entry `entry` of the extent block.
    pub(crate) fn get_entry_bytes(&self, entry: u16) -> Option<ExtentBlkRawEntry> {
        let header = self.get_header();
        let entries = header.entries;

        if cast::<u16, Ext4ExtentHeaderEntriesCount>(entry) >= entries {
            return None;
        }

        Some(ExtentBlkRawEntry(
            &self.0[(mem::size_of::<ExtentHeader>() + usize::from(entry) * mem::size_of::<Extent>())
                ..mem::size_of::<ExtentHeader>()
                    + (1 + usize::from(entry)) * mem::size_of::<Extent>()],
        ))
    }
}

/// Raw bytes for an extent block entry.
///
/// Can be consumed into an [`Extent`] or an [`ExtentIdx`], depending on what type the entry is
/// expected to have.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(C)]
pub(crate) struct ExtentBlkRawEntry<'en>(&'en [u8]);

impl<'en> ExtentBlkRawEntry<'en> {
    /// Consumes this raw extent block entry into an [`Extent`]
    pub(crate) fn as_extent(self) -> Extent {
        *from_bytes(self.0)
    }

    /// Consumed this raw extent block entry into an [`ExtentIdx`]
    fn as_extent_idx(self) -> ExtentIdx {
        *from_bytes(self.0)
    }
}

/// Checksum for an entire extent block.
///
/// Located on-disk in the last four bytes of any extent block, expect for the 4 extents located in
/// the inode which are already checksummed (as the entire inode structure is).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub(crate) struct ExtentBlockChksum(u32);

/// Extent-layer traversal routine.
fn traverse_extent_layer(
    fs: &Ext4Fs,
    ext_data: &ExtentBlock,
    extents: &mut Vec<Extent>,
    inode: &Inode,
) -> Option<()> {
    let sb = fs.superblock.read();
    let header = ext_data.get_header();

    // this extent points directly to data blocks
    if header.is_leaf() {
        for entry in 0..cast::<Ext4ExtentHeaderEntriesCount, u16>(header.entries) {
            let extent: Extent = ext_data.get_entry_bytes(entry)?.as_extent();

            extents.push(extent);
        }

        return Some(());
    }

    for entry in 0..cast::<Ext4ExtentHeaderEntriesCount, u16>(header.entries) {
        let extent_idx: ExtentIdx = ext_data.get_entry_bytes(entry)?.as_extent_idx();

        let mut data = fs.allocate_blk();

        fs.read_blk_from_device(extent_idx.leaf(), &mut data).ok()?;

        let extent_blk = ExtentBlock(data);
        extent_blk.validate_chksum(sb.uuid, inode.number, inode.generation());
        traverse_extent_layer(fs, &extent_blk, extents, inode);
    }

    Some(())
}

impl ExtentTree {
    /// Loads an entire extent tree associated with an [`Ext4Inode`] to memory.
    pub(crate) fn load_extent_tree(
        locked_fs: LockedExt4Fs,
        locked_inode: LockedInodeStrongRef,
    ) -> Option<Self> {
        let fs = locked_fs.read();
        let sb = fs.superblock.read();
        let inode = locked_inode.read();
        if !sb
            .feature_incompat
            .includes(IncompatibleFeatureSet::EXT4_FEATURE_INCOMPAT_EXTENTS)
            | !inode.uses_extent_tree()
        {
            return None;
        };
        let mut extents: Vec<Extent> = alloc::vec![];
        let extent_blk = inode.i_block.as_extent_block();
        drop(sb);

        traverse_extent_layer(fs.deref(), &extent_blk, &mut extents, inode.deref());
        extents.sort_unstable();
        drop(inode);

        Some(Self {
            extents,
            locked_inode,
        })
    }

    /// Returns the physical block address corresponding to a logical block for this [`Ext4Inode`].
    pub(crate) fn get_exact_blk_mapping(&self, blk_id: Ext4InodeRelBlkId) -> Option<Ext4RealBlkId> {
        let ext_id = self
            .extents
            .binary_search_by(|ext| {
                if ext.contains(blk_id) {
                    return Ordering::Equal;
                } else if ext.block > blk_id {
                    return Ordering::Greater;
                }

                Ordering::Less
            })
            .ok()?;

        let extent = self.extents.get(ext_id)?;
        let offset_in_extent = blk_id - extent.block;

        Some(extent.start_blk() + offset_in_extent)
    }
}

/// A 16-bit physical block address (valid for direct reads from the disk).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub(crate) struct Ext4RealBlkId16(u16);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub(crate) struct Ext4RealBlkId32(u32);

impl Ext4RealBlkId32 {
    pub(crate) fn add_high_bits(self, high: Ext4RealBlkId32) -> Ext4RealBlkId {
        cast(u64::from(self.0) | (u64::from(high.0) << 32))
    }
}

/// A physical block address (valid for direct reads from the disk).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub(crate) struct Ext4RealBlkId(u64);

impl core::ops::Add<Ext4BlkCount> for Ext4RealBlkId {
    type Output = Ext4RealBlkId;

    fn add(self, rhs: Ext4BlkCount) -> Self::Output {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl core::ops::Add<u64> for Ext4RealBlkId {
    type Output = Ext4RealBlkId;

    fn add(self, rhs: u64) -> Self::Output {
        Self(self.0.saturating_add(rhs))
    }
}

impl PartialEq<Ext4BlkCount> for Ext4RealBlkId {
    fn eq(&self, other: &Ext4BlkCount) -> bool {
        self.0 == cast(*other)
    }
}

impl PartialOrd<Ext4BlkCount> for Ext4RealBlkId {
    fn partial_cmp(&self, other: &Ext4BlkCount) -> Option<Ordering> {
        Some(self.0.cmp(&cast(*other)))
    }
}

impl From<u64> for Ext4RealBlkId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<Ext4RealBlkId> for usize {
    fn from(value: Ext4RealBlkId) -> Self {
        value.0.try_into().expect("invalid blk number")
    }
}

impl From<usize> for Ext4RealBlkId {
    fn from(value: usize) -> Self {
        Ext4RealBlkId(value.try_into().expect("invalid blk number"))
    }
}

impl core::ops::Mul<u64> for Ext4RealBlkId {
    type Output = u64;

    fn mul(self, rhs: u64) -> Self::Output {
        self.0 * rhs
    }
}

impl core::ops::Add<Ext4ExtentLength> for Ext4RealBlkId {
    type Output = Ext4RealBlkId;

    fn add(self, rhs: Ext4ExtentLength) -> Self::Output {
        Self(self.0 + u64::from(rhs.0))
    }
}

impl core::ops::Add<Ext4InodeRelBlkId> for Ext4RealBlkId {
    type Output = Ext4RealBlkId;

    fn add(self, rhs: Ext4InodeRelBlkId) -> Self::Output {
        Ext4RealBlkId(self.0 + rhs.0)
    }
}

/// A logical block address, relative to the beginning of this [`Ext4Inode`].
///
/// Must be translated to a [`Ext4RealBlkId`] in order to be used to read valid data from the
/// disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub(crate) struct Ext4InodeRelBlkId(u64);

impl core::ops::Add<u64> for Ext4InodeRelBlkId {
    type Output = Ext4InodeRelBlkId;

    fn add(self, rhs: u64) -> Self::Output {
        Ext4InodeRelBlkId(self.0 + rhs)
    }
}

impl core::ops::Sub<u64> for Ext4InodeRelBlkId {
    type Output = Self;

    fn sub(self, rhs: u64) -> Self::Output {
        Self(self.0 - rhs)
    }
}

impl core::ops::Sub<Ext4ExtentInitialBlock> for Ext4InodeRelBlkId {
    type Output = Self;

    fn sub(self, rhs: Ext4ExtentInitialBlock) -> Self::Output {
        Self(self.0 - u64::from(rhs.0))
    }
}

impl PartialEq<Ext4ExtentInitialBlock> for Ext4InodeRelBlkId {
    fn eq(&self, other: &Ext4ExtentInitialBlock) -> bool {
        self.0 == u64::from(other.0)
    }
}

impl PartialOrd<Ext4ExtentInitialBlock> for Ext4InodeRelBlkId {
    fn partial_cmp(&self, other: &Ext4ExtentInitialBlock) -> Option<Ordering> {
        Some(self.0.cmp(&u64::from(other.0)))
    }
}

ext4_uint_field_range!(
    Ext4InodeRelBlkIdRange,
    Ext4InodeRelBlkId,
    " A range bounded inclusively below and exclusively above between two logical block addresses
relative to an [`Inode`]."
);

/// Magic number contained in an [`ExtentHeader`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
struct Ext4ExtentHeaderMagic(u16);

impl Ext4ExtentHeaderMagic {
    const VALID_EXT4_MAGIC: Self = Self(0xF30A);
}

/// Depth of the associated extent nodes in the extent tree.
///
/// If `== 0`, this extent points directly to data blocks (leaf nodes). Otherwise, it points to
/// other extent nodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
struct Ext4ExtentHeaderDepth(u16);

impl Ext4ExtentHeaderDepth {
    pub(crate) const LEAF_DEPTH: Self = Self(0);

    /// Change the depth of the associated extent nodes.
    ///
    /// Must be at most 5.
    pub(crate) fn set_depth(&mut self, new_depth: u16) -> CanFail<IOError> {
        if new_depth > 5 {
            return Err(IOError::InvalidCommand);
        }

        self.0 = new_depth;

        Ok(())
    }
}

/// Generation of the extent tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
struct Ext4ExtentHeaderGeneration(u32);

/// Number of valid extent entries following the header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
struct Ext4ExtentHeaderEntriesCount(u16);

/// Maximum number of valid extent entries following the header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
struct Ext4ExtentHeaderEntriesMax(u16);

/// Header contained in each node of the `ext4` extent tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(C, packed)]
pub(crate) struct ExtentHeader {
    /// Magic number (should be `0xf30a`)
    magic: Ext4ExtentHeaderMagic,

    /// Number of valid entries following the header
    entries: Ext4ExtentHeaderEntriesCount,

    /// Maximum number of entries that could follow the header
    max: Ext4ExtentHeaderEntriesMax,

    /// Depth of this node in the extent tree.
    ///
    /// If `eh_depth == 0`, this extent points to data blocks
    depth: Ext4ExtentHeaderDepth,

    /// Generation of the tree
    generation: Ext4ExtentHeaderGeneration,
}

impl ExtentHeader {
    /// Checks if this header corresponds to leaf nodes.
    pub(crate) fn is_leaf(&self) -> bool {
        let depth = self.depth;
        depth == Ext4ExtentHeaderDepth::LEAF_DEPTH
    }

    /// Loads an `ExtentHeader` from raw bytes, and checks if it corresponds to a valid header.
    pub(crate) unsafe fn load(h_bytes: &[u8]) -> Option<Self> {
        let header: ExtentHeader = *from_bytes(h_bytes);

        let magic = header.magic;
        if magic == Ext4ExtentHeaderMagic::VALID_EXT4_MAGIC {
            Some(header)
        } else {
            None
        }
    }
}

/// Number of blocks covered by a leaf node of the extent tree.
///
/// Covers at most 32768 blocks for an initialized extent, and 32767 blocks for an uninitialized
/// extent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub(super) struct Ext4ExtentLength(u16);

impl Ext4ExtentLength {
    /// Checks if this extent is initialized
    pub(crate) fn is_initialized(self) -> bool {
        self.0 <= 32768
    }

    /// Returns the number of blocks covered by the associated extent, whether it is initialized or
    /// not.
    pub(crate) fn length(self) -> u16 {
        if self.is_initialized() {
            self.0
        } else {
            self.0 - 32768
        }
    }
}

/// First file block covered by an extent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub(super) struct Ext4ExtentInitialBlock(u32);

impl PartialEq<Ext4InodeRelBlkId> for Ext4ExtentInitialBlock {
    fn eq(&self, other: &Ext4InodeRelBlkId) -> bool {
        u64::from(self.0) == other.0
    }
}

impl PartialOrd<Ext4InodeRelBlkId> for Ext4ExtentInitialBlock {
    fn partial_cmp(&self, other: &Ext4InodeRelBlkId) -> Option<Ordering> {
        Some(u64::from(self.0).cmp(&other.0))
    }
}

impl core::ops::Add<Ext4ExtentLength> for Ext4ExtentInitialBlock {
    type Output = Ext4ExtentInitialBlock;

    fn add(self, rhs: Ext4ExtentLength) -> Self::Output {
        Self(self.0 + u32::from(rhs.0))
    }
}

/// Lower 32-bits of the block number to which the extent points.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub(super) struct Ext4ExtentPtrLo(u32);

/// Upper 16-bits of the block number to which the extent points.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub(super) struct Ext4ExtentPtrHi(u16);

impl core::ops::Add<Ext4ExtentPtrHi> for Ext4ExtentPtrLo {
    type Output = Ext4RealBlkId;

    fn add(self, rhs: Ext4ExtentPtrHi) -> Self::Output {
        Ext4RealBlkId(u64::from(self.0) | (u64::from(rhs.0) << 32))
    }
}

/// Represents a leaf node of the extent tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(C)]
pub(crate) struct Extent {
    /// First file block number that this extent covers
    pub(super) block: Ext4ExtentInitialBlock,

    /// Number of blocks covered by the extent.
    ///
    /// If `ee_len > 32768`, the extnt is uninitialized and the actual extent
    /// length is `ee_len - 32768`.
    pub(super) len: Ext4ExtentLength,

    /// High 16-bits of the block number to which this extent points
    pub(super) start_hi: Ext4ExtentPtrHi,

    /// Low 32-bits of the block number to which this extent points.
    pub(super) start_lo: Ext4ExtentPtrLo,
}

impl Extent {
    pub(crate) fn start_blk(&self) -> Ext4RealBlkId {
        self.start_lo + self.start_hi
    }

    pub(crate) fn contains(&self, blk_id: Ext4InodeRelBlkId) -> bool {
        self.block <= blk_id && self.block + self.len >= blk_id
    }
}

/// Lower 32-bits of the block number of the extent one level lower in the tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub(super) struct Ext4ExtentLeafPtrLo(u32);

/// Upper 16-bits of the block number of the extent one level lower in the tree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(transparent)]
pub(super) struct Ext4ExtentLeafPtrHi(u16);

impl core::ops::Add<Ext4ExtentLeafPtrHi> for Ext4ExtentLeafPtrLo {
    type Output = Ext4RealBlkId;

    fn add(self, rhs: Ext4ExtentLeafPtrHi) -> Self::Output {
        Ext4RealBlkId::from(u64::from(self.0) | (u64::from(rhs.0) << 32))
    }
}

/// Represents an internal node of the extent tree (an index node)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Pod, Zeroable)]
#[repr(C)]
struct ExtentIdx {
    /// This index node covers file blocks from `block` onward.
    block: Ext4ExtentInitialBlock,

    /// Low 32-bits of the block number of the extent node that is the next level lower in the
    /// tree.
    leaf_lo: Ext4ExtentLeafPtrLo,

    /// High 16-bits of the block number of the extent node that is the next level lower in the
    /// tree.
    leaf_hi: Ext4ExtentLeafPtrHi,

    unused: u16,
}

impl ExtentIdx {
    fn leaf(&self) -> Ext4RealBlkId {
        self.leaf_lo + self.leaf_hi
    }
}
