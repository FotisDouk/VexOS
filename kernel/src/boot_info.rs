pub const MAGIC: u64 = u64::from_le_bytes(*b"BOOTINFO");

#[repr(C)]
pub struct BootInfo {
    pub magic: u64,
    pub mmap_addr: u64,
    pub mmap_count: u32,
    pub boot_drive: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MemoryRegion {
    pub base: u64,
    pub length: u64,
    pub kind: u32,
    pub acpi_attributes: u32,
}

impl BootInfo {
    pub unsafe fn from_ptr(ptr: *const BootInfo) -> Option<&'static BootInfo> {
        let info = unsafe { ptr.as_ref()? };
        (info.magic == MAGIC).then_some(info)
    }

    pub fn memory_map(&self) -> &'static [MemoryRegion] {
        unsafe {
            core::slice::from_raw_parts(
                self.mmap_addr as *const MemoryRegion,
                self.mmap_count as usize,
            )
        }
    }
}

impl MemoryRegion {
    pub fn is_usable(&self) -> bool {
        self.kind == 1
    }

    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            1 => "usable",
            2 => "reserved",
            3 => "ACPI reclaimable",
            4 => "ACPI NVS",
            5 => "bad memory",
            _ => "unknown",
        }
    }
}
