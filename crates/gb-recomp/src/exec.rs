//! Executable memory, and the cache of translated blocks that lives in it.

use std::collections::HashMap;

/// A page-aligned region that can hold code and be jumped into.
pub struct ExecMem {
    ptr: *mut u8,
    len: usize,
}

// The pointer is owned exclusively and never handed out as a shared reference.
unsafe impl Send for ExecMem {}

#[cfg(windows)]
mod sys {
    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(addr: *mut u8, size: usize, alloc: u32, protect: u32) -> *mut u8;
        fn VirtualFree(addr: *mut u8, size: usize, free_type: u32) -> i32;
    }

    const MEM_COMMIT_RESERVE: u32 = 0x1000 | 0x2000;
    const PAGE_EXECUTE_READWRITE: u32 = 0x40;
    const MEM_RELEASE: u32 = 0x8000;

    pub unsafe fn alloc(size: usize) -> *mut u8 {
        VirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_COMMIT_RESERVE,
            PAGE_EXECUTE_READWRITE,
        )
    }

    pub unsafe fn free(ptr: *mut u8, _size: usize) {
        VirtualFree(ptr, 0, MEM_RELEASE);
    }
}

#[cfg(unix)]
mod sys {
    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }

    const PROT_RWX: i32 = 0x1 | 0x2 | 0x4;
    const MAP_PRIVATE_ANON: i32 = 0x02 | 0x20;

    pub unsafe fn alloc(size: usize) -> *mut u8 {
        let p = mmap(
            std::ptr::null_mut(),
            size,
            PROT_RWX,
            MAP_PRIVATE_ANON,
            -1,
            0,
        );
        if p as isize == -1 {
            std::ptr::null_mut()
        } else {
            p
        }
    }

    pub unsafe fn free(ptr: *mut u8, size: usize) {
        munmap(ptr, size);
    }
}

impl ExecMem {
    pub fn new(len: usize) -> Option<ExecMem> {
        let ptr = unsafe { sys::alloc(len) };
        if ptr.is_null() {
            None
        } else {
            Some(ExecMem { ptr, len })
        }
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn capacity(&self) -> usize {
        self.len
    }

    /// Copy `bytes` to `offset`. Callers keep offsets inside the arena.
    pub fn write(&mut self, offset: usize, bytes: &[u8]) {
        assert!(offset + bytes.len() <= self.len, "code arena overflow");
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(offset), bytes.len());
        }
    }

    pub fn read(&self, offset: usize, len: usize) -> &[u8] {
        assert!(offset + len <= self.len);
        unsafe { std::slice::from_raw_parts(self.ptr.add(offset), len) }
    }
}

impl Drop for ExecMem {
    fn drop(&mut self) {
        unsafe { sys::free(self.ptr, self.len) }
    }
}

/// Identifies a block. The same address holds different code depending on what
/// the mapper has selected, so the bank is part of the identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockKey {
    pub bank: u16,
    pub addr: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct BlockInfo {
    /// Byte offset of the block's entry point within the arena.
    pub offset: usize,
    pub len: usize,
    /// One past the last Game Boy address the block covers.
    pub end: u16,
}

/// A direct jump that is waiting for its destination to be translated.
#[derive(Debug, Clone, Copy)]
struct PendingLink {
    /// Absolute offset of the rel32 field in the arena.
    site: usize,
    to: BlockKey,
}

pub struct CodeCache {
    arena: ExecMem,
    used: usize,
    blocks: HashMap<BlockKey, BlockInfo>,
    unresolved: Vec<PendingLink>,
    trampoline: usize,
}

/// Default arena size. Translated code runs roughly 12-20 host bytes per Game
/// Boy instruction, so this covers a very large cartridge with room to spare.
pub const DEFAULT_ARENA: usize = 64 * 1024 * 1024;

impl CodeCache {
    pub fn new(arena_bytes: usize) -> Option<CodeCache> {
        let mut cache = CodeCache {
            arena: ExecMem::new(arena_bytes)?,
            used: 0,
            blocks: HashMap::new(),
            unresolved: Vec::new(),
            trampoline: 0,
        };
        let tramp = crate::translate::build_trampoline();
        cache.trampoline = cache.append(&tramp);
        Some(cache)
    }

    fn append(&mut self, bytes: &[u8]) -> usize {
        // Keep every block 16-byte aligned; branch targets like it and it makes
        // the arena easier to read in a debugger.
        let offset = (self.used + 15) & !15;
        self.arena.write(offset, bytes);
        self.used = offset + bytes.len();
        offset
    }

    pub fn used(&self) -> usize {
        self.used
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub fn capacity(&self) -> usize {
        self.arena.capacity()
    }

    /// Address of the Rust-callable trampoline.
    pub fn trampoline(&self) -> crate::abi::Trampoline {
        unsafe { std::mem::transmute(self.arena.as_ptr().add(self.trampoline)) }
    }

    pub fn host_ptr(&self, offset: usize) -> *const u8 {
        unsafe { self.arena.as_ptr().add(offset) }
    }

    pub fn lookup(&self, key: BlockKey) -> Option<BlockInfo> {
        self.blocks.get(&key).copied()
    }

    /// Install a freshly translated block and resolve links in both directions.
    pub fn install(&mut self, key: BlockKey, block: crate::translate::Block) -> BlockInfo {
        let offset = self.append(&block.code);
        let info = BlockInfo {
            offset,
            len: block.code.len(),
            end: block.end,
        };
        self.blocks.insert(key, info);

        for link in &block.links {
            let site = offset + link.site;
            let target = BlockKey {
                bank: key.bank,
                addr: link.target,
            };
            match self.blocks.get(&target) {
                Some(dest) => self.patch(site, dest.offset),
                None => self.unresolved.push(PendingLink { site, to: target }),
            }
        }

        // Anything that was waiting on this block can now jump straight to it.
        let mut still_waiting = Vec::with_capacity(self.unresolved.len());
        for pending in std::mem::take(&mut self.unresolved) {
            if pending.to == key {
                self.patch(pending.site, offset);
            } else {
                still_waiting.push(pending);
            }
        }
        self.unresolved = still_waiting;

        info
    }

    /// Point the rel32 at `site` to `target_offset`.
    fn patch(&mut self, site: usize, target_offset: usize) {
        let rel = target_offset as i64 - (site as i64 + 4);
        let rel = i32::try_from(rel).expect("code arena larger than a 32-bit branch");
        self.arena.write(site, &rel.to_le_bytes());
    }

    /// Forget every block whose code lives in a 256-byte page that just changed.
    /// The arena is not reclaimed; the entries simply stop being reachable, and
    /// nothing links directly into RAM code, so no stale jump can survive.
    pub fn invalidate_page(&mut self, page: u8) {
        let lo = (page as u16) << 8;
        let hi = lo | 0xff;
        self.blocks
            .retain(|key, info| !(key.addr <= hi && info.end.wrapping_sub(1) >= lo));
        self.unresolved
            .retain(|link| !(link.to.addr >= lo && link.to.addr <= hi));
    }

    /// The arena contents and block map, for stamping into an executable.
    ///
    /// Translated code contains no absolute addresses, so the bytes are valid at
    /// whatever address the loader later puts them.
    pub fn export(&self) -> (Vec<u8>, Vec<(BlockKey, u32, u32)>) {
        let mut map: Vec<(BlockKey, u32, u32)> = self
            .blocks
            .iter()
            .map(|(&key, info)| (key, info.offset as u32, info.end as u32))
            .collect();
        map.sort_unstable();
        (self.arena.read(0, self.used).to_vec(), map)
    }

    /// Reinstate an exported arena. `blob` must have come from `export` on a
    /// cache built for the same cartridge.
    pub fn import(&mut self, blob: &[u8], map: &[(BlockKey, u32, u32)]) {
        self.arena.write(0, blob);
        self.used = blob.len();
        for &(key, offset, end) in map {
            self.blocks.insert(
                key,
                BlockInfo {
                    offset: offset as usize,
                    len: 0,
                    end: end as u16,
                },
            );
        }
    }
}
