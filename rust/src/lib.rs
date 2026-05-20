use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use thiserror::Error;

// On-disk format constants. Changing any of these requires an explicit
// migration/versioning strategy because persisted images depend on them.
const MAGIC: u32 = 0x5553_4653;
const VERSION: u32 = 1;
const BLOCK_SIZE: u32 = 4096;
const INODE_SIZE: usize = 128;
const DIRENT_SIZE: usize = 264;
const DIRECT_PTRS: usize = 12;
const INODE_COUNT: u32 = 1024;
const JOURNAL_BLOCKS: u32 = 64;

#[derive(Debug, Error)]
enum FsError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt filesystem")]
    Corrupt,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("not a directory: {0}")]
    NotDir(String),
    #[error("is a directory: {0}")]
    IsDir(String),
    #[error("no space")]
    NoSpace,
    #[error("invalid path: {0}")]
    InvalidPath(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InodeType {
    Free = 0,
    File = 1,
    Dir = 2,
}

#[derive(Clone, Copy)]
struct Superblock {
    total_blocks: u32,
    journal_start: u32,
    journal_blocks: u32,
    inode_bitmap_start: u32,
    inode_bitmap_blocks: u32,
    block_bitmap_start: u32,
    block_bitmap_blocks: u32,
    inode_table_start: u32,
    inode_table_blocks: u32,
    data_start: u32,
    clean: u32,
    sequence: u32,
}

#[derive(Clone)]
struct Inode {
    id: u32,
    kind: InodeType,
    links: u8,
    size: u32,
    direct: [u32; DIRECT_PTRS],
}

#[derive(Clone)]
struct Dirent {
    active: u8,
    kind: InodeType,
    inode_id: u32,
    name: String,
}

struct OpenHandle {
    inode_id: u32,
    offset: usize,
}

#[pyclass]
/// Python-visible filesystem object backed by a single disk image file.
pub struct PyUserSpaceFs {
    file: File,
    sb: Superblock,
    next_fd: i32,
    open: HashMap<i32, OpenHandle>,
}

#[pymethods]
impl PyUserSpaceFs {
    #[staticmethod]
    pub fn format(image_path: &str, total_blocks: u32) -> PyResult<()> {
        // Delegate to internal formatter and convert domain error into PyErr.
        format_fs(image_path, total_blocks).map_err(to_py)
    }

    #[staticmethod]
    pub fn mount(image_path: &str) -> PyResult<Self> {
        // Mount returns a fully initialized Python-visible filesystem object.
        mount_fs(image_path).map_err(to_py)
    }

    pub fn unmount(&mut self) -> PyResult<()> {
        // Mark image clean before shutdown so next mount can detect clean exit.
        self.sb.clean = 1;
        self.sb.sequence = self.sb.sequence.wrapping_add(1);
        // Persist updated superblock and force writeback.
        write_superblock(&mut self.file, &self.sb).map_err(to_py)?;
        self.file.flush().map_err(|e| to_py(FsError::Io(e)))?;
        Ok(())
    }

    pub fn mkdir(&mut self, path: &str) -> PyResult<()> {
        // Directory creation is the same create pipeline with Dir inode type.
        create_node(self, path, InodeType::Dir).map_err(to_py)
    }

    pub fn create(&mut self, path: &str) -> PyResult<()> {
        // Regular file creation uses the same create pipeline.
        create_node(self, path, InodeType::File).map_err(to_py)
    }

    pub fn remove(&mut self, path: &str) -> PyResult<()> {
        // Remove handles both files and empty directories.
        remove_node(self, path).map_err(to_py)
    }

    pub fn readdir(&mut self, path: &str) -> PyResult<Vec<String>> {
        // Resolve path to directory inode id.
        let inode_id = resolve_path(self, path).map_err(to_py)?;
        let inode = read_inode(&mut self.file, &self.sb, inode_id).map_err(to_py)?;
        // Enforce directory-only semantics for readdir.
        if inode.kind != InodeType::Dir {
            return Err(to_py(FsError::NotDir(path.to_string())));
        }
        let entries = read_dir_entries(self, &inode).map_err(to_py)?;
        // Hide internal "."/".." entries from external callers.
        Ok(entries
            .into_iter()
            // Expose only user-created names.
            .filter(|e| e.active == 1 && e.name != "." && e.name != "..")
            .map(|e| e.name)
            .collect())
    }

    pub fn open(&mut self, path: &str) -> PyResult<i32> {
        // Resolve name to inode and validate it is not a directory.
        let inode_id = resolve_path(self, path).map_err(to_py)?;
        let inode = read_inode(&mut self.file, &self.sb, inode_id).map_err(to_py)?;
        if inode.kind == InodeType::Dir {
            return Err(to_py(FsError::IsDir(path.to_string())));
        }
        // Allocate user-space file descriptor and remember cursor offset.
        let fd = self.next_fd;
        self.next_fd += 1;
        self.open.insert(fd, OpenHandle { inode_id, offset: 0 });
        Ok(fd)
    }

    pub fn close(&mut self, fd: i32) -> PyResult<()> {
        // Remove handle mapping; missing handle is treated as caller error.
        self.open
            .remove(&fd)
            .ok_or_else(|| to_py(FsError::NotFound(format!("fd {}", fd))))?;
        Ok(())
    }

    pub fn read(&mut self, fd: i32, length: usize) -> PyResult<Vec<u8>> {
        // Load current descriptor cursor state.
        let (inode_id, offset) = {
            let h = self
                .open
                .get(&fd)
                .ok_or_else(|| to_py(FsError::NotFound(format!("fd {}", fd))))?;
            (h.inode_id, h.offset)
        };
        // Read file payload and slice requested range.
        let data = read_file_bytes(self, inode_id).map_err(to_py)?;
        // Clamp requested read to end-of-file.
        let end = usize::min(offset + length, data.len());
        let out = data[offset..end].to_vec();
        // Advance descriptor cursor by actual bytes returned.
        if let Some(h) = self.open.get_mut(&fd) {
            h.offset = end;
        }
        Ok(out)
    }

    pub fn write(&mut self, fd: i32, data: Vec<u8>) -> PyResult<usize> {
        // Resolve descriptor to target inode and current cursor.
        let (inode_id, offset) = {
            let h = self
                .open
                .get(&fd)
                .ok_or_else(|| to_py(FsError::NotFound(format!("fd {}", fd))))?;
            (h.inode_id, h.offset)
        };
        // Expand in-memory payload when write extends file length.
        let mut current = read_file_bytes(self, inode_id).map_err(to_py)?;
        let next_len = usize::max(current.len(), offset + data.len());
        // Grow buffer if write extends current file.
        if current.len() < next_len {
            current.resize(next_len, 0);
        }
        // Overwrite target byte range at current cursor.
        current[offset..offset + data.len()].copy_from_slice(&data);
        // Wrap metadata/data mutation with journal markers.
        append_journal_marker(self, b"TX_BEGIN").map_err(to_py)?;
        write_file_bytes(self, inode_id, &current).map_err(to_py)?;
        append_journal_marker(self, b"TX_COMMIT").map_err(to_py)?;
        // Move descriptor cursor past written bytes.
        // Descriptor should exist; this is defensive in case map changes.
        if let Some(h) = self.open.get_mut(&fd) {
            h.offset = offset + data.len();
        }
        Ok(data.len())
    }

    pub fn truncate(&mut self, path: &str, size: usize) -> PyResult<()> {
        // Resolve path and reject truncating directories.
        let inode_id = resolve_path(self, path).map_err(to_py)?;
        let inode = read_inode(&mut self.file, &self.sb, inode_id).map_err(to_py)?;
        if inode.kind == InodeType::Dir {
            return Err(to_py(FsError::IsDir(path.to_string())));
        }
        // Resize logical payload, zero-filling when growing.
        let mut content = read_file_bytes(self, inode_id).map_err(to_py)?;
        // Resize with zero fill for growth.
        content.resize(size, 0);
        // Persist mutation through write path.
        append_journal_marker(self, b"TX_BEGIN").map_err(to_py)?;
        write_file_bytes(self, inode_id, &content).map_err(to_py)?;
        append_journal_marker(self, b"TX_COMMIT").map_err(to_py)?;
        Ok(())
    }
}

fn to_py(err: FsError) -> PyErr {
    // Use RuntimeError for now; wrapper layer refines this into typed Python errors.
    PyRuntimeError::new_err(err.to_string())
}

/// Create a new image and initialize superblock/bitmaps/root directory.
fn format_fs(image_path: &str, total_blocks: u32) -> Result<(), FsError> {
    // Create or overwrite the backing disk image.
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(image_path)?;
    // Pre-allocate the full image size in bytes.
    file.set_len(total_blocks as u64 * BLOCK_SIZE as u64)?;

    // Compute region sizes for inode bitmap, block bitmap, and inode table.
    let inode_bitmap_blocks = 1;
    let block_bitmap_blocks =
        ((total_blocks as usize + (BLOCK_SIZE as usize * 8 - 1)) / (BLOCK_SIZE as usize * 8)) as u32;
    let inode_table_blocks =
        ((INODE_COUNT as usize * INODE_SIZE + BLOCK_SIZE as usize - 1) / BLOCK_SIZE as usize) as u32;
    let sb = Superblock {
        total_blocks,
        journal_start: 1,
        journal_blocks: JOURNAL_BLOCKS,
        inode_bitmap_start: 1 + JOURNAL_BLOCKS,
        inode_bitmap_blocks,
        block_bitmap_start: 1 + JOURNAL_BLOCKS + inode_bitmap_blocks,
        block_bitmap_blocks,
        inode_table_start: 1 + JOURNAL_BLOCKS + inode_bitmap_blocks + block_bitmap_blocks,
        inode_table_blocks,
        data_start: 1 + JOURNAL_BLOCKS + inode_bitmap_blocks + block_bitmap_blocks + inode_table_blocks,
        clean: 1,
        sequence: 1,
    };
    // Ensure there is at least one data block after metadata regions.
    if sb.data_start >= total_blocks {
        return Err(FsError::NoSpace);
    }
    // Persist superblock first so later writes have a valid layout to target.
    write_superblock(&mut file, &sb)?;
    // Reserve inode 0 for root.
    set_bitmap_bit(&mut file, sb.inode_bitmap_start, 0, true)?;

    // Build root directory inode.
    let mut root = Inode {
        id: 0,
        kind: InodeType::Dir,
        links: 2,
        size: 0,
        direct: [0; DIRECT_PTRS],
    };
    // Allocate first data block for root directory entries.
    let blk = alloc_data_block(&mut file, &sb)?;
    root.direct[0] = blk;
    // Seed "." and ".." entries in root.
    let dot = encode_dirent(&Dirent {
        active: 1,
        kind: InodeType::Dir,
        inode_id: 0,
        name: ".".to_string(),
    });
    let dotdot = encode_dirent(&Dirent {
        active: 1,
        kind: InodeType::Dir,
        inode_id: 0,
        name: "..".to_string(),
    });
    write_block(&mut file, blk, &[dot, dotdot].concat())?;
    // Directory size tracks payload bytes, not number of entries.
    root.size = (2 * DIRENT_SIZE) as u32;
    write_inode(&mut file, &sb, &root)?;
    // Flush all metadata and root directory content.
    file.flush()?;
    Ok(())
}

/// Open an existing image, replay pending journal markers, and mark dirty.
fn mount_fs(image_path: &str) -> Result<PyUserSpaceFs, FsError> {
    // Open in read-write mode because mount mutates clean flag/journal state.
    let mut file = OpenOptions::new().read(true).write(true).open(image_path)?;
    // Load superblock to discover region layout.
    let mut sb = read_superblock(&mut file)?;
    // Validate and clear journal markers from previous run.
    replay_journal(&mut file, &sb)?;
    // Mark filesystem "dirty" while mounted.
    sb.clean = 0;
    sb.sequence = sb.sequence.wrapping_add(1);
    write_superblock(&mut file, &sb)?;
    Ok(PyUserSpaceFs {
        file,
        sb,
        next_fd: 3,
        open: HashMap::new(),
    })
}

/// Shared create path for both regular files and directories.
fn create_node(fs: &mut PyUserSpaceFs, path: &str, kind: InodeType) -> Result<(), FsError> {
    // Resolve parent directory and final path component.
    let (parent_id, leaf) = resolve_parent(fs, path)?;
    // Fail fast if name already exists.
    if lookup_dir(fs, parent_id, &leaf)?.is_some() {
        return Err(FsError::AlreadyExists(path.to_string()));
    }
    // Allocate inode and initialize default metadata.
    let inode_id = alloc_inode(&mut fs.file, &fs.sb)?;
    let mut node = Inode {
        id: inode_id,
        kind,
        links: if kind == InodeType::Dir { 2 } else { 1 },
        size: 0,
        direct: [0; DIRECT_PTRS],
    };
    // Directory creation needs initial self/parent links.
    if kind == InodeType::Dir {
        // Directories are created with "." and ".." entries.
        let blk = alloc_data_block(&mut fs.file, &fs.sb)?;
        node.direct[0] = blk;
        let dot = encode_dirent(&Dirent {
            active: 1,
            kind: InodeType::Dir,
            inode_id,
            name: ".".to_string(),
        });
        let dotdot = encode_dirent(&Dirent {
            active: 1,
            kind: InodeType::Dir,
            inode_id: parent_id,
            name: "..".to_string(),
        });
        write_block(&mut fs.file, blk, &[dot, dotdot].concat())?;
        node.size = (2 * DIRENT_SIZE) as u32;
    }
    // Metadata updates are wrapped with lightweight begin/commit markers.
    // V1 journal stores markers rather than full logical redo records.
    append_journal_marker(fs, b"TX_BEGIN")?;
    write_inode(&mut fs.file, &fs.sb, &node)?;
    // Link new inode into parent directory namespace.
    insert_dirent(fs, parent_id, Dirent { active: 1, kind, inode_id, name: leaf })?;
    append_journal_marker(fs, b"TX_COMMIT")?;
    Ok(())
}

/// Remove a file or empty directory and release all associated resources.
fn remove_node(fs: &mut PyUserSpaceFs, path: &str) -> Result<(), FsError> {
    // Resolve object and parent directory where the name is stored.
    let (parent_id, leaf) = resolve_parent(fs, path)?;
    let target_id = lookup_dir(fs, parent_id, &leaf)?.ok_or_else(|| FsError::NotFound(path.to_string()))?;
    let target = read_inode(&mut fs.file, &fs.sb, target_id)?;
    // Enforce "rmdir only if empty".
    if target.kind == InodeType::Dir {
        // Non-empty directories are not removable in this implementation.
        let ents = read_dir_entries(fs, &target)?;
        let active_children = ents
            .into_iter()
            .filter(|e| e.active == 1 && e.name != "." && e.name != "..")
            .count();
        if active_children > 0 {
            return Err(FsError::InvalidPath("directory not empty".to_string()));
        }
    }
    append_journal_marker(fs, b"TX_BEGIN")?;
    // Remove namespace link first.
    remove_dirent(fs, parent_id, &leaf)?;
    // Then free payload blocks and inode slot.
    free_inode_data(fs, &target)?;
    set_bitmap_bit(&mut fs.file, fs.sb.inode_bitmap_start, target_id as usize, false)?;
    let empty = Inode {
        id: target_id,
        kind: InodeType::Free,
        links: 0,
        size: 0,
        direct: [0; DIRECT_PTRS],
    };
    write_inode(&mut fs.file, &fs.sb, &empty)?;
    append_journal_marker(fs, b"TX_COMMIT")?;
    Ok(())
}

fn resolve_path(fs: &mut PyUserSpaceFs, path: &str) -> Result<u32, FsError> {
    // Root is a special case.
    if path == "/" {
        return Ok(0);
    }
    let mut cur = 0;
    // Walk each path component from root.
    for p in split_path(path)? {
        cur = lookup_dir(fs, cur, p)?.ok_or_else(|| FsError::NotFound(path.to_string()))?;
    }
    Ok(cur)
}

/// Resolve `/a/b/c` into `(inode(/a/b), "c")`.
fn resolve_parent(fs: &mut PyUserSpaceFs, path: &str) -> Result<(u32, String), FsError> {
    let parts = split_path(path)?;
    // Root has no parent/leaf pair in this API.
    if parts.is_empty() {
        return Err(FsError::InvalidPath(path.to_string()));
    }
    let mut cur = 0;
    // Traverse all components except the leaf.
    for p in &parts[..parts.len() - 1] {
        cur = lookup_dir(fs, cur, p)?.ok_or_else(|| FsError::NotFound(path.to_string()))?;
        let inode = read_inode(&mut fs.file, &fs.sb, cur)?;
        // Parent chain must always remain directory-typed.
        if inode.kind != InodeType::Dir {
            return Err(FsError::NotDir(path.to_string()));
        }
    }
    Ok((cur, parts[parts.len() - 1].to_string()))
}

/// Parse absolute paths; relative paths are rejected by design.
fn split_path(path: &str) -> Result<Vec<&str>, FsError> {
    // Require canonical absolute paths for deterministic traversal.
    if !path.starts_with('/') {
        return Err(FsError::InvalidPath(path.to_string()));
    }
    // Ignore repeated or trailing separators by filtering empty segments.
    Ok(path.split('/').filter(|x| !x.is_empty()).collect())
}

fn lookup_dir(fs: &mut PyUserSpaceFs, dir_inode: u32, name: &str) -> Result<Option<u32>, FsError> {
    // Fetch directory inode metadata to ensure caller passed a directory.
    let inode = read_inode(&mut fs.file, &fs.sb, dir_inode)?;
    if inode.kind != InodeType::Dir {
        return Err(FsError::NotDir(name.to_string()));
    }
    // Linear scan through active entries.
    for e in read_dir_entries(fs, &inode)? {
        if e.active == 1 && e.name == name {
            return Ok(Some(e.inode_id));
        }
    }
    Ok(None)
}

/// Read active directory entries from direct blocks up to inode.size.
fn read_dir_entries(fs: &mut PyUserSpaceFs, dir: &Inode) -> Result<Vec<Dirent>, FsError> {
    let mut out = Vec::new();
    // Use inode.size to avoid decoding unused bytes.
    let mut remaining = dir.size as usize;
    // Iterate only populated direct pointers.
    for ptr in dir.direct.iter().copied().filter(|x| *x != 0) {
        let blk = read_block(&mut fs.file, ptr)?;
        let mut off = 0usize;
        // Decode fixed-size dirent records from each populated block.
        while remaining >= DIRENT_SIZE && off + DIRENT_SIZE <= blk.len() {
            out.push(decode_dirent(&blk[off..off + DIRENT_SIZE])?);
            off += DIRENT_SIZE;
            remaining -= DIRENT_SIZE;
        }
        if remaining == 0 {
            break;
        }
    }
    Ok(out)
}

/// Append a new directory record to the parent directory payload.
fn insert_dirent(fs: &mut PyUserSpaceFs, parent_id: u32, ent: Dirent) -> Result<(), FsError> {
    let mut parent = read_inode(&mut fs.file, &fs.sb, parent_id)?;
    // Compute append location based on byte-sized directory payload.
    let block_index = (parent.size as usize) / BLOCK_SIZE as usize;
    let offset_in_block = (parent.size as usize) % BLOCK_SIZE as usize;
    // Directory cannot grow beyond direct pointer capacity in this version.
    if block_index >= DIRECT_PTRS {
        return Err(FsError::NoSpace);
    }
    if parent.direct[block_index] == 0 {
        // Allocate backing block lazily when first needed.
        parent.direct[block_index] = alloc_data_block(&mut fs.file, &fs.sb)?;
    }
    let mut blk = read_block(&mut fs.file, parent.direct[block_index])?;
    let bytes = encode_dirent(&ent);
    // Sanity guard for fixed-size dirent encoding.
    if offset_in_block + bytes.len() > blk.len() {
        return Err(FsError::NoSpace);
    }
    blk[offset_in_block..offset_in_block + bytes.len()].copy_from_slice(&bytes);
    write_block(&mut fs.file, parent.direct[block_index], &blk)?;
    // Grow directory logical size by one dirent record.
    parent.size += DIRENT_SIZE as u32;
    write_inode(&mut fs.file, &fs.sb, &parent)?;
    Ok(())
}

/// Tombstone a directory entry in place (does not compact directory payload).
fn remove_dirent(fs: &mut PyUserSpaceFs, parent_id: u32, name: &str) -> Result<(), FsError> {
    let parent = read_inode(&mut fs.file, &fs.sb, parent_id)?;
    let mut remaining = parent.size as usize;
    // Search and tombstone matching active entry in place.
    // Scan each populated directory block.
    for ptr in parent.direct.iter().copied().filter(|x| *x != 0) {
        let mut blk = read_block(&mut fs.file, ptr)?;
        let mut off = 0usize;
        while remaining >= DIRENT_SIZE && off + DIRENT_SIZE <= blk.len() {
            let mut d = decode_dirent(&blk[off..off + DIRENT_SIZE])?;
            // Match active entry by name and tombstone it.
            if d.active == 1 && d.name == name {
                d.active = 0;
                blk[off..off + DIRENT_SIZE].copy_from_slice(&encode_dirent(&d));
                write_block(&mut fs.file, ptr, &blk)?;
                return Ok(());
            }
            off += DIRENT_SIZE;
            remaining -= DIRENT_SIZE;
        }
    }
    Err(FsError::NotFound(name.to_string()))
}

/// Materialize full file content into memory from direct data blocks.
fn read_file_bytes(fs: &mut PyUserSpaceFs, inode_id: u32) -> Result<Vec<u8>, FsError> {
    let inode = read_inode(&mut fs.file, &fs.sb, inode_id)?;
    // Directory payload is not readable through file API.
    if inode.kind == InodeType::Dir {
        return Err(FsError::IsDir(format!("{}", inode_id)));
    }
    // Materialize logical file length into memory.
    let mut out = vec![0u8; inode.size as usize];
    let mut copied = 0usize;
    // Stream direct blocks in order into output buffer.
    for ptr in inode.direct.iter().copied().filter(|x| *x != 0) {
        if copied >= out.len() {
            break;
        }
        let blk = read_block(&mut fs.file, ptr)?;
        // Copy only the remaining logical byte count from the current block.
        let take = usize::min(BLOCK_SIZE as usize, out.len() - copied);
        out[copied..copied + take].copy_from_slice(&blk[..take]);
        copied += take;
    }
    Ok(out)
}

/// Rewrite full file content and adjust block allocation for new length.
fn write_file_bytes(fs: &mut PyUserSpaceFs, inode_id: u32, data: &[u8]) -> Result<(), FsError> {
    let mut inode = read_inode(&mut fs.file, &fs.sb, inode_id)?;
    // Directory payload cannot be written through file API.
    if inode.kind == InodeType::Dir {
        return Err(FsError::IsDir(format!("{}", inode_id)));
    }
    // Determine how many direct blocks are needed for new payload length.
    let needed_blocks = (data.len() + BLOCK_SIZE as usize - 1) / BLOCK_SIZE as usize;
    // Write each required block chunk.
    for i in 0..needed_blocks {
        if i >= DIRECT_PTRS {
            return Err(FsError::NoSpace);
        }
        if inode.direct[i] == 0 {
            // Allocate block on demand when extending file.
            inode.direct[i] = alloc_data_block(&mut fs.file, &fs.sb)?;
        }
        let start = i * BLOCK_SIZE as usize;
        let end = usize::min(start + BLOCK_SIZE as usize, data.len());
        write_block(&mut fs.file, inode.direct[i], &data[start..end])?;
    }
    // Release surplus blocks if file shrank.
    // Sweep tail pointers and release blocks no longer needed.
    for i in needed_blocks..DIRECT_PTRS {
        if inode.direct[i] != 0 {
            set_bitmap_bit(
                &mut fs.file,
                fs.sb.block_bitmap_start,
                (inode.direct[i] - fs.sb.data_start) as usize,
                false,
            )?;
            inode.direct[i] = 0;
        }
    }
    inode.size = data.len() as u32;
    write_inode(&mut fs.file, &fs.sb, &inode)?;
    Ok(())
}

/// Free all direct data blocks referenced by an inode.
fn free_inode_data(fs: &mut PyUserSpaceFs, inode: &Inode) -> Result<(), FsError> {
    // Free each referenced direct block.
    for ptr in inode.direct.iter().copied().filter(|x| *x != 0) {
        set_bitmap_bit(
            &mut fs.file,
            fs.sb.block_bitmap_start,
            (ptr - fs.sb.data_start) as usize,
            false,
        )?;
    }
    Ok(())
}

/// Append a journal marker in a single journal block ring-like region.
fn append_journal_marker(fs: &mut PyUserSpaceFs, marker: &[u8]) -> Result<(), FsError> {
    let mut blk = read_block(&mut fs.file, fs.sb.journal_start)?;
    let mut off = 0usize;
    // Find end of existing journal marker sequence.
    while off + 4 <= blk.len() {
        let len = u32::from_le_bytes([blk[off], blk[off + 1], blk[off + 2], blk[off + 3]]) as usize;
        // Zero length marks end of marker stream.
        if len == 0 {
            break;
        }
        off += 4 + len;
    }
    if off + 4 + marker.len() > blk.len() {
        // If marker will not fit, clear block and restart marker stream.
        blk.fill(0);
        off = 0;
    }
    blk[off..off + 4].copy_from_slice(&(marker.len() as u32).to_le_bytes());
    blk[off + 4..off + 4 + marker.len()].copy_from_slice(marker);
    write_block(&mut fs.file, fs.sb.journal_start, &blk)?;
    Ok(())
}

/// Validate begin/commit pairing and clear journal region on mount.
fn replay_journal(file: &mut File, sb: &Superblock) -> Result<(), FsError> {
    let mut blk = read_block(file, sb.journal_start)?;
    let mut saw_begin = false;
    let mut saw_commit = false;
    let mut off = 0usize;
    // Parse marker records encoded as [u32 len][bytes payload].
    while off + 4 <= blk.len() {
        let len = u32::from_le_bytes([blk[off], blk[off + 1], blk[off + 2], blk[off + 3]]) as usize;
        if len == 0 || off + 4 + len > blk.len() {
            break;
        }
        let rec = &blk[off + 4..off + 4 + len];
        // Track begin marker occurrence.
        if rec == b"TX_BEGIN" {
            saw_begin = true;
        // Track commit marker occurrence.
        } else if rec == b"TX_COMMIT" {
            saw_commit = true;
        }
        off += 4 + len;
    }
    if saw_begin && !saw_commit {
        // Reject image if transaction appears interrupted mid-flight.
        return Err(FsError::Corrupt);
    }
    // Journal region is always reset after mount-time scan.
    blk.fill(0);
    write_block(file, sb.journal_start, &blk)?;
    Ok(())
}

fn read_superblock(file: &mut File) -> Result<Superblock, FsError> {
    let blk = read_block(file, 0)?;
    // Validate magic/version before using any offsets from disk.
    if read_u32(&blk, 0) != MAGIC || read_u32(&blk, 4) != VERSION {
        return Err(FsError::Corrupt);
    }
    Ok(Superblock {
        // Decode fixed offset fields from superblock block.
        total_blocks: read_u32(&blk, 12),
        journal_start: read_u32(&blk, 16),
        journal_blocks: read_u32(&blk, 20),
        inode_bitmap_start: read_u32(&blk, 24),
        inode_bitmap_blocks: read_u32(&blk, 28),
        block_bitmap_start: read_u32(&blk, 32),
        block_bitmap_blocks: read_u32(&blk, 36),
        inode_table_start: read_u32(&blk, 40),
        inode_table_blocks: read_u32(&blk, 44),
        data_start: read_u32(&blk, 48),
        clean: read_u32(&blk, 52),
        sequence: read_u32(&blk, 56),
    })
}

fn write_superblock(file: &mut File, sb: &Superblock) -> Result<(), FsError> {
    // Start from zero-filled block so undefined bytes are deterministic.
    let mut blk = vec![0u8; BLOCK_SIZE as usize];
    // Encode header and region layout fields at fixed offsets.
    // Header identity fields.
    write_u32(&mut blk, 0, MAGIC);
    write_u32(&mut blk, 4, VERSION);
    write_u32(&mut blk, 8, BLOCK_SIZE);
    // Capacity and region boundaries.
    write_u32(&mut blk, 12, sb.total_blocks);
    write_u32(&mut blk, 16, sb.journal_start);
    write_u32(&mut blk, 20, sb.journal_blocks);
    write_u32(&mut blk, 24, sb.inode_bitmap_start);
    write_u32(&mut blk, 28, sb.inode_bitmap_blocks);
    write_u32(&mut blk, 32, sb.block_bitmap_start);
    write_u32(&mut blk, 36, sb.block_bitmap_blocks);
    write_u32(&mut blk, 40, sb.inode_table_start);
    write_u32(&mut blk, 44, sb.inode_table_blocks);
    write_u32(&mut blk, 48, sb.data_start);
    // Runtime state flags.
    write_u32(&mut blk, 52, sb.clean);
    write_u32(&mut blk, 56, sb.sequence);
    write_block(file, 0, &blk)?;
    Ok(())
}

fn inode_offset(sb: &Superblock, inode_id: u32) -> (u32, usize) {
    // Inode table is a packed fixed-width array.
    let byte = inode_id as usize * INODE_SIZE;
    let blk = sb.inode_table_start + (byte / BLOCK_SIZE as usize) as u32;
    (blk, byte % BLOCK_SIZE as usize)
}

fn read_inode(file: &mut File, sb: &Superblock, inode_id: u32) -> Result<Inode, FsError> {
    // Locate inode record by id and read containing block.
    let (blk_no, off) = inode_offset(sb, inode_id);
    let blk = read_block(file, blk_no)?;
    let raw = &blk[off..off + INODE_SIZE];
    let mut direct = [0u32; DIRECT_PTRS];
    // Deserialize direct block pointers.
    for (i, p) in direct.iter_mut().enumerate() {
        *p = read_u32(raw, 16 + i * 4);
    }
    Ok(Inode {
        // Core inode identity and type.
        id: read_u32(raw, 0),
        kind: match raw[4] {
            1 => InodeType::File,
            2 => InodeType::Dir,
            _ => InodeType::Free,
        },
        // Link count and logical byte length.
        links: raw[5],
        size: read_u32(raw, 8),
        // Direct data block pointers.
        direct,
    })
}

fn write_inode(file: &mut File, sb: &Superblock, inode: &Inode) -> Result<(), FsError> {
    // Read-modify-write enclosing block because inodes are packed.
    let (blk_no, off) = inode_offset(sb, inode.id);
    let mut blk = read_block(file, blk_no)?;
    let mut raw = vec![0u8; INODE_SIZE];
    write_u32(&mut raw, 0, inode.id);
    // Encode scalar inode fields first.
    raw[4] = inode.kind as u8;
    raw[5] = inode.links;
    write_u32(&mut raw, 8, inode.size);
    // Encode direct pointers into fixed slots.
    for (i, p) in inode.direct.iter().enumerate() {
        write_u32(&mut raw, 16 + i * 4, *p);
    }
    // Splice encoded inode back into inode table block.
    blk[off..off + INODE_SIZE].copy_from_slice(&raw);
    write_block(file, blk_no, &blk)?;
    Ok(())
}

fn encode_dirent(ent: &Dirent) -> Vec<u8> {
    // Fixed-width encoding keeps directory scans simple and deterministic.
    let mut b = vec![0u8; DIRENT_SIZE];
    b[0] = ent.active;
    b[1] = ent.kind as u8;
    let bytes = ent.name.as_bytes();
    // Truncate long names to on-disk maximum.
    let len = bytes.len().min(255);
    b[2..4].copy_from_slice(&(len as u16).to_le_bytes());
    b[4..8].copy_from_slice(&ent.inode_id.to_le_bytes());
    b[8..8 + len].copy_from_slice(&bytes[..len]);
    b
}

fn decode_dirent(raw: &[u8]) -> Result<Dirent, FsError> {
    // Validate name length before slicing.
    let len = u16::from_le_bytes([raw[2], raw[3]]) as usize;
    if 8 + len > raw.len() {
        return Err(FsError::Corrupt);
    }
    Ok(Dirent {
        // Active tombstone flag and inode type.
        active: raw[0],
        kind: match raw[1] {
            1 => InodeType::File,
            2 => InodeType::Dir,
            _ => InodeType::Free,
        },
        // Target inode id and decoded UTF-8-ish name.
        inode_id: read_u32(raw, 4),
        name: String::from_utf8_lossy(&raw[8..8 + len]).to_string(),
    })
}

fn alloc_inode(file: &mut File, sb: &Superblock) -> Result<u32, FsError> {
    // Inode id is the bit index in inode bitmap.
    alloc_from_bitmap(file, sb.inode_bitmap_start, sb.inode_bitmap_blocks).map(|v| v as u32)
}

fn alloc_data_block(file: &mut File, sb: &Superblock) -> Result<u32, FsError> {
    // Data bitmap index is translated into absolute block number.
    alloc_from_bitmap(file, sb.block_bitmap_start, sb.block_bitmap_blocks)
        .map(|v| sb.data_start + v as u32)
}

/// Linear bitmap allocator: first free bit wins.
fn alloc_from_bitmap(file: &mut File, start: u32, blocks: u32) -> Result<usize, FsError> {
    // Scan bitmap block-by-block.
    for bi in 0..blocks {
        let mut blk = read_block(file, start + bi)?;
        // Scan each byte for available bits.
        for i in 0..blk.len() {
            if blk[i] != 0xFF {
                // Scan bits in this byte from LSB to MSB.
                for bit in 0..8 {
                    if (blk[i] & (1 << bit)) == 0 {
                        // Claim first free bit and persist immediately.
                        blk[i] |= 1 << bit;
                        write_block(file, start + bi, &blk)?;
                        return Ok((bi as usize * BLOCK_SIZE as usize + i) * 8 + bit);
                    }
                }
            }
        }
    }
    Err(FsError::NoSpace)
}

fn set_bitmap_bit(file: &mut File, start_block: u32, index: usize, on: bool) -> Result<(), FsError> {
    // Convert logical bit index into bitmap block and byte/bit offsets.
    let byte = index / 8;
    let bit = index % 8;
    let blk_no = start_block + (byte / BLOCK_SIZE as usize) as u32;
    let off = byte % BLOCK_SIZE as usize;
    let mut blk = read_block(file, blk_no)?;
    // Turn target bit on/off as requested.
    if on {
        blk[off] |= 1 << bit;
    } else {
        blk[off] &= !(1 << bit);
    }
    // Persist bit toggle back to bitmap block.
    write_block(file, blk_no, &blk)?;
    Ok(())
}

/// Raw block read helper; all higher-level structures build on this.
fn read_block(file: &mut File, block_no: u32) -> Result<Vec<u8>, FsError> {
    let mut out = vec![0u8; BLOCK_SIZE as usize];
    // Seek/read exact block boundary.
    file.seek(SeekFrom::Start(block_no as u64 * BLOCK_SIZE as u64))?;
    file.read_exact(&mut out)?;
    Ok(out)
}

/// Raw block write helper. Writes are padded/truncated to exact block size.
fn write_block(file: &mut File, block_no: u32, bytes: &[u8]) -> Result<(), FsError> {
    let mut out = vec![0u8; BLOCK_SIZE as usize];
    // Copy as much payload as fits in one block.
    let n = usize::min(out.len(), bytes.len());
    out[..n].copy_from_slice(&bytes[..n]);
    // Always write full blocks to maintain a block-device contract.
    file.seek(SeekFrom::Start(block_no as u64 * BLOCK_SIZE as u64))?;
    file.write_all(&out)?;
    file.flush()?;
    Ok(())
}

fn write_u32(b: &mut [u8], off: usize, v: u32) {
    // Little-endian layout is fixed for portability across hosts.
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn read_u32(b: &[u8], off: usize) -> u32 {
    // Mirror of write_u32 for fixed-width field decoding.
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[pymodule]
fn userspace_fs(_py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Export extension class into module namespace.
    m.add_class::<PyUserSpaceFs>()?;
    Ok(())
}
