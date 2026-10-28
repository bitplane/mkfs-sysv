//! Minimal SVR4 (System V) filesystem image writer.
//!
//! Layout (matches Linux `sysv_fs.h`):
//!
//! ```text
//! block 0:        boot
//! block 1:        superblock
//! blocks 2..=33:  inode table (32 blocks * 8 inodes = 256 inodes)
//! blocks 34..:    data zones
//! ```
//!
//! Files use direct-only block addressing (3 bytes per zone pointer,
//! 10 direct slots), so the maximum file size is 5 KB. Subdirectories
//! are supported. The free-block list uses the traditional SysV chain
//! scheme: each chain block holds up to 50 zone numbers plus a pointer
//! to the previous chain block.

use anyhow::Context;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const BLOCK_SIZE: usize = 512;
pub const MAGIC: u32 = 0xfd18_7e20;
const NICINOD: usize = 100;
const NICFREE: usize = 50;
const BADBL_INO: u32 = 1;
const ROOT_INO: u32 = 2;
const NAMELEN: usize = 14;
const DIRECT_BLOCKS: usize = 10;
const MAX_FILE_SIZE: usize = DIRECT_BLOCKS * BLOCK_SIZE;
const INODE_BLOCKS: usize = 32;
const INODE_SIZE: usize = 64;
const DIRENT_SIZE: usize = 16;

/// Number of inodes in the table (constant per layout).
pub const NINODES: u32 = (INODE_BLOCKS * BLOCK_SIZE / INODE_SIZE) as u32;
/// First data zone (after boot + super + inode table).
pub const FIRST_DATA_ZONE: u32 = (2 + INODE_BLOCKS) as u32;

const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

/// Stats reported after writing.
pub struct Stats {
    pub total_blocks: u32,
    pub ninodes: u32,
    pub free_blocks: u32,
    pub free_inodes: u32,
}

#[derive(Clone, Copy)]
struct Inode {
    mode: u16,
    nlink: u16,
    uid: u16,
    gid: u16,
    size: u32,
    /// 39 bytes of zone pointers: 10 direct (×3 bytes), then 1 indirect,
    /// 1 double, 1 triple. Only direct slots are populated by this writer.
    data: [u8; 39],
    gen: u8,
    atime: u32,
    mtime: u32,
    ctime: u32,
}

impl Inode {
    const fn zero() -> Self {
        Self {
            mode: 0,
            nlink: 0,
            uid: 0,
            gid: 0,
            size: 0,
            data: [0; 39],
            gen: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
        }
    }
}

fn get_inode_block(inode: &Inode, file_block: usize) -> u32 {
    let p = &inode.data[file_block * 3..];
    u32::from(p[0]) | (u32::from(p[1]) << 8) | (u32::from(p[2]) << 16)
}

fn set_inode_block(inode: &mut Inode, file_block: usize, disk_block: u32) -> anyhow::Result<()> {
    if file_block >= DIRECT_BLOCKS {
        anyhow::bail!("file too large for direct-only SysV test image");
    }
    let p = &mut inode.data[file_block * 3..];
    p[0] = (disk_block & 0xff) as u8;
    p[1] = ((disk_block >> 8) & 0xff) as u8;
    p[2] = ((disk_block >> 16) & 0xff) as u8;
    Ok(())
}

pub struct Sysv {
    image: Vec<u8>,
    total_blocks: u32,
    inodes: Vec<Inode>,
    block_used: Vec<bool>,
    inode_used: Vec<bool>,
    next_block: u32,
    next_inode: u32,
    now: u32,
}

impl Sysv {
    pub fn new(size_mb: u32) -> anyhow::Result<Self> {
        if size_mb == 0 {
            anyhow::bail!("size must be positive");
        }
        let image_size = u64::from(size_mb) * 1024 * 1024;
        if image_size > u64::from(u32::MAX) {
            anyhow::bail!("image size exceeds 4 GB");
        }
        let total_blocks = (image_size / BLOCK_SIZE as u64) as u32;
        if total_blocks <= FIRST_DATA_ZONE + 1 {
            anyhow::bail!("filesystem too small");
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);

        let mut block_used = vec![false; total_blocks as usize];
        for slot in block_used.iter_mut().take(FIRST_DATA_ZONE as usize) {
            *slot = true;
        }

        let mut inode_used = vec![false; (NINODES + 1) as usize];
        inode_used[BADBL_INO as usize] = true;
        inode_used[ROOT_INO as usize] = true;

        let mut sysv = Self {
            image: vec![0u8; image_size as usize],
            total_blocks,
            inodes: vec![Inode::zero(); NINODES as usize],
            block_used,
            inode_used,
            next_block: FIRST_DATA_ZONE,
            next_inode: ROOT_INO + 1,
            now,
        };

        // Set up root inode + its initial directory block.
        let root_block = sysv.alloc_block()?;
        {
            let root = &mut sysv.inodes[(ROOT_INO - 1) as usize];
            root.mode = (S_IFDIR | 0o755) as u16;
            root.nlink = 2;
            root.size = 2 * DIRENT_SIZE as u32;
            root.atime = now;
            root.mtime = now;
            root.ctime = now;
            set_inode_block(root, 0, root_block)?;
        }
        sysv.init_dir_block(root_block, ROOT_INO, ROOT_INO);

        Ok(sysv)
    }

    pub fn populate_root(&mut self, src: &Path) -> anyhow::Result<()> {
        self.populate_dir(ROOT_INO, src)
    }

    fn populate_dir(&mut self, dir_ino: u32, src_path: &Path) -> anyhow::Result<()> {
        for entry in fs::read_dir(src_path)? {
            let entry = entry?;
            let path = entry.path();
            let meta = fs::symlink_metadata(&path)?;
            if !meta.is_file() && !meta.is_dir() {
                anyhow::bail!("unsupported file type: {}", path.display());
            }
            let raw_name = entry.file_name();
            let name = raw_name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 filename: {:?}", raw_name))?;

            let child_ino = self.alloc_inode()?;
            let (perm, uid, gid) = perm_uid_gid(&meta)
                .with_context(|| format!("invalid metadata for {}", path.display()))?;
            {
                let child = &mut self.inodes[(child_ino - 1) as usize];
                child.uid = uid;
                child.gid = gid;
                child.atime = self.now;
                child.mtime = self.now;
                child.ctime = self.now;
            }

            self.add_dir_entry(dir_ino, name, child_ino)?;

            if meta.is_dir() {
                let block = self.alloc_block()?;
                {
                    let child = &mut self.inodes[(child_ino - 1) as usize];
                    set_inode_block(child, 0, block)?;
                    child.mode = (S_IFDIR | u32::from(perm)) as u16;
                    child.nlink = 2;
                    child.size = 2 * DIRENT_SIZE as u32;
                }
                self.init_dir_block(block, child_ino, dir_ino);
                self.inodes[(dir_ino - 1) as usize].nlink += 1;
                self.populate_dir(child_ino, &path)?;
            } else {
                {
                    let child = &mut self.inodes[(child_ino - 1) as usize];
                    child.mode = (S_IFREG | u32::from(perm)) as u16;
                    child.nlink = 1;
                }
                self.write_file_data(child_ino, &path)?;
            }
        }
        Ok(())
    }

    fn write_file_data(&mut self, ino: u32, path: &Path) -> anyhow::Result<()> {
        let mut data = Vec::new();
        File::open(path)?
            .take((MAX_FILE_SIZE + 1) as u64)
            .read_to_end(&mut data)
            .with_context(|| format!("reading {}", path.display()))?;
        if data.len() > MAX_FILE_SIZE {
            anyhow::bail!(
                "file exceeds {MAX_FILE_SIZE}-byte limit: {}",
                path.display()
            );
        }
        let size = data.len() as u32;
        self.inodes[(ino - 1) as usize].size = size;

        for (file_block, chunk) in data.chunks(BLOCK_SIZE).enumerate() {
            let disk_block = self.alloc_block()?;
            set_inode_block(&mut self.inodes[(ino - 1) as usize], file_block, disk_block)?;
            let off = (disk_block as usize) * BLOCK_SIZE;
            self.image[off..off + chunk.len()].copy_from_slice(chunk);
        }
        Ok(())
    }

    fn add_dir_entry(&mut self, dir_ino: u32, name: &str, child_ino: u32) -> anyhow::Result<()> {
        let bytes = name.as_bytes();
        let disk_name_len = bytes.len().min(NAMELEN);

        let dir = self.inodes[(dir_ino - 1) as usize]; // Copy
        let entries = (dir.size as usize) / DIRENT_SIZE;

        // Match the C version's collision rule: equal first `disk_name_len`
        // bytes, and either both are exactly 14 bytes or the existing entry
        // is null-terminated at that position.
        for i in 0..entries {
            let off_in_dir = i * DIRENT_SIZE;
            let file_block_idx = off_in_dir / BLOCK_SIZE;
            let block_off = off_in_dir % BLOCK_SIZE;
            let disk_block = get_inode_block(&dir, file_block_idx);
            let abs = (disk_block as usize) * BLOCK_SIZE + block_off;
            let existing_inode = u16::from_le_bytes(self.image[abs..abs + 2].try_into().unwrap());
            if existing_inode == 0 {
                continue;
            }
            let existing_name = &self.image[abs + 2..abs + 16];
            let prefix_match = existing_name[..disk_name_len] == bytes[..disk_name_len];
            let null_terminated = disk_name_len == NAMELEN || existing_name[disk_name_len] == 0;
            if prefix_match && null_terminated {
                anyhow::bail!("duplicate {NAMELEN}-byte directory name: {name}");
            }
        }

        let entry_idx = entries;
        let off_in_dir = entry_idx * DIRENT_SIZE;
        let file_block_idx = off_in_dir / BLOCK_SIZE;
        let block_off = off_in_dir % BLOCK_SIZE;

        if file_block_idx >= DIRECT_BLOCKS {
            anyhow::bail!("directory too large for direct-only SysV test image");
        }

        let mut disk_block = get_inode_block(&dir, file_block_idx);
        if disk_block == 0 {
            disk_block = self.alloc_block()?;
            set_inode_block(
                &mut self.inodes[(dir_ino - 1) as usize],
                file_block_idx,
                disk_block,
            )?;
            // image[disk_block..] is already zero from initial allocation.
        }

        let abs = (disk_block as usize) * BLOCK_SIZE + block_off;
        // Clear the dirent slot before writing, in case anything snuck in.
        for byte in self.image[abs..abs + DIRENT_SIZE].iter_mut() {
            *byte = 0;
        }
        self.image[abs..abs + 2].copy_from_slice(&(child_ino as u16).to_le_bytes());
        self.image[abs + 2..abs + 2 + disk_name_len].copy_from_slice(&bytes[..disk_name_len]);

        self.inodes[(dir_ino - 1) as usize].size += DIRENT_SIZE as u32;
        Ok(())
    }

    fn init_dir_block(&mut self, block: u32, self_ino: u32, parent_ino: u32) {
        let off = (block as usize) * BLOCK_SIZE;
        // dirent 0: ".", self_ino
        self.image[off..off + 2].copy_from_slice(&(self_ino as u16).to_le_bytes());
        self.image[off + 2] = b'.';
        // dirent 1: "..", parent_ino at offset +16
        self.image[off + 16..off + 18].copy_from_slice(&(parent_ino as u16).to_le_bytes());
        self.image[off + 18] = b'.';
        self.image[off + 19] = b'.';
    }

    fn alloc_block(&mut self) -> anyhow::Result<u32> {
        while self.next_block < self.total_blocks {
            let b = self.next_block as usize;
            self.next_block += 1;
            if !self.block_used[b] {
                self.block_used[b] = true;
                return Ok(b as u32);
            }
        }
        anyhow::bail!("no free blocks");
    }

    fn alloc_inode(&mut self) -> anyhow::Result<u32> {
        while self.next_inode <= NINODES {
            let i = self.next_inode as usize;
            self.next_inode += 1;
            if !self.inode_used[i] {
                self.inode_used[i] = true;
                return Ok(i as u32);
            }
        }
        anyhow::bail!("no free inodes");
    }

    pub fn write_to(mut self, path: &Path) -> anyhow::Result<Stats> {
        let (free_blocks, s_nfree, s_free) = self.build_free_block_list();
        let (free_inodes, s_ninode, s_inode) = self.build_free_inode_list();
        self.write_superblock(
            free_blocks,
            s_nfree,
            &s_free,
            free_inodes,
            s_ninode,
            &s_inode,
        );
        self.write_inode_table();

        let mut f = File::create(path)?;
        f.write_all(&self.image)?;
        f.sync_all()?;

        Ok(Stats {
            total_blocks: self.total_blocks,
            ninodes: NINODES,
            free_blocks,
            free_inodes,
        })
    }

    fn build_free_block_list(&mut self) -> (u32, u16, [u32; NICFREE]) {
        // Mirrors the C version's chain-builder: when the in-progress batch
        // is empty (initial state) or full (NICFREE entries), the current
        // free block is consumed as a "chain" block holding the previous
        // batch, then a new batch starts including this block as its first
        // entry. Both the chain-block-as-storage and chain-block-as-payload
        // contribute to total_free.
        let mut blocks = [0u32; NICFREE];
        let mut count: usize = 0;
        let mut total_free: u32 = 0;

        for block in FIRST_DATA_ZONE..self.total_blocks {
            if self.block_used[block as usize] {
                continue;
            }

            if count == NICFREE || count == 0 {
                self.write_free_list_block(block, count as u16, &blocks);
                count = 0;
            }

            blocks[count] = block;
            count += 1;
            total_free += 1;
        }

        let mut s_free = [0u32; NICFREE];
        s_free[..count].copy_from_slice(&blocks[..count]);
        (total_free, count as u16, s_free)
    }

    fn write_free_list_block(&mut self, block: u32, count: u16, blocks: &[u32]) {
        let off = (block as usize) * BLOCK_SIZE;
        for byte in self.image[off..off + BLOCK_SIZE].iter_mut() {
            *byte = 0;
        }
        // count: u16 at offset 0; pad u16 at 2
        self.image[off..off + 2].copy_from_slice(&count.to_le_bytes());
        // blocks: u32 starting at offset 4
        for (i, blk) in blocks.iter().take(count as usize).enumerate() {
            let p = off + 4 + i * 4;
            self.image[p..p + 4].copy_from_slice(&blk.to_le_bytes());
        }
    }

    fn build_free_inode_list(&self) -> (u32, u16, [u16; NICINOD]) {
        let mut s_inode = [0u16; NICINOD];
        let mut s_ninode: usize = 0;
        let mut total_free: u32 = 0;

        for ino in (ROOT_INO + 1)..=NINODES {
            if self.inode_used[ino as usize] {
                continue;
            }
            total_free += 1;
            if s_ninode < NICINOD {
                s_inode[s_ninode] = ino as u16;
                s_ninode += 1;
            }
        }
        (total_free, s_ninode as u16, s_inode)
    }

    fn write_superblock(
        &mut self,
        total_free: u32,
        s_nfree: u16,
        s_free: &[u32; NICFREE],
        total_inode: u32,
        s_ninode: u16,
        s_inode: &[u16; NICINOD],
    ) {
        let off = BLOCK_SIZE; // block 1
        let sb = &mut self.image[off..off + BLOCK_SIZE];
        for byte in sb.iter_mut() {
            *byte = 0;
        }

        sb[0..2].copy_from_slice(&(FIRST_DATA_ZONE as u16).to_le_bytes()); // s_isize
                                                                           // s_pad0 at 2
        sb[4..8].copy_from_slice(&self.total_blocks.to_le_bytes()); // s_fsize
        sb[8..10].copy_from_slice(&s_nfree.to_le_bytes()); // s_nfree
                                                           // s_pad1 at 10
                                                           // s_free[50] at 12
        for (i, blk) in s_free.iter().enumerate() {
            let p = 12 + i * 4;
            sb[p..p + 4].copy_from_slice(&blk.to_le_bytes());
        }
        sb[212..214].copy_from_slice(&s_ninode.to_le_bytes()); // s_ninode
                                                               // s_pad2 at 214
                                                               // s_inode[100] at 216
        for (i, ino) in s_inode.iter().enumerate() {
            let p = 216 + i * 2;
            sb[p..p + 2].copy_from_slice(&ino.to_le_bytes());
        }
        // s_flock/s_ilock/s_fmod/s_ronly at 416..420 stay 0
        sb[420..424].copy_from_slice(&self.now.to_le_bytes()); // s_time
                                                               // s_dinfo at 424..432 stays 0
        sb[432..436].copy_from_slice(&total_free.to_le_bytes()); // s_tfree
        sb[436..438].copy_from_slice(&(total_inode as u16).to_le_bytes()); // s_tinode
                                                                           // s_pad3 at 438
        sb[440..446].copy_from_slice(b"sysv\0\0"); // s_fname
        sb[446..452].copy_from_slice(b"test\0\0"); // s_fpack
                                                   // s_fill at 452..500 stays 0
        let s_state = 0x7c26_9d38_u32.wrapping_sub(self.now);
        sb[500..504].copy_from_slice(&s_state.to_le_bytes());
        sb[504..508].copy_from_slice(&MAGIC.to_le_bytes());
        sb[508..512].copy_from_slice(&1u32.to_le_bytes()); // s_type
    }

    fn write_inode_table(&mut self) {
        for (idx, inode) in self.inodes.iter().enumerate() {
            let off = 2 * BLOCK_SIZE + idx * INODE_SIZE;
            let buf = &mut self.image[off..off + INODE_SIZE];
            buf[0..2].copy_from_slice(&inode.mode.to_le_bytes());
            buf[2..4].copy_from_slice(&inode.nlink.to_le_bytes());
            buf[4..6].copy_from_slice(&inode.uid.to_le_bytes());
            buf[6..8].copy_from_slice(&inode.gid.to_le_bytes());
            buf[8..12].copy_from_slice(&inode.size.to_le_bytes());
            buf[12..51].copy_from_slice(&inode.data);
            buf[51] = inode.gen;
            buf[52..56].copy_from_slice(&inode.atime.to_le_bytes());
            buf[56..60].copy_from_slice(&inode.mtime.to_le_bytes());
            buf[60..64].copy_from_slice(&inode.ctime.to_le_bytes());
        }
    }
}

#[cfg(unix)]
fn checked_owner(uid: u32, gid: u32) -> anyhow::Result<(u16, u16)> {
    let uid = u16::try_from(uid).with_context(|| format!("UID {uid} exceeds 65535"))?;
    let gid = u16::try_from(gid).with_context(|| format!("GID {gid} exceeds 65535"))?;
    Ok((uid, gid))
}

#[cfg(unix)]
fn perm_uid_gid(meta: &fs::Metadata) -> anyhow::Result<(u16, u16, u16)> {
    use std::os::unix::fs::MetadataExt;
    let (uid, gid) = checked_owner(meta.uid(), meta.gid())?;
    Ok(((meta.mode() & 0o7777) as u16, uid, gid))
}

#[cfg(not(unix))]
fn perm_uid_gid(meta: &fs::Metadata) -> anyhow::Result<(u16, u16, u16)> {
    let perm = if meta.is_dir() { 0o755 } else { 0o644 };
    Ok((perm, 0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn read_u16_le(buf: &[u8], off: usize) -> u16 {
        u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
    }
    fn read_u32_le(buf: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
    }

    #[cfg(unix)]
    #[test]
    fn ownership_must_fit_on_disk() {
        assert_eq!(checked_owner(0, 65535).unwrap(), (0, 65535));
        assert_eq!(checked_owner(65535, 0).unwrap(), (65535, 0));
        assert!(checked_owner(65536, 0)
            .unwrap_err()
            .to_string()
            .contains("UID 65536"));
        assert!(checked_owner(0, 65536)
            .unwrap_err()
            .to_string()
            .contains("GID 65536"));
        assert!(checked_owner(u32::MAX, 0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn special_permissions_survive_serialization() {
        use std::os::unix::fs::PermissionsExt;
        for (is_dir, mode) in [(true, 0o1777), (true, 0o2775), (false, 0o4755)] {
            let src = TempDir::new().unwrap();
            let path = src.path().join("entry");
            if is_dir {
                fs::create_dir(&path).unwrap();
            } else {
                File::create(&path).unwrap();
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            let dst = TempDir::new().unwrap();
            let image = dst.path().join("image");
            let mut sysv = Sysv::new(1).unwrap();
            sysv.populate_root(src.path()).unwrap();
            sysv.write_to(&image).unwrap();
            let bytes = fs::read(image).unwrap();
            let expected_type = if is_dir { S_IFDIR } else { S_IFREG };
            assert_eq!(
                read_u16_le(&bytes, 1024 + 2 * 64),
                (expected_type | mode) as u16
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_entries_are_rejected_with_paths() {
        use std::os::unix::{fs::symlink, net::UnixListener};
        let src = TempDir::new().unwrap();
        let link = src.path().join("dangling");
        symlink("missing", &link).unwrap();
        let err = Sysv::new(1).unwrap().populate_root(src.path()).unwrap_err();
        assert!(err.to_string().contains("unsupported file type"));
        assert!(err.to_string().contains(&link.display().to_string()));
        fs::remove_file(link).unwrap();
        let socket = src.path().join("socket");
        let _listener = UnixListener::bind(&socket).unwrap();
        let err = Sysv::new(1).unwrap().populate_root(src.path()).unwrap_err();
        assert!(err.to_string().contains("unsupported file type"));
        assert!(err.to_string().contains(&socket.display().to_string()));
    }

    #[test]
    fn file_size_limit_is_checked_before_allocating_blocks() {
        let src = TempDir::new().unwrap();
        let path = src.path().join("payload");
        for size in [0, 1, 511, 512, 513, 5120, 5121, 32 * 1024 * 1024] {
            File::create(&path).unwrap().set_len(size).unwrap();
            let mut sysv = Sysv::new(1).unwrap();
            let initial_block = sysv.next_block;
            let result = sysv.populate_root(src.path());
            if size <= 5120 {
                result.unwrap();
                assert_eq!(sysv.inodes[2].size, size as u32);
                assert_eq!(sysv.next_block - initial_block, size.div_ceil(512) as u32);
            } else {
                let err = result.unwrap_err().to_string();
                assert!(err.contains("5120-byte limit"));
                assert!(err.contains(&path.display().to_string()));
                assert_eq!(sysv.next_block, initial_block);
            }
        }
    }

    #[test]
    fn empty_image_has_correct_superblock() {
        let dir = TempDir::new().unwrap();
        let img = dir.path().join("test.sysv");
        Sysv::new(1).unwrap().write_to(&img).unwrap();

        let bytes = fs::read(&img).unwrap();
        assert_eq!(bytes.len(), 1024 * 1024);

        let sb_off = BLOCK_SIZE;
        assert_eq!(read_u16_le(&bytes, sb_off), FIRST_DATA_ZONE as u16);
        assert_eq!(read_u32_le(&bytes, sb_off + 4), 2048);
        assert_eq!(&bytes[sb_off + 440..sb_off + 444], b"sysv");
        assert_eq!(&bytes[sb_off + 446..sb_off + 450], b"test");
        assert_eq!(read_u32_le(&bytes, sb_off + 504), MAGIC);
        assert_eq!(read_u32_le(&bytes, sb_off + 508), 1);
    }

    #[test]
    fn root_inode_has_dot_and_dotdot() {
        let dir = TempDir::new().unwrap();
        let img = dir.path().join("test.sysv");
        Sysv::new(1).unwrap().write_to(&img).unwrap();

        let bytes = fs::read(&img).unwrap();
        // Root inode at index 1 (ino 2 → fs_inodes[1])
        let root_off = 2 * BLOCK_SIZE + INODE_SIZE;
        let root_size = read_u32_le(&bytes, root_off + 8);
        assert_eq!(root_size, 2 * DIRENT_SIZE as u32);

        // Root data block: extracted from i_data[0..3]
        let p = &bytes[root_off + 12..root_off + 15];
        let root_data_block = u32::from(p[0]) | (u32::from(p[1]) << 8) | (u32::from(p[2]) << 16);
        let root_data_off = (root_data_block as usize) * BLOCK_SIZE;
        assert_eq!(read_u16_le(&bytes, root_data_off), ROOT_INO as u16);
        assert_eq!(bytes[root_data_off + 2], b'.');
        assert_eq!(read_u16_le(&bytes, root_data_off + 16), ROOT_INO as u16);
        assert_eq!(&bytes[root_data_off + 18..root_data_off + 20], b"..");
    }

    #[test]
    fn populate_walks_subdirs() {
        let src = TempDir::new().unwrap();
        let mut f = File::create(src.path().join("hello.txt")).unwrap();
        f.write_all(b"hi\n").unwrap();
        drop(f);
        fs::create_dir(src.path().join("sub")).unwrap();
        File::create(src.path().join("sub").join("nested.txt")).unwrap();

        let dst = TempDir::new().unwrap();
        let img = dst.path().join("out.sysv");
        let mut sysv = Sysv::new(1).unwrap();
        sysv.populate_root(src.path()).unwrap();
        let stats = sysv.write_to(&img).unwrap();

        // ninodes is fixed by layout; just check the file got created.
        assert_eq!(stats.ninodes, NINODES);
        let bytes = fs::read(&img).unwrap();
        assert_eq!(bytes.len(), 1024 * 1024);
    }

    #[test]
    fn duplicate_truncated_name_is_rejected() {
        let src = TempDir::new().unwrap();
        File::create(src.path().join("aaaaaaaaaaaaaa01")).unwrap();
        File::create(src.path().join("aaaaaaaaaaaaaa02")).unwrap();
        let mut sysv = Sysv::new(1).unwrap();
        let err = sysv.populate_root(src.path()).unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }
}
