#![no_std]
#![no_main]

use x86_64::instructions::hlt;

bootloader_api::entry_point!(kernel_main, config = &CONFIG);

const CONFIG: bootloader_api::BootloaderConfig = {
    let mut c = bootloader_api::BootloaderConfig::new_default();
    c
};

fn kernel_main(_boot_info: &'static mut bootloader_api::BootInfo) -> ! {
    loop { hlt(); }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
