use core::ptr::copy_nonoverlapping;
use alloc::sync::Arc;

use crate::BlockDevice;
use crate::disk_inode::{DirEntry,DiskInode, InodeType};
use crate::file::{VFile,FileType};
use crate::inode::{ICACHE,Inode};
use crate::superblock::RawSuperBlock;
use crate::fs_const::{FSMAGIC,BSIZE,IPB,FSSIZE,NDINODES, LOGSIZE};


static mut FREEBLOCK:usize=0;
static mut FREEINODE:usize=1;

/// Disk layout:
/// 
/// boot block | superblock block | log | inode blocks | free bit map | data blocks 
pub struct Xv6FS{
    ninodeblocks:usize,
    nlog:usize,
    nmeta:usize,
    nblocks:usize,
}

pub fn iblock(inum:usize,rsb_inodestart:usize)->usize{
    inum/IPB+rsb_inodestart
}

impl Xv6FS {
    pub fn new()->Self{
        Self {
            ninodeblocks: NDINODES/IPB + 1, 
            nlog: LOGSIZE, 
            // 1 fs block = 1 disk sector
            //nmeta=2 + nlog + ninodeblocks + nbitmap
            nmeta: 2 + LOGSIZE + NDINODES/IPB + 1 + FSSIZE/(BSIZE*8) + 1, 
            //nblocks = FSSIZE - nmeta
            nblocks:  FSSIZE-(2 + LOGSIZE + NDINODES/IPB + 1 + FSSIZE/(BSIZE*8) + 1)
        }
    }

    pub fn create(&self,block_device:Arc<dyn BlockDevice>){
        //set superblock
        let mut raw_superblock=RawSuperBlock::new();
        raw_superblock.magic=FSMAGIC;
        raw_superblock.size=FSSIZE as u32;
        raw_superblock.nblocks=self.nblocks as u32;
        raw_superblock.ninodes=NDINODES as u32;
        raw_superblock.nlog=self.nlog as u32;
        raw_superblock.logstart=2;
        raw_superblock.inodestart=2+self.nlog as u32;
        raw_superblock.bmapstart=(2+self.nlog+self.ninodeblocks) as u32;
        let mut buf=[0 as u8;BSIZE];
        for i in 0..FSSIZE{
            block_device.write_block(i, &buf);
        }
        unsafe{copy_nonoverlapping(&raw_superblock as *const RawSuperBlock, buf.as_mut_ptr() as *mut RawSuperBlock, 1);}
        block_device.write_block(1, &buf);
        //set root inode
        unsafe{FREEBLOCK=self.nmeta;}
        let mut drinode=DiskInode::new();
        let rinum:usize=unsafe{FREEINODE+1 as usize};
        unsafe{FREEINODE+=1;}
        drinode.itype=InodeType::Directory;
        drinode.nlink=1;
        drinode.size=0;
        let block_id=iblock(rinum, raw_superblock.inodestart as usize);
        block_device.read_block(block_id, &mut buf);
        unsafe{
            copy_nonoverlapping(
                &drinode as *const DiskInode, 
                (buf.as_mut_ptr() as usize + (rinum%IPB)*core::mem::size_of::<DiskInode>()) as *mut DiskInode, 
                1
            );
        }
        block_device.write_block(block_id, &buf);
        let mut dir_entry=DirEntry::new();
        unsafe{copy_nonoverlapping(".".as_bytes().as_ptr(), dir_entry.name.as_mut_ptr(), 2);}
        todo!()//有空继续翻译mkfs.c里面的内容



    }

    pub fn get_root_inode(&mut self)->Inode{
        ICACHE.get_root_dir()
    }

    pub fn get_root_vfile(&self)->VFile{
        let inode=ICACHE.get_root_dir();
        let idata=inode.lock();
        let ftype=FileType::Directory;
        drop(idata);
        VFile { 
            ftype,
            readable:true, 
            writeable:true, 
            inode:Some(inode), 
            offset:0,
        }
    }
    

}

