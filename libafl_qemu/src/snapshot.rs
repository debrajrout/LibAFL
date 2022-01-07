use bio::data_structures::interval_tree::IntervalTree;
use libafl::{executors::ExitKind, inputs::Input, observers::ObserversTuple, state::HasMetadata};
use std::collections::HashMap;

use crate::{
    emu::{Emulator, MmapPerms},
    executor::QemuExecutor,
    helper::{QemuHelper, QemuHelperTuple},
    SYS_mmap, SYS_mprotect, SYS_mremap,
};

pub const SNAPSHOT_PAGE_SIZE: usize = 4096;

#[derive(Debug)]
pub struct SnapshotPageInfo {
    pub addr: u64,
    pub perms: MmapPerms,
    pub private: bool,
    pub dirty: bool,
    pub data: Option<Box<[u8; SNAPSHOT_PAGE_SIZE]>>,
}

#[derive(Debug)]
// TODO be thread-safe maybe with https://amanieu.github.io/thread_local-rs/thread_local/index.html
pub struct QemuSnapshotHelper {
    pub access_cache: [u64; 4],
    pub access_cache_idx: usize,
    pub pages: HashMap<u64, SnapshotPageInfo>,
    pub dirty: Vec<u64>,
    pub brk: u64,
    //pub new_maps: Vec<(u64, usize, Option<MmapPerms>)>,
    pub new_maps: IntervalTree<u64, Option<MmapPerms>>,
    pub empty: bool,
}

impl QemuSnapshotHelper {
    #[must_use]
    pub fn new() -> Self {
        Self {
            access_cache: [u64::MAX; 4],
            access_cache_idx: 0,
            pages: HashMap::default(),
            dirty: vec![],
            brk: 0,
            new_maps: IntervalTree::new(),
            empty: true,
        }
    }

    pub fn snapshot(&mut self, emulator: &Emulator) {
        self.brk = emulator.get_brk();
        self.pages.clear();
        for map in emulator.mappings() {
            // TODO track all the pages OR track mproctect
            let mut addr = map.start();
            while addr < map.end() {
                let mut info = SnapshotPageInfo {
                    addr,
                    perms: map.flags(),
                    private: map.is_priv(),
                    dirty: false,
                    data: None,
                };
                if map.flags().is_w() {
                    unsafe {
                        info.data = Some(Box::new(core::mem::MaybeUninit::uninit().assume_init()));
                        emulator.read_mem(addr, &mut info.data.as_mut().unwrap()[..]);
                    }
                }
                self.pages.insert(addr, info);
                addr += SNAPSHOT_PAGE_SIZE as u64;
            }
        }
        self.empty = false;
    }

    pub fn page_access(&mut self, page: u64) {
        if self.access_cache[0] == page
            || self.access_cache[1] == page
            || self.access_cache[2] == page
            || self.access_cache[3] == page
        {
            return;
        }
        self.access_cache[self.access_cache_idx] = page;
        self.access_cache_idx = (self.access_cache_idx + 1) & 3;
        if let Some(info) = self.pages.get_mut(&page) {
            if info.dirty {
                return;
            }
            info.dirty = true;
        }
        self.dirty.push(page);
    }

    pub fn access(&mut self, addr: u64, size: usize) {
        debug_assert!(size > 0);
        let page = addr & (SNAPSHOT_PAGE_SIZE as u64 - 1);
        self.page_access(page);
        let second_page = (addr + size as u64 - 1) & (SNAPSHOT_PAGE_SIZE as u64 - 1);
        if page != second_page {
            self.page_access(second_page);
        }
    }

    pub fn reset(&mut self, emulator: &Emulator) {
        self.reset_maps(emulator);
        self.access_cache = [u64::MAX; 4];
        self.access_cache_idx = 0;
        while let Some(page) = self.dirty.pop() {
            if let Some(info) = self.pages.get_mut(&page) {
                if let Some(data) = info.data.as_ref() {
                    unsafe { emulator.write_mem(page, &data[..]) };
                }
                info.dirty = false;
            }
        }
        emulator.set_brk(self.brk);
    }

    pub fn add_mapped(&mut self, start: u64, size: usize, perms: Option<MmapPerms>) {
        self.new_maps.insert(start..start + (size as u64), perms);
    }

    pub fn reset_maps(&mut self, emulator: &Emulator) {
        for r in self.new_maps.find(0..u64::MAX) {
            let addr = r.interval().start;
            let end = r.interval().end;
            let perms = r.data();
            let mut page = addr & (SNAPSHOT_PAGE_SIZE as u64 - 1);
            let mut to_unmap = vec![];
            let mut prev = false;
            while page < end {
                if let Some(info) = self.pages.get(&page) {
                    prev = false;
                    if let Some(p) = perms {
                        if info.perms != *p {
                            emulator.change_prot(page, SNAPSHOT_PAGE_SIZE, info.perms);
                        }
                    }
                } else {
                    if !prev {
                        to_unmap.push((page, SNAPSHOT_PAGE_SIZE));
                    } else if let Some(last) = to_unmap.last_mut() {
                        last.1 += SNAPSHOT_PAGE_SIZE;
                    }
                    prev = true;
                }
                page += SNAPSHOT_PAGE_SIZE as u64;
            }
            for (addr, size) in to_unmap {
                //drop(emulator.unmap(addr, size));
            }
            //drop(emulator.unmap(*addr, *size));
        }
        //self.new_maps.clear();
        self.new_maps = IntervalTree::new();
    }
}

impl Default for QemuSnapshotHelper {
    fn default() -> Self {
        Self::new()
    }
}

impl<I, S> QemuHelper<I, S> for QemuSnapshotHelper
where
    I: Input,
    S: HasMetadata,
{
    fn init<'a, H, OT, QT>(&self, executor: &QemuExecutor<'a, H, I, OT, QT, S>)
    where
        H: FnMut(&I) -> ExitKind,
        OT: ObserversTuple<I, S>,
        QT: QemuHelperTuple<I, S>,
    {
        executor.hook_write8_execution(trace_write8_snapshot::<I, QT, S>);
        executor.hook_write4_execution(trace_write4_snapshot::<I, QT, S>);
        executor.hook_write2_execution(trace_write2_snapshot::<I, QT, S>);
        executor.hook_write1_execution(trace_write1_snapshot::<I, QT, S>);
        executor.hook_write_n_execution(trace_write_n_snapshot::<I, QT, S>);

        executor.hook_after_syscalls(trace_mmap_snapshot::<I, QT, S>);
    }

    fn pre_exec(&mut self, emulator: &Emulator, _input: &I) {
        if self.empty {
            self.snapshot(emulator);
        } else {
            self.reset(emulator);
        }
    }
}

pub fn trace_write1_snapshot<I, QT, S>(
    _emulator: &Emulator,
    helpers: &mut QT,
    _state: &mut S,
    _id: u64,
    addr: u64,
) where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers
        .match_first_type_mut::<QemuSnapshotHelper>()
        .unwrap();
    h.access(addr, 1);
}

pub fn trace_write2_snapshot<I, QT, S>(
    _emulator: &Emulator,
    helpers: &mut QT,
    _state: &mut S,
    _id: u64,
    addr: u64,
) where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers
        .match_first_type_mut::<QemuSnapshotHelper>()
        .unwrap();
    h.access(addr, 2);
}

pub fn trace_write4_snapshot<I, QT, S>(
    _emulator: &Emulator,
    helpers: &mut QT,
    _state: &mut S,
    _id: u64,
    addr: u64,
) where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers
        .match_first_type_mut::<QemuSnapshotHelper>()
        .unwrap();
    h.access(addr, 4);
}

pub fn trace_write8_snapshot<I, QT, S>(
    _emulator: &Emulator,
    helpers: &mut QT,
    _state: &mut S,
    _id: u64,
    addr: u64,
) where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers
        .match_first_type_mut::<QemuSnapshotHelper>()
        .unwrap();
    h.access(addr, 8);
}

pub fn trace_write_n_snapshot<I, QT, S>(
    _emulator: &Emulator,
    helpers: &mut QT,
    _state: &mut S,
    _id: u64,
    addr: u64,
    size: usize,
) where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers
        .match_first_type_mut::<QemuSnapshotHelper>()
        .unwrap();
    h.access(addr, size);
}

#[allow(clippy::too_many_arguments)]
pub fn trace_mmap_snapshot<I, QT, S>(
    _emulator: &Emulator,
    helpers: &mut QT,
    _state: &mut S,
    result: u64,
    sys_num: i32,
    a0: u64,
    a1: u64,
    a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
) -> u64
where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    if result == u64::MAX
    /* -1 */
    {
        return result;
    }
    if i64::from(sys_num) == SYS_mmap {
        if let Ok(prot) = MmapPerms::try_from(a2 as i32) {
            let h = helpers
                .match_first_type_mut::<QemuSnapshotHelper>()
                .unwrap();
            h.add_mapped(result, a1 as usize, Some(prot));
        }
    } else if i64::from(sys_num) == SYS_mremap {
        let h = helpers
            .match_first_type_mut::<QemuSnapshotHelper>()
            .unwrap();
        h.add_mapped(a0, a2 as usize, None);
    } else if i64::from(sys_num) == SYS_mprotect {
        if let Ok(prot) = MmapPerms::try_from(a2 as i32) {
            let h = helpers
                .match_first_type_mut::<QemuSnapshotHelper>()
                .unwrap();
            h.add_mapped(a0, a2 as usize, Some(prot));
        }
    }
    result
}
