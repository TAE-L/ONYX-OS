//! M5: ATA PIO driver for the primary IDE controller.
//!
//! QEMU's `-drive format=raw,file=...` attaches the image as a plain IDE hard
//! disk on the primary bus, so the classic port-I/O registers at 0x1F0..0x1F7
//! (+ control 0x3F6) talk to it. We poll the status register instead of using
//! IRQ14 (the PIC keeps it masked anyway) and set nIEN so the drive cannot
//! fire interrupts behind our back.
//!
//! P16-1.1: LBA48. The driver used to be LBA28-only, which capped the address
//! space at 128 GiB *silently* — a bigger disk would simply have had its upper
//! half unreachable, with no error at format time. LBAs are now `u64`
//! end-to-end and the EXT commands are used when the drive advertises them,
//! because a disk that reports a 48-bit capacity but can only be addressed
//! through 28 bits is worse than one that never claimed the capacity.
//!
//! Note the drive still only does PIO; LBA48 is about the address width, not
//! the transfer mechanism. Replacing PIO is Phase 3.5 (virtio-blk).

use alloc::string::{String, ToString};
use x86_64::instructions::port::Port;

/// Bytes per ATA sector.
pub const SECTOR_SIZE: usize = 512;
/// Highest LBA addressable with 28-bit commands (128 GiB).
const LBA28_MAX: u64 = 0x0FFF_FFFF;
/// Highest LBA addressable with 48-bit commands (~256 TiB).
pub const LBA48_MAX: u64 = 0x0000_FFFF_FFFF_FFFF;

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
/// P16-1.1: 48-bit read/write, and the task-file handshake that sets the high
/// LBA bytes (written once BEFORE the low bytes, which is why this is not a
/// register rename but a second command in the sequence).
const CMD_READ_EXT: u8 = 0x25;
const CMD_WRITE_EXT: u8 = 0x35;
const CMD_SET_HIGH_LBA: u8 = 0x91;

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
    /// P16-1.1: IDENTIFY word 83 bit 10. When false the drive can only be
    /// addressed through 28 bits, so the driver refuses an LBA above
    /// `LBA28_MAX` rather than silently truncating it — a silent truncation
    /// would write sector 0 of the disk when asked for sector 2^32.
    lba48: bool,
    /// P16-1.1: total user-addressable sectors, so the upper bound is checked
    /// against what the drive actually claims rather than a constant.
    total_sectors: u64,
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
            lba48: false,
            total_sectors: 0,
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

        // P16-1.1: remember both facts the transfer path needs. `total_sectors`
        // is stored on the drive so the range check uses the drive's OWN claimed
        // capacity; a constant would be wrong for a small image (and the test
        // image is only tens of MiB).
        d.lba48 = lba48_supported;
        d.total_sectors = total;

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
    /// P16-1.1: `u64` LBA all the way down, so a partition above 128 GiB is
    /// reachable instead of being truncated to a low address.
    pub fn read_sectors(&mut self, lba: u64, count: u8, buf: &mut [u8]) -> Result<(), AtaError> {
        crate::blkcache::read(self, lba, count, buf)
    }

    /// Cached (write-through) PIO write of `count` sectors, then a flush.
    pub fn write_sectors(&mut self, lba: u64, count: u8, buf: &[u8]) -> Result<(), AtaError> {
        crate::blkcache::write(self, lba, count, buf)
    }

    /// Uncached PIO read — used by boot-time verification and the cache
    /// internals (must NOT recurse back into the cache).
    pub(crate) fn read_sectors_raw(
        &mut self,
        lba: u64,
        count: u8,
        buf: &mut [u8],
    ) -> Result<(), AtaError> {
        assert!(count > 0);
        assert!(buf.len() >= count as usize * SECTOR_SIZE);
        let (read_cmd, _) = self.prepare_lba(lba, count)?;
        unsafe { self.cmd.write(read_cmd) };
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
        lba: u64,
        count: u8,
        buf: &[u8],
    ) -> Result<(), AtaError> {
        assert!(count > 0);
        assert!(buf.len() >= count as usize * SECTOR_SIZE);
        let (_, write_cmd) = self.prepare_lba(lba, count)?;
        unsafe { self.cmd.write(write_cmd) };
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

    /// Validate `lba` and program the task file, returning `(read_cmd,
    /// write_cmd)` for this transfer's address width.
    ///
    /// P16-1.1: LBA48 needs the high bytes written FIRST, under a separate
    /// `SET HIGH LBA` command, and the sector count re-written afterwards —
    /// the CPU clears SECCOUNT when the high-LBA command is issued. That ordering
    /// is the whole reason this is a separate function rather than a register
    /// rename.
    fn prepare_lba(&mut self, lba: u64, count: u8) -> Result<(u8, u8), AtaError> {
        let last = lba
            .checked_add(u64::from(count))
            .and_then(|v| v.checked_sub(1))
            .ok_or(AtaError::OutOfRange)?;
        if last >= self.total_sectors {
            return Err(AtaError::OutOfRange);
        }

        // P16-1.1, lesson learned the hard way: take the 48-bit path ONLY when
        // the address actually exceeds 28 bits.
        //
        // An earlier version here used LBA48 whenever the drive advertised it,
        // which sent `SET HIGH LBA` + `CMD_READ_EXT` even for LBA 0 on a 40 MiB
        // disk — and the whole filesystem stack stopped finding its partition
        // table. Two reasons not to: the EXT sequence is a second, less-exercised
        // code path that adds risk for zero benefit below 128 GiB, and it makes a
        // bug in that path affect every disk instead of only large ones.
        if last <= LBA28_MAX {
            self.prepare_lba28(lba as u32, count)?;
            return Ok((CMD_READ, CMD_WRITE));
        }
        if !self.lba48 {
            // The drive cannot reach this sector. Refuse rather than truncate:
            // a silent truncation would write sector 0 when asked for sector 2^32.
            return Err(AtaError::OutOfRange);
        }

        self.wait_not_busy()?;
        unsafe {
            // High half (ATA-6 register layout, NOT "byte 16 then byte 32"):
            //   LBA_LO  <- LBA[39:32]      LBA_MID <- LBA[47:40]
            //   LBA_HI  <- LBA[51:48] (low 4 bits; the rest are reserved)
            // The first version used (lba>>32)/(lba>>40)/(lba>>44) here, which is
            // the whole 48-bit value split as if the field were 44 bits wide.
            self.seccount.write(count);
            self.lba_lo.write((lba >> 32) as u8);
            self.lba_mid.write((lba >> 40) as u8);
            self.lba_hi.write(((lba >> 48) & 0x0F) as u8);
            self.cmd.write(CMD_SET_HIGH_LBA);
        }
        self.wait_not_busy()?;
        unsafe {
            // Low half. Device/head bits 3:0 carry LBA[44:40] (NOT LBA[24:28]
            // as the 28-bit path does) — the register means different things in
            // the two modes, which is the second half of the earlier mistake.
            self.drive.write(0xE0 | ((lba >> 40) & 0x0F) as u8);
            // SECCOUNT must be rewritten: `SET HIGH LBA` clears it.
            self.seccount.write(count);
            self.lba_lo.write(lba as u8);
            self.lba_mid.write((lba >> 8) as u8);
            self.lba_hi.write((lba >> 16) as u8);
        }
        Ok((CMD_READ_EXT, CMD_WRITE_EXT))
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