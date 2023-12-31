use axlog::debug;
#[cfg(not(test))]
use axlog::{info, warn}; use core::fmt::DebugList;
// Use log crate when building application
 
#[cfg(test)]
use std::{println as info, println as warn}; // Workaround to use prinltn! for logs.

use crate::{SleepLock, init_lock, SleepLockGuard, disk_inode};
use crate::fs_const::{BSIZE, DIRSIZ, IPB, NDIRECT, NINDIRECT, NINODE, ROOTDEV, ROOTINUM, NININDIRECT};
use crate::log::LOG_MANAGER;
use crate::bitmap::{inode_alloc, bisalloc};
use crate::misc::{min, mem_set};
use crate::interface::INTERFACE_MANAGER;

use spin::{Mutex,MutexGuard};

use core::mem::size_of;
use core::ptr::{self, read, write};
use core::{str, usize};

use array_macro::array;

use crate::buffer_cache::{BLOCK_CACHE_MANAGER, BufData};
use crate::superblock::{SUPER_BLOCK, RawSuperBlock};
use super::stat::Stat;
use crate::disk_inode::{ InodeType, DiskInode, DirEntry };
use super::bitmap::{balloc, bfree};
use alloc::{vec::Vec,string::String};
use lazy_init::LazyInit;

pub static ICACHE: LazyInit<InodeCache> = LazyInit::new();

type BlockNo = u32;

 
pub struct InodeCache {
    meta: Mutex<[InodeMeta; NINODE]>,
    data: [SleepLock<InodeData>; NINODE]
}

impl InodeCache {
    pub fn new() -> Self {
        Self {
            meta: Mutex::new(array![_ => InodeMeta::new(); NINODE]),
            data: array![_ => SleepLock::new(InodeData::new(),init_lock()); NINODE],
        }
    }


    /// Clone an inode by just increment its reference count by 1. 
    fn dup(&self, inode: &Inode) -> Inode {
        let mut guard = self.meta.lock();
        guard[inode.index].refs += 1;
        Inode {
            dev: inode.dev,
            inum: inode.inum,
            index: inode.index
        }
    }

    /// Done with this inode. 
    /// If this is the last reference in the inode cache, then is might be recycled. 
    /// Further, if this inode has no links anymore, free this inode in the disk. 
    /// It should only be called by the Drop impl of Inode. 
    fn put(&self, inode: &mut Inode) {
        let mut guard = self.meta.lock();
        let i = inode.index;
        let imeta = &mut guard[i];

        if imeta.refs == 1 {
            // SAFETY: reference count is 1, so this lock will not block. 
            let mut idata = self.data[i].lock();
            if !idata.valid || idata.dinode.nlink > 0 {
                idata.valid = false;
                drop(idata);
                imeta.refs -= 1;
                drop(guard);
            } else {
                drop(guard);
                idata.dinode.itype = InodeType::Empty;
                idata.truncate(inode);
                idata.valid = false;
                drop(idata);

                // recycle after this inode content in the cache is no longer valid. 
                // note: it is wrong to recycle it earlier, 
                // otherwise the cache content might change
                // before the previous content written to disk. 
                let mut guard = self.meta.lock();
                guard[i].refs -= 1;
                debug_assert_eq!(guard[i].refs, 0);
                drop(guard);
            }
        } else {
            imeta.refs -= 1;
            drop(guard);
        }
    }

    /// Allocate an inode on device dev. 
    /// Mark it as allocated by giving it type type. 
    /// Returns an unlocked but allocated and reference inode 
    pub fn alloc(&self, dev: u32, itype: InodeType) -> Option<Inode> {
        let ninodes = unsafe {
            SUPER_BLOCK.ninodes()
        };
        for inum in 1 ..= ninodes {
            // get block id
            let block_id = unsafe {
                SUPER_BLOCK.locate_inode(inum)
            };
            // read block into buffer by device and block_id
            debug!("alloc");
            let mut block = BLOCK_CACHE_MANAGER.bread(dev, block_id);
        
            // Get inode offset in the block
            let offset = locate_inode_offset(inum) as isize;
            let dinode = unsafe { (block.raw_data_mut() as *mut DiskInode).offset(offset) };
            let dinode = unsafe{ &mut *dinode };
            // Find a empty inode
            if dinode.try_alloc(itype).is_ok() {
                LOG_MANAGER.write(block);
                return Some(self.get(dev, inum))
            }
            // drop(block);
        }
        None
    }

    /// Lookup the inode in the inode cache. 
    /// If found, return an handle. 
    /// If not found, alloc an in-memory location in the cache, 
    /// but not fetch it from the disk yet. 
    pub fn get(&self, dev: u32, inum: u32) -> Inode {
        let mut guard = self.meta.lock();

        // lookup in the cache 
        let mut empty_i: Option<usize> = None;
        for i in 0..NINODE {
            if guard[i].inum == inum && guard[i].refs > 0 && guard[i].dev == dev {
                guard[i].refs += 1;
                // info!("[Debug] 获取Inode");
                return Inode {
                    dev,
                    inum,
                    index: i,
                }
            }
            if empty_i.is_none() && guard[i].refs == 0 {
                empty_i = Some(i);
            }
        }

        // not found 
        let empty_i = match empty_i {
            Some(i) => i,
            None => panic!("inode: not enough"),
        };
        guard[empty_i].dev = dev;
        guard[empty_i].inum = inum;
        guard[empty_i].refs = 1;
        // 此时 Inode Cache 应当是无效的
        let idata = self.data[empty_i].lock();
        assert!(idata.valid == false, "此时 idata 应当无效");
        Inode {
            dev,
            inum,
            index: empty_i
        }
    }

    pub fn get_inum_type(&self,dev: u32,inum: u32)->InodeType{
        let inode=self.get(dev, inum);
        let inode_data=inode.lock();
        let dinode=inode_data.dinode;
        let itype=dinode.itype;
        drop(inode_data);
        itype 
    }

    /// Helper function for 'namei' and 'namei_parent'
    fn namex(
        &self, 
        path: &[u8], 
        name: &mut [u8;DIRSIZ], 
        is_parent: bool
    ) -> Option<Inode> {
        let mut inode: Inode;
        if path[0] == b'/' {
            inode = self.get(ROOTDEV, ROOTINUM);
            //info!("path 0 is /");
        } else {
            //这里是要获取当前目录的名称
            inode=self.dup(INTERFACE_MANAGER.interface.as_ref().get_cur_dir_inode().as_ref().unwrap());
        }
        let mut cur: usize = 0;
        loop {
            cur = skip_path(path, cur, name);//这里name获取了/后面的第一个路径名
            if cur == 0 { break; }
            //info!("cur is {:?}, and name is {:?}",cur,String::from_utf8(name.to_vec()).unwrap());
            let mut data_guard = inode.lock();
            //info!("acquire lock");
            if data_guard.dinode.itype != InodeType::Directory {
                drop(data_guard);
                return None
            }
            if is_parent && path[cur] == 0 {
                //info!("is is parent and path[cur]=0");
                drop(data_guard);
                return Some(inode)
            }

            match data_guard.dir_lookup(name) {
                None => {
                    drop(data_guard);
                    // info!("[Kernel] name: {}", String::from_utf8(name.to_vec()).unwrap());
                    return None
                },
                Some(last_inode) => {
                    drop(data_guard);
                    inode = last_inode;
                }
            }
            mem_set(name.as_mut_ptr(), 0, DIRSIZ);
        }
        if is_parent {
            // only when querying root inode's parent 
            //info!("[Kernel] Warning: namex querying root inode's parent");
            None 
        } else {
            Some(inode)
        }
    }

    /// namei interprets the path argument as an pathname to Unix file. 
    /// It will return an [`inode`] if succeed, Err(()) if fail. 
    /// It must be called inside a transaction(i.e.,'begin_op' and `end_op`) since it calls `put`.
    /// Note: the path should end with 0u8, otherwise it might panic due to out-of-bound. 
    pub fn namei(&self, path: &[u8]) -> Option<Inode> {
        let mut name: [u8;DIRSIZ] = [0;DIRSIZ];
        self.namex(path, &mut name, false)
    }

    /// Same behavior as `namei`, but return the parent of the inode, 
    /// and copy the end path into name. 
    pub fn namei_parent(&self, path: &[u8], name: &mut [u8;DIRSIZ]) -> Option<Inode> {
        self.namex(path, name, true)
    }

    pub fn look_up(&self,path: &[u8])->Result<Inode, &'static str>{
        info!("[Xv6fs] lookup file/dir: path: {}", String::from_utf8(path.to_vec()).unwrap());
        let mut name: [u8; DIRSIZ] = [0; DIRSIZ];
        let dirinode = self.namei_parent(path, &mut name).unwrap();
        let mut dirinode_guard = dirinode.lock();
        match dirinode_guard.dir_lookup(&name) {
            Some(node) => Ok(node), 
            None => Err("not found"),
        }
    }

    pub fn create(
        &self,
        path: &[u8],
        itype: InodeType,
        major: i16,
        minor: i16
    ) -> Result<Inode, &'static str> {
        info!("[Xv6fs] create file/dir: path: {}", String::from_utf8(path.to_vec()).unwrap());
        let mut name: [u8; DIRSIZ] = [0; DIRSIZ];
        let dirinode = self.namei_parent(path, &mut name).unwrap();
        let mut dirinode_guard = dirinode.lock();
        match dirinode_guard.dir_lookup(&name) {
            Some(inode) => {
                drop(dirinode_guard);
                let inode_guard = inode.lock();
                match inode_guard.dinode.itype {
                    InodeType::Directory| InodeType::Device | InodeType::File => {
                        if itype == InodeType::File || itype == InodeType::Directory {
                            drop(inode_guard);
                            return Ok(inode)
                        }
                        return Err("create: unmatched type.");
                    },
    
                    _ => {
                        return Err("create: unmatched type.")
                    }
                }
            },
    
            None => {}
        }
        // Allocate a new inode to create file
        let dev = dirinode_guard.dev;
        let inum = inode_alloc(dev, itype);
        let inode = self.get(dev, inum);
        
        let mut inode_guard = inode.lock();
        // initialize new allocated inode
        inode_guard.dinode.major = major;
        inode_guard.dinode.minor = minor;
        inode_guard.dinode.nlink = 1;
        // Write back to disk
        inode_guard.update();
        debug_assert_eq!(inode_guard.dinode.itype, itype);
    
        // Directory, create .. 
        if itype == InodeType::Directory {
            // Create . and .. entries. 
            inode_guard.dinode.nlink += 1;
            inode_guard.update();
            // No nlink++ for . to avoid recycle ref count. 
            inode_guard.dir_link(".".as_bytes(), inode.inum)?;
            inode_guard.dir_link("..".as_bytes(), dirinode_guard.inum)?;
        }
        dirinode_guard
            .dir_link(&name, inode_guard.inum)
            .expect("Parent inode fail to link");

        drop(inode_guard);
        drop(dirinode_guard);
        Ok(inode)
    }

    pub fn get_root_dir(&self)->Inode{
        self.get(ROOTDEV, ROOTINUM)
    }

    pub fn remove(&self,path: &[u8])->Result<(),&'static str>{
        //info!("begin remove");
        info!("[Xv6fs] remove file/dir, path is {:?}",core::str::from_utf8(path));
        let mut name: [u8; DIRSIZ] = [0; DIRSIZ];
        let dirinode = self.namei_parent(path, &mut name).unwrap();
        //info!("name is {:?} as {:?}",&name,String::from_utf8(name.to_vec()));
        let mut dirinode_guard = dirinode.lock();
        //info!("get locked dirinode!");
        match dirinode_guard.dir_lookup(&name) {
            Some(inode) => {
                let mut idata = inode.lock();
                //info!("get locked inode!");
                match idata.dinode.itype {
                    InodeType::Directory=> {
                        idata.clear_dir()?;
                        idata.dinode.itype=InodeType::Empty;
                        idata.truncate(&inode);
                        idata.valid=false;
                        drop(idata);
                        dirinode_guard.dir_unlink(&name)?;
                        dirinode_guard.update();
                        return Ok(());
                    },

                    InodeType::File=>{
                        idata.dinode.itype=InodeType::Empty;
                        idata.truncate(&inode);
                        idata.valid=false;
                        drop(idata);
                        dirinode_guard.dir_unlink(&name)?;
                        dirinode_guard.update();
                        return Ok(());
                    },
    
                    _ => {
                        return Err("remove: unmatched type.")
                    }
                }
            },
    
            None => {
                return  Err("remove: error path.");
            }
        }
    }
}

/// Skip the path starting at cur by b'/'s. 
/// It will copy the skipped content to name. 
/// Return the current offset after skiping. 
fn skip_path(
    path: &[u8], 
    mut cur: usize, 
    name: &mut [u8; DIRSIZ]
) -> usize {
    // skip preceding b'/'
    while path[cur] == b'/' {
        cur += 1;
    }
    if path[cur] == 0 {
        return 0
    }

    let start = cur;
    while path[cur] != b'/' && path[cur] != 0 {
        cur += 1;
    }

    let mut count = cur - start; 
    if count >= name.len() {
        debug_assert!(false);
        count = name.len() - 1;
    }
    unsafe{
        ptr::copy(path.as_ptr().offset(start as isize), name.as_mut_ptr(), count);
    }
    name[count] = 0;

    // skip succeeding b'/'
    while path[cur] == b'/' {
        cur += 1;
    }
    cur
}


struct InodeMeta {
    /// device number
    dev: u32,
    /// block number, calculated from inum
    blockno: u32,
    /// inode number
    inum: u32,
    /// reference count
    refs: usize
}

impl InodeMeta {
    const fn new() -> Self {
        Self {
            dev: 0,
            blockno: 0,
            inum: 0,
            refs: 0
        }
    }
}

/// In-memory copy of an inode
pub struct InodeData {
    pub valid: bool,
    pub dev: u32,
    pub inum: u32,
    pub dinode: DiskInode
}

impl InodeData {
    const fn new() -> Self {
        Self {
            valid: false,
            dev: 0,
            inum: 0,
            dinode: DiskInode::new()
        }
    }


    /// Copy stat information from inode
    pub fn stat(&self, stat: &mut Stat) {
        stat.dev = self.dev;
        stat.inum = self.inum;
        stat.itype = self.dinode.itype;
        stat.nlink = self.dinode.nlink;
        stat.size = self.dinode.size as usize;
    }

    pub fn clear_block(dev:u32,block_id:u32){
        //debug!("clear block blockid is {}",block_id);
        let mut buf=BLOCK_CACHE_MANAGER.bread(dev, block_id);
        let buf_ptr=unsafe{(buf.raw_data_mut() as *mut u8).offset(0)};
        let empty_block:[u8;BSIZE]=[0;BSIZE];
        unsafe{ptr::copy(&empty_block as *const u8, buf_ptr, BSIZE)};
        LOG_MANAGER.write(buf);
    }

    /// Discard the inode data/content. 
    pub fn truncate(&mut self, inode: &Inode) {
        // direct block
        for i in 0..NDIRECT {
            if self.dinode.addrs[i] > 0 {
                let _=bfree(self.dinode.addrs[i]);
                self.dinode.addrs[i] = 0;
            }
        }

        // indirect block
        if self.dinode.addrs[NDIRECT] > 0 {
            //debug!("truncate bread indirect block ");
            let buf = BLOCK_CACHE_MANAGER.bread(inode.dev, self.dinode.addrs[NDIRECT]);
            let buf_ptr = buf.raw_data() as *const BlockNo;
            for i in 0..NINDIRECT {
                let bn = unsafe{ read(buf_ptr.offset(i as isize)) };
                if bn > 0 {
                    let _=bfree(bn);
                }
            }
            drop(buf);
            let _=bfree(self.dinode.addrs[NDIRECT]);//这里要清空这个页面才行
            self.dinode.addrs[NDIRECT] = 0;
        }

        if self.dinode.addrs[NDIRECT+1] > 0 {
            //debug!("truncate bread inindirect block");
            let buf = BLOCK_CACHE_MANAGER.bread(inode.dev, self.dinode.addrs[NDIRECT+1]);
            let buf_ptr=buf.raw_data() as *const BlockNo;
            for i in 0..NINDIRECT{
                let ibn=unsafe { read(buf_ptr.offset(i as isize))};
                info!("[Xv6fs] inode truncate: indirect block no is {}",ibn);
                if ibn > 0{
                    //debug!("ibn is {}",ibn);
                    let ibuf=BLOCK_CACHE_MANAGER.bread(inode.dev, ibn);
                    let ibuf_ptr=ibuf.raw_data() as *const BlockNo;
                    for j in 0..NINDIRECT{
                        let bn = unsafe{ read(ibuf_ptr.offset(j as isize)) };
                        info!("[Xv6fs] inode truncate: direct block no is {}",bn);
                        if bn > 0 {
                            let _=bfree(bn);
                        }
                    }
                    drop(ibuf);
                    let _=bfree(ibn);
                }
            }
            drop(buf);
            let _=bfree(self.dinode.addrs[NDIRECT+1]);
            self.dinode.addrs[NDIRECT+1]=0;
        }

        self.dinode.size = 0;
        self.update();
    }

    pub fn resize(&mut self,inode: &Inode,size:u64)->usize{//todo! need verify！！！
        let nblocks:usize=match size%BSIZE as u64{
            0=>size as usize/BSIZE,
            _=>size as usize/BSIZE+1,
        };
        let begin:usize=match self.dinode.size as usize%BSIZE{
            0=>self.dinode.size as usize/BSIZE,
            _=>self.dinode.size as usize/BSIZE+1,
        };
        if self.dinode.size == size as u32{
            return size as usize;
        }else if self.dinode.size > size as u32{
            for i in nblocks..NDIRECT {
                if self.dinode.addrs[i] > 0 {
                    let _=bfree(self.dinode.addrs[i]);
                    self.dinode.addrs[i] = 0;
                }
            }
    
            // indirect block
            let mut _count=NDIRECT;
            let indirect_index=(nblocks-NDIRECT).max(0);
            if self.dinode.addrs[NDIRECT] > 0 {
                //debug!("truncate bread indirect block ");
                let buf = BLOCK_CACHE_MANAGER.bread(inode.dev, self.dinode.addrs[NDIRECT]);
                let buf_ptr = buf.raw_data() as *const BlockNo;
                for i in indirect_index..NINDIRECT {
                    let bn = unsafe{ read(buf_ptr.offset(i as isize)) };
                    if bn > 0 {
                        let _=bfree(bn);
                    }
                    _count += 1;
                }
                drop(buf);
                if nblocks <= NDIRECT{
                    let _=bfree(self.dinode.addrs[NDIRECT]);//这里要清空这个页面才行
                    self.dinode.addrs[NDIRECT] = 0;
                }
            }
            let left_blocks=(nblocks-NDIRECT-NINDIRECT).max(0);
            let inindirect_index=left_blocks/NINDIRECT;
            let indirect_index=left_blocks%NINDIRECT;
            if self.dinode.addrs[NDIRECT+1] > 0 {//这个还没弄呢
                //debug!("truncate bread inindirect block");
                let buf = BLOCK_CACHE_MANAGER.bread(inode.dev, self.dinode.addrs[NDIRECT+1]);
                let buf_ptr=buf.raw_data() as *const BlockNo;
                let ibn=unsafe { read(buf_ptr.offset(inindirect_index as isize))};
                if ibn > 0{
                    //debug!("ibn is {}",ibn);
                    let ibuf=BLOCK_CACHE_MANAGER.bread(inode.dev, ibn);
                    let ibuf_ptr=ibuf.raw_data() as *const BlockNo;
                    for j in indirect_index..NINDIRECT{
                        let bn = unsafe{ read(ibuf_ptr.offset(j as isize)) };
                        info!("[Xv6fs] inode truncate: direct block no is {}",bn);
                        if bn > 0 {
                            let _=bfree(bn);
                        }
                    }
                    drop(ibuf);
                    if indirect_index==0{
                        let _=bfree(ibn);
                    }
                }
                for i in inindirect_index+1..NINDIRECT{
                    let ibn=unsafe { read(buf_ptr.offset(i as isize))};
                    info!("[Xv6fs] inode truncate: indirect block no is {}",ibn);
                    if ibn > 0{
                        //debug!("ibn is {}",ibn);
                        let ibuf=BLOCK_CACHE_MANAGER.bread(inode.dev, ibn);
                        let ibuf_ptr=ibuf.raw_data() as *const BlockNo;
                        for j in 0..NINDIRECT{
                            let bn = unsafe{ read(ibuf_ptr.offset(j as isize)) };
                            info!("[Xv6fs] inode truncate: direct block no is {}",bn);
                            if bn > 0 {
                                let _=bfree(bn);
                            }
                        }
                        drop(ibuf);
                        let _=bfree(ibn);
                    }
                }
                drop(buf);
                if nblocks <= NDIRECT+NINDIRECT{
                    let _=bfree(self.dinode.addrs[NDIRECT+1]);
                    self.dinode.addrs[NDIRECT+1]=0;
                }
            }
        }else{
            for i in begin..=nblocks{
                let _=self.bmap(i as u32,true);
            }
        }
        self.update();
        0
    }

    /// Update a modified in-memory inode to disk. 
    /// Typically called after changing the content of inode info. 
    pub fn update(&mut self) {
        //info!("update: begin update");
        let mut buf = BLOCK_CACHE_MANAGER.bread(
            self.dev, 
            unsafe { SUPER_BLOCK.locate_inode(self.inum)}
        );
        let offset = locate_inode_offset(self.inum) as isize;
        let dinode = unsafe{ (buf.raw_data_mut() as *mut DiskInode).offset(offset) };
        unsafe{ write(dinode, self.dinode) };
        //info!("update: self.dindoe: {:?}", self.dinode);
        LOG_MANAGER.write(buf);
    }

    /// The content (data) associated with each inode is stored
    /// in blocks on the disk. The first NDIRECT block numbers
    /// are listed in self.dinode.addrs, The next NINDIRECT blocks are 
    /// listed in block self.dinode.addrs[NDIRECT]. 
    /// 
    /// Return the disk block address of the nth block in inode. 
    /// If there is no such block, bmap allocates one. 
    pub fn bmap(&mut self, offset_bn: u32, balloc_flag: bool) -> Result<u32, &'static str> {
        let mut addr;
        let mut iaddr:u32;
        let offset_bn = offset_bn as usize;
        if offset_bn < NDIRECT {
            if self.dinode.addrs[offset_bn] == 0 {
                addr = balloc(self.dev);
                self.dinode.addrs[offset_bn] = addr;
                return Ok(addr)
            } else {
                return Ok(self.dinode.addrs[offset_bn])
            }
        }
        if offset_bn < NINDIRECT + NDIRECT {
            // Load indirect block, allocating if necessary. 
            let count = offset_bn - NDIRECT;
            if self.dinode.addrs[NDIRECT] == 0 {
                iaddr = balloc(self.dev);
                self.dinode.addrs[NDIRECT] = iaddr;
                Self::clear_block(self.dev, iaddr);
            } else {
                iaddr = self.dinode.addrs[NDIRECT]
            }
            //debug!("bread iaddr {}",iaddr);
            let mut buf = BLOCK_CACHE_MANAGER.bread(self.dev, iaddr);
            let mut buf_data = buf.raw_data() as *mut u32;
            addr = unsafe{ read(buf_data.offset(count as isize)) };
            debug!("[Xv6fs] bmap: addr is {}",addr);
            if addr == 0 || !(bisalloc(addr)) || balloc_flag{
                unsafe{
                    addr = balloc(self.dev);
                    write(buf_data.offset(count as isize), addr);
                }
                LOG_MANAGER.write(buf);//这里是个什么玩意啊，裂开
            }
            // drop(buf);
            return Ok(addr)
        }
        if offset_bn < NINDIRECT+NDIRECT+NININDIRECT{
            let count=offset_bn-NDIRECT-NINDIRECT;
            if self.dinode.addrs[NDIRECT+1]==0{
                addr=balloc(self.dev);
                self.dinode.addrs[NDIRECT+1]=addr;
                Self::clear_block(self.dev, addr);
            }else {
                addr=self.dinode.addrs[NDIRECT+1];
            }
            let indirect_count=count/64;
            let indirect_offset=count%64;
            //debug!("bread addr {}",addr);
            let mut buf=BLOCK_CACHE_MANAGER.bread(self.dev, addr);
            let mut buf_data=buf.raw_data() as * mut u32;
            let mut iaddr = unsafe { read(buf_data.offset(indirect_count as isize))};
            //debug!("[Xv6fs] bmap: iaddr is {}, balloc_flag is {}, bisalloc is {}",iaddr,balloc_flag,bisalloc(iaddr));
            if balloc_flag!=(!bisalloc(iaddr)) && iaddr != 0{
                //panic!("balloc flag is not same with !bisalloc");
            }
            if iaddr == 0 || !(bisalloc(iaddr)) /*|| balloc_flag*/{
                unsafe{
                    iaddr=balloc(self.dev);
                    write(buf_data.offset(indirect_count as isize), iaddr);
                    Self::clear_block(self.dev, iaddr);
                }
                LOG_MANAGER.write(buf);
                drop(buf_data);
            }
            //debug!("bread indirect iaddr {}",iaddr);
            let mut ibuf=BLOCK_CACHE_MANAGER.bread(self.dev, iaddr);
            let mut ibuf_data=ibuf.raw_data() as *mut u32;
            addr=unsafe { read(ibuf_data.offset(indirect_offset as isize))};
            //debug!("[Xv6fs] bmap: addr is {}, balloc_flag is {}, bisalloc is {}",addr,balloc_flag,bisalloc(addr));
            if addr ==0 || !(bisalloc(addr)) /*|| balloc_flag*/{
                unsafe{
                    addr=balloc(self.dev);
                    write(ibuf_data.offset(indirect_offset as isize), addr);
                }
                LOG_MANAGER.write(ibuf);
            }
            return Ok(addr);
        }
        panic!("inode bmap: out of range.");
    }

    /// Read data from inode. 
    /// Caller must hold inode's sleeplock. 
    /// If is_user is true, then dst is a user virtual address;
    /// otherwise, dst is a kernel address. 
    /// is_user 为 true 表示 dst 为用户虚拟地址，否则表示内核虚拟地址
    /// 以上是曾经的注释，目前已经没有用户虚拟地址和内核虚拟地址的区别
    pub fn read(
        &mut self,
        mut dst: usize, 
        offset: u32, 
        count: u32
    ) -> Result<usize, &'static str> { 
        // Check the reading content is in range.
        let end = offset.checked_add(count).ok_or("Fail to add count.")?;
        if end > self.dinode.size {
            info!("[Kernel] read: end: {}, dinode.size: {}", end, self.dinode.size);
            //return Err("inode read: end is more than diskinode's size.")
        }
        //read all content of the file.
        if(offset>=self.dinode.size){
            info!("[Kernel] read: end: {}, dinode.size: {}, offset: {}", end, self.dinode.size,offset);
            return Ok(0);
        }
        let mut total: usize = 0;
        let mut offset = offset as usize;
        let count=count as usize;
        let count = min((count as usize + offset as usize),self.dinode.size as usize) - offset as usize;
        info!("count is {}",count);
        //return Ok(10);
        let mut block_basic = offset / BSIZE;
        let mut block_offset = offset % BSIZE;
        while total < count as usize {
            let surplus_len = count - total;
            let block_no = self.bmap(block_basic as u32, false)?;
            debug!("read block no is {},offset is {}",block_no,offset);
            let buf = BLOCK_CACHE_MANAGER.bread(self.dev, block_no);
            let write_len = min(surplus_len, BSIZE - block_offset);
            // if copy_from_kernel(
            //     is_user, 
            //     dst, 
            //     unsafe{ (buf.raw_data() as *mut u8).offset((offset % BSIZE) as isize) },
            //     write_len as usize
            // ).is_err() {
            //     drop(buf);
            //     return Err("inode read: Fail to either copy out.")
            // }
            let src=unsafe{ (buf.raw_data() as *mut u8).offset((offset % BSIZE) as isize) };
            unsafe{ptr::copy(src as *const u8, dst as *mut u8, write_len);}
            drop(buf);
            total += write_len as usize;
            offset += write_len as usize;
            dst += write_len as usize;
            // 块的初始值及块的偏移量
            block_basic = offset / BSIZE;
            block_offset = offset % BSIZE;
        }
        Ok(total)
    }


    /// Write data to inode. 
    /// Caller must hold inode's sleeplock. 
    /// If is_user is true, then src is a user virtual address; 
    /// otherwise, src is a kernel address. 
    /// Returns the number of bytes successfully written. 
    /// If the return value is less than the requestes n, 
    /// there was an error of some kind. 
    pub fn write(
        &mut self,
        mut src: usize, 
        offset: u32, 
        count: u32
    ) -> Result<usize, &'static str> {
        // let end = offset.checked_add(count).ok_or("Fail to add count.")?;
        // if end > self.dinode.size {
        //     info!("[Kernel] write: end: {}, dinode.size: {}", end, self.dinode.size);
        //     return Err("inode write: end is more than diskinode's size.")
        // }
        info!("[Xv6fs] inode write file/dir: begin inode write");
        let mut offset = offset as usize;
        info!("[Xv6fs] inode write file/dir: write block offset is {}",offset);
        let count = count as usize;
        let mut total = 0;
        let mut block_basic = offset / BSIZE;
        let mut block_offset = offset % BSIZE;
        let mut balloc_flag=false;
        while total < count {
            let surplus_len = count - total;
            let write_len = min(surplus_len, BSIZE - block_offset);
            if self.dinode.size < (offset+write_len) as u32{
                balloc_flag=true;
            }else{
                balloc_flag=false;
            }
            let block_no = self.bmap(block_basic as u32,balloc_flag)?;
            info!("[Xv6fs] inode write file/dir: write block no is {}",block_no);
            let mut buf = BLOCK_CACHE_MANAGER.bread(self.dev, block_no);
            let dst=unsafe{ (buf.raw_data_mut() as *mut u8).offset((offset % BSIZE) as isize ) };
            unsafe{ptr::copy(src as *const u8, dst, write_len);}
            offset += write_len;
            src += write_len;
            total += write_len;

            block_basic = offset / BSIZE;
            block_offset = offset % BSIZE;

            LOG_MANAGER.write(buf);
        }

        if self.dinode.size < offset as u32 {
            self.dinode.size = offset as u32;
        }

        self.update();
        
        // info!("[Kernel] Write end");
        Ok(total)
    }

    /// Look for an inode entry in this directory according the name. 
    /// Panics if this is not a directory. 
    pub fn dir_lookup(&mut self, name: &[u8]) -> Option<Inode> {
        // assert!(name.len() == DIRSIZ);
        info!("[Xv6fs] dir lookup: name is {:?}",core::str::from_utf8(name));
        if self.dinode.itype != InodeType::Directory {
            panic!("inode type is not directory");
        }
        let de_size = size_of::<DirEntry>();
        let mut dir_entry = DirEntry::new();
        let dir_entry_ptr = &mut dir_entry as *mut _ as *mut u8;
        for offset in (0..self.dinode.size).step_by(de_size) {
            self.read(
                dir_entry_ptr as usize, 
                offset, 
                de_size as u32
            ).expect("Cannot read entry in this dir");
            if dir_entry.inum == 0 {
                continue;
            }
            info!("dir_entry_name: {}, name: {}, inum: {}", String::from_utf8(dir_entry.name.to_vec()).unwrap(), String::from_utf8(name.to_vec()).unwrap(),dir_entry.inum);
            for i in 0..DIRSIZ {
                if dir_entry.name[i] != name[i] {
                    break;
                }
                if dir_entry.name[i] == 0 {
                    info!("find you!");
                    return Some(ICACHE.get(self.dev, dir_entry.inum as u32))
                }
            }
        }
        None
    }

    /// Write s new directory entry (name, inum) into the directory
    pub fn dir_link(&mut self, name: &[u8], inum: u32) -> Result<(), &'static str>{
        info!("[Xv6fs] dir link: path is {:?}",String::from_utf8(name.to_vec()).unwrap());
        if self.dir_lookup(name).is_some() {
            return Err("It's incorrect to find entry in disk")
        }
        let mut dir_entry = DirEntry::new();
        // look for an empty dir_entry
        let mut entry_offset = 0;
        for offset in (0..self.dinode.size).step_by(size_of::<DirEntry>()) {
            //info!("dir link begin read dir entry");
            self.read(
                (&mut dir_entry) as *mut DirEntry as usize, 
                offset, 
                size_of::<DirEntry>() as u32
            )?;
            //info!("read entry is {:?}",dir_entry);
            if dir_entry.inum == 0 {
                break;
            }
            entry_offset += size_of::<DirEntry>() as u32;
        }
        unsafe {
            ptr::copy(name.as_ptr(), dir_entry.name.as_mut_ptr(), name.len());
        }
        dir_entry.inum = inum as u16;
        self.write(
            (&dir_entry) as *const _ as usize, 
            entry_offset, 
            size_of::<DirEntry>() as u32
        )?;
        
        Ok(())
    }

    /// Is the directory empty execpt for "." and ".." ?
    pub fn is_dir_empty(&mut self) -> bool {
        let mut dir_entry = DirEntry::new();
        // "." and ".." size
        let init_size = 2 * size_of::<DirEntry>() as u32;
        let final_size = self.dinode.size;
        for offset in (init_size..final_size).step_by(size_of::<DirEntry>()) {
            // Check each direntry, foreach step by size of DirEntry. 
            if self.read(
                &mut dir_entry as *mut DirEntry as usize, 
                offset, 
                size_of::<DirEntry>() as u32
            ).is_err() {
                panic!("is_dir_empty(): Fail to read dir content");
            }

            if dir_entry.inum != 0 {
                return true
            }
        }
        false
    }

    pub fn rename(path:&str,new_name:&str){
        let mut flag=false;
        let mut old_name = [0u8; DIRSIZ];
        let parent=match ICACHE.namei_parent(&path.as_bytes(), &mut old_name) {
            Some(cur)=>cur,
            None=>panic!("[Xv6fs] vfile_unlink: not find path")
        };
        let mut parent_guard=parent.lock();
        let de_size = size_of::<DirEntry>();
        let mut dir_entry = DirEntry::new();
        let dir_entry_ptr = &mut dir_entry as *mut _ as *mut u8;
        for offset in (0..parent_guard.dinode.size).step_by(de_size) {
            parent_guard.read(
                dir_entry_ptr as usize, 
                offset, 
                de_size as u32
            ).expect("Cannot read entry in this dir");
            if dir_entry.inum == 0 {
                continue;
            }
            //info!("dir_entry_name: {}, name: {}, inum: {}", String::from_utf8(dir_entry.name.to_vec()).unwrap(), String::from_utf8(name.to_vec()).unwrap(),dir_entry.inum);
            for i in 0..DIRSIZ {
                if dir_entry.name[i] != old_name[i] {
                    break;
                }
                if dir_entry.name[i] == 0 {
                    let name=&new_name.as_bytes();
                    for i in 0..DIRSIZ {
                        dir_entry.name[i]=name[i];
                        if name[i]==0{
                            break;
                        }
                    }
                    parent_guard.write(dir_entry_ptr as usize, offset, de_size as u32);
                    LOG_MANAGER.end_op();
                    return;
                }
            }
        }
        panic!("[Xv6fs] inode rename: not find dir entry");
    }

    pub fn ls(&mut self)->Option<Vec<String>>{
        if self.dinode.itype!=InodeType::Directory{
            None
        }else{
           let mut v=Vec::new();
           let de_size = size_of::<DirEntry>();
           let mut dir_entry = DirEntry::new();
           let dir_entry_ptr = &mut dir_entry as *mut _ as *mut u8;
           for offset in (0..self.dinode.size).step_by(de_size) {
               self.read(
                   dir_entry_ptr as usize, 
                   offset, 
                   de_size as u32
               ).expect("Cannot read entry in this dir");
               if dir_entry.inum == 0 {
                   continue;
               }
               // info!("dir_entry_name: {}, name: {}", String::from_utf8(dir_entry.name.to_vec()).unwrap(), String::from_utf8(name.to_vec()).unwrap());
               let name=String::from_utf8(dir_entry.name.to_vec()).unwrap();
               v.push(name);
           }
           Some(v)
        }
    }

    pub fn dir_unlink(&mut self, name: &[u8]) -> Result<(),&'static str> {
        // assert!(name.len() == DIRSIZ);
        info!("[Xv6fs] dir unlink: path is {}",String::from_utf8(name.to_vec()).unwrap());
        if self.dinode.itype != InodeType::Directory {
            panic!("inode type is not directory");
        }
        let de_size = size_of::<DirEntry>();
        let mut dir_entry = DirEntry::new();
        let dir_entry_ptr = &mut dir_entry as *mut _ as *mut u8;
        for offset in (0..self.dinode.size).step_by(de_size) {
            self.read(
                dir_entry_ptr as usize, 
                offset, 
                de_size as u32
            ).expect("Cannot read entry in this dir");
            if dir_entry.inum == 0 {
                continue;
            }
            //info!("dir_entry_name: {}, name: {}", String::from_utf8(dir_entry.name.to_vec()).unwrap(), String::from_utf8(name.to_vec()).unwrap());
            for i in 0..DIRSIZ {
                if dir_entry.name[i] != name[i] {
                    break;
                }
                if dir_entry.name[i] == 0 {
                    //info!("find you!!!");
                    dir_entry.inum=0;
                    dir_entry.name=[0;DIRSIZ];
                    self.write(dir_entry_ptr as usize, offset, de_size as u32);
                    return Ok(());
                }
            }
        }
        Err("not find this file in the directory")
    }

    pub fn clear_dir(&mut self) -> Result<(),&'static str> {
        // assert!(name.len() == DIRSIZ);
        if self.dinode.itype != InodeType::Directory {
            panic!("inode type is not directory");
        }
        let de_size = size_of::<DirEntry>();
        let mut dir_entry = DirEntry::new();
        let dir_entry_ptr = &mut dir_entry as *mut _ as *mut u8;
        for offset in (0..self.dinode.size).step_by(de_size) {
            self.read(
                dir_entry_ptr as usize, 
                offset, 
                de_size as u32
            ).expect("Cannot read entry in this dir");
            if dir_entry.inum == 0 || offset/(de_size as u32) < 2{
                continue;
            }
            // info!("dir_entry_name: {}, name: {}", String::from_utf8(dir_entry.name.to_vec()).unwrap(), String::from_utf8(name.to_vec()).unwrap());
            let mut child_inode=ICACHE.get(self.dev, dir_entry.inum as u32);
            let mut cdata=child_inode.lock();
            match cdata.dinode.itype {
                InodeType::File=>{
                    cdata.dinode.itype=InodeType::Empty;
                    cdata.truncate(&child_inode);
                    cdata.valid=false;
                    drop(cdata);
                    self.dir_unlink(&dir_entry.name);
                },
                InodeType::Directory=>{
                    cdata.clear_dir();
                    cdata.dinode.itype=InodeType::Empty;
                    cdata.truncate(&child_inode);
                    cdata.valid=false;
                    drop(cdata);
                    self.dir_unlink(&dir_entry.name);
                },

                _=>{
                    panic!("this is not shoud be in the directory!");
                }
            }
        }
        self.update();
        Ok(())
    }
}

/// Inode handed out by inode cache. 
/// It is actually a handle pointing to the cache. 
#[derive(Debug)]
pub struct Inode {
    pub dev: u32,
    pub inum: u32,
    pub index: usize
}

impl Clone for Inode {
    fn clone(&self) -> Self {
        ICACHE.dup(self)
    }
}

impl Inode {
    /// Lock the inode. 
    /// Load it from the disk if its content not cached yet. 
    pub fn lock<'a>(&'a self) -> SleepLockGuard<'a, InodeData> {
        assert!(self.index < NINODE, "index must less than NINODE");
        //info!("[Kernel] inode.lock(): inode index: {}, dev: {}, inum: {}", self.index, self.dev, self.inum);
        let mut guard = ICACHE.data[self.index].lock();
        
        if !guard.valid {
            let blockno = unsafe{ SUPER_BLOCK.locate_inode(self.inum) };
            //info!("lock blockno is {}",blockno);
            let buf = BLOCK_CACHE_MANAGER.bread(self.dev, blockno);
            let offset = locate_inode_offset(self.inum) as isize;
            //info!("offset is {:?}",offset);
            //let data=buf.raw_data() as *const RawSuperBlock;
            //let data=buf.raw_data() as *const DiskInode;
            // for i in 0..16{
            //     let d=unsafe {
            //         data.offset(i)
            //     };
            //     let dd=unsafe {
            //         core::ptr::read(d)
            //     };
            //     info!("{} disk inode is {:?}",i,dd);
            // }
            //info!("data is {:?}",unsafe{core::ptr::read(data)});
            //let dinode = unsafe{ (buf.raw_data() as *const RawSuperBlock).offset(offset) };
            let dinode = unsafe{ (buf.raw_data() as *const DiskInode).offset(offset) };
            guard.dinode = unsafe{ core::ptr::read(dinode) };
            //info!("{:?}",guard.dinode);
            // info!("dinode is {:?}",unsafe {
            //     core::ptr::read(dinode)
            // });
            drop(buf);
            guard.valid = true;
            guard.dev = self.dev;
            guard.inum = self.inum;
            if guard.dinode.itype == InodeType::Empty {
                panic!("inode lock: trying to lock an inode whose type is empty.")
            }
        }
        guard
    }
}

impl Drop for Inode {
    /// Done with this inode. 
    /// If this is the last reference in the inode cache, then is might be recycled. 
    /// Further, if this inode has no links anymore, free this inode in the disk. 
    fn drop(&mut self) {
        ICACHE.put(self)
    }
}


/// Given an inode number. 
/// Calculate the offset index of this inode inside the block. 
#[inline]
fn locate_inode_offset(inum: u32) -> usize {
    inum as usize % IPB
}
