
#![cfg_attr(not(test), no_std)]

#[cfg(not(test))]
use axlog::{info, warn}; // Use log crate when building application
 
#[cfg(test)]
use std::{println as info, println as warn}; // Workaround to use prinltn! for logs.

extern crate alloc;

pub mod block_dev;
pub mod fs_const;
pub mod buffer_cache;
pub mod log;
pub mod superblock;
pub mod stat;
pub mod disk_inode;
pub mod bitmap;
pub mod inode;
pub mod misc;
pub mod file;
pub mod interface;
pub mod sync;
pub mod xv6fs;

use core::ops::DerefMut;

use alloc::sync::Arc;
pub use block_dev::BlockDevice;
use buffer_cache::BLOCK_CACHE_MANAGER;
use fs_const::{NBUF,BSIZE};
use disk_inode::{InodeType,DiskInode};
use log::{LOG_MANAGER,Log,LogHeader};
use superblock::SUPER_BLOCK;
use xv6fs::Xv6FS;
pub use sync::sleeplock::*;
use core::mem::size_of;


use crate::inode::{ICACHE, InodeCache};

pub unsafe fn init(block_dev:Arc<dyn BlockDevice>,dev:u32) {
    BLOCK_CACHE_MANAGER.set_block_device(Arc::clone(&block_dev));
    BLOCK_CACHE_MANAGER.binit();
    info!("init ICACHE");
    let icache=InodeCache::new();
    ICACHE.init_by(icache);
    info!("init SUPER BLOCK");
    SUPER_BLOCK.init(dev);
    info!("init LOG");
    let log=LOG_MANAGER.log.lock().deref_mut() as *mut Log;
    log.as_mut().unwrap().init(dev);
    info!("block size:{}, disk inode size:{}, log header size:{}",BSIZE,size_of::<DiskInode>(),size_of::<LogHeader>());
    info!("file system: setup done!");
}