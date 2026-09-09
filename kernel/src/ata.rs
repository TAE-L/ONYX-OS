//! M5: ATA PIO driver for the primary IDE controller.
//!
//! QEMU's `-drive format=raw,file=...` attaches the image as a plain IDE hard
//! disk on the primary bus, so the classic port-I/O registers at 0x1F0..0x1F7
//! (+ control 0x3F6) talk to it. We poll the status register instead of using
//! IRQ14 (the PIC keeps it masked anyway) and set nIEN so the drive cannot
//! fire interrupts behind our back.
//!
//! LBA28 read/write covers QEMU disks up to 128 GiB; LBA48 is a small
//! extension if we ever need bigger images.

use alloc::string::{String, ToString};
use x86_64::instructions::port::Port;

/// Bytes per ATA sector.
pub const SECTOR_SIZE: usize = 512;
/// Highest LBA addressable with 28-bit commands.
const LBA28_MAX: u32 = 0x0FFF_FFFF;

// Primary-bus register addresses.
const REG_DATA: u16 = 0x1F0;
const REG_ERROR: u16 = 0x1F1;
const REG_SECCOUNT: u16 = 0x1F2;
const REG_LBA_LO: u16 = 0x1F3;
const REG_LBA_MID: u16 = 0x1F4;
const REG_LBA_HI: u16 = 0x1F5;
const REG_DRIVE: u16 = 0x1F6;
const REG_CMD: u16 = 0x1F7; // command (write) / status (read)
const REG_CTRL: u16 = 0x3F6; // device control (write) / alt status (read)

const CMD_IDENTIFY: u8 = 0xEC;
const CMD_READ: u8 = 0x20;
const CMD_WRITE: u8 = 0x30;
const CMD_FLUSH: u8 = 0xE7;

// Status register bits.
const ST_BSY: u8 = 0x80;
const ST_DF: u8 = 0x20;
const ST_DRQ: u8 = 0x08;
const ST_ERR: u8 = 0x01;

/// Bound for polling loops so a wedged controller can't hang the kernel.
const POLL_LIMIT: u32 = 4_000_000;

#[derive(Debug, Clone, Copy)]
pub enum AtaError {
    NoDrive,
    CommandFailed,
    Timeout,
    OutOfRange,
}

/// What IDENTIFY DEVICE told us about the disk.
pub struct DriveInfo {
    /// Total user-addressable sectors (LBA48 if supported, else LBA28).
    pub total_sectors: u64,
    /// Device model string, trimmed.
    pub model: String,
}

pub struct AtaDrive {
    data: Port<u16>,
    seccount: Port<u8>,
    lba_lo: Port<u8>,
    lba_mid: Port<u8>,
    lba_hi: Port<u8>,
    drive: Port<u8>,
    cmd: Port<u8>,
    ctrl: Port<u8>,
}

impl AtaDrive {
    /// Probe the primary bus master. Returns the drive handle + info.
    pub fn new_primary_master() -> Result<(Self, DriveInfo), AtaError> {
        let mut d = Self {
            data: Port::new(REG_DATA),
            seccount: Port::new(REG_SECCOUNT),
            lba_lo: Port::new(REG_LBA_LO),
            lba_mid: Port::new(REG_LBA_MID),
            lba_hi: Port::new(REG_LBA_HI),
            drive: Port::new(REG_DRIVE),
            cmd: Port::new(REG_CMD),
            ctrl: Port::new(REG_CTRL),
        };

        unsafe {
            d.ctrl.write(0x02); // nIEN: mask drive interrupts (we poll)
            d.drive.write(0xA0); // select master, LBA mode
        }
        d.settle();
        d.wait_not_busy()?;

        // IDENTIFY DEVICE.
        unsafe { d.cmd.write(CMD_IDENTIFY) };
        d.settle();
        if d.status() == 0 {
            return Err(AtaError::NoDrive);
        }
        d.wait_not_busy()?;
        if d.status() & (ST_ERR | ST_DF) != 0 {
            // e.g. an ATAPI device aborting IDENTIFY — not our disk.
            return Err(AtaError::NoDrive);
        }
        d.wait_data_ready()?;

        let mut id = [0u16; 256];
        for w in id.iter_mut() {
            *w = unsafe { d.data.read() };
        }

        let lba28 = u32::from(id[60]) | (u32::from(id[61]) << 16);
        let lba48_supported = id[83] & (1u16 << 10) != 0;
        let lba48 = u64::from(id[100])
            | (u64::from(id[101]) << 16)
            | (u64::from(id[102]) << 32)
            | (u64::from(id[103]) << 48);
        let total =
            if lba48_supported && lba48 > 0 { lba48 } else { u64::from(lba28) };

        // Model name: words 27..47, big-endian bytes per word.
        let mut model = String::new();
        for w in &id[27..47] {
            model.push((w >> 8) as u8 as char);
            model.push((*w & 0xFF) as u8 as char);
        }

        Ok((
            d,
            DriveInfo {
                total_sectors: total,
                model: model.trim().to_string(),
            },
        ))
    }

    /// Cached PIO read of `count` sectors starting at `lba` (A5: routes every
    /// FS read through the block cache).
    pub fn read_sectors(&mut self, lba: u32, count: u8, buf: &mut [u8]) -> Result<(), AtaError> {
        crate::blkcache::read(self, lba, count, buf)
    }

    /// Cached (write-through) PIO write of `count` sectors, then a flush.
    pub fn write_sectors(&mut self, lba: u32, count: u8, buf: &[u8]) -> Result<(), AtaError> {
        crate::blkcache::write(self, lba, count, buf)
    }

    /// Uncached PIO read — used by boot-time verification and the cache
    /// internals (must NOT recurse back into the cache).
    pub(crate) fn read_sectors_raw(
        &mut self,
        lba: u32,
        count: u8,
        buf: &mut [u8],
    ) -> Result<(), AtaError> {
        assert!(count > 0);
        assert!(buf.len() >= count as usize * SECTOR_SIZE);
        if u64::from(lba) + u64::from(count) - 1 > u64::from(LBA28_MAX) {
            return Err(AtaError::OutOfRange);
        }
        self.prepare_lba28(lba, count)?;
        unsafe { self.cmd.write(CMD_READ) };
        for s in 0..count as usize {
            self.wait_data_ready()?;
            let base = s * SECTOR_SIZE;
            for i in 0..256 {
                let w = unsafe { self.data.read() };
                buf[base + i * 2] = w as u8;
                buf[base + i * 2 + 1] = (w >> 8) as u8;
            }
        }
        Ok(())
    }

    /// Uncached PIO write — used by the cache write path (must NOT recurse).
    pub(crate) fn write_sectors_raw(
        &mut self,
        lba: u32,
        count: u8,
        buf: &[u8],
    ) -> Result<(), AtaError> {
        assert!(count > 0);
        assert!(buf.len() >= count as usize * SECTOR_SIZE);
        if u64::from(lba) + u64::from(count) - 1 > u64::from(LBA28_MAX) {
            return Err(AtaError::OutOfRange);
        }
        self.prepare_lba28(lba, count)?;
        unsafe { self.cmd.write(CMD_WRITE) };
        for s in 0..count as usize {
            self.wait_data_ready()?;
            let base = s * SECTOR_SIZE;
            for i in 0..256 {
                let w = u16::from(buf[base + i * 2]) | (u16::from(buf[base + i * 2 + 1]) << 8);
                unsafe { self.data.write(w) };
            }
            // The drive raises BSY while moving the words into its buffer.
            self.wait_not_busy()?;
        }
        unsafe { self.cmd.write(CMD_FLUSH) };
        self.wait_not_busy()?;
        Ok(())
    }

    /// Load the task-file registers for an LBA28 transfer.
    fn prepare_lba28(&mut self, lba: u32, count: u8) -> Result<(), AtaError> {
        self.wait_not_busy()?;
        unsafe {
            self.drive.write(0xE0 | ((lba >> 24) & 0x0F) as u8);
            self.seccount.write(count);
            self.lba_lo.write(lba as u8);
            self.lba_mid.write((lba >> 8) as u8);
            self.lba_hi.write((lba >> 16) as u8);
        }
        Ok(())
    }

    fn status(&mut self) -> u8 {
        unsafe { self.cmd.read() }
    }

    /// ~400 ns settle: four reads of the alternate-status register.
    fn settle(&mut self) {
        for _ in 0..4 {
            let _ = unsafe { self.ctrl.read() };
        }
    }

    fn wait_not_busy(&mut self) -> Result<(), AtaError> {
        for _ in 0..POLL_LIMIT {
            if unsafe { self.ctrl.read() } & ST_BSY == 0 {
                return Ok(());
            }
        }
        Err(AtaError::Timeout)
    }

    fn wait_data_ready(&mut self) -> Result<(), AtaError> {
        for _ in 0..POLL_LIMIT {
            let st = unsafe { self.ctrl.read() };
            if st & (ST_ERR | ST_DF) != 0 {
                return Err(AtaError::CommandFailed);
            }
            if st & ST_BSY == 0 && st & ST_DRQ != 0 {
                return Ok(());
            }
        }
        Err(AtaError::Timeout)
    }
}