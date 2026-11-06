#![no_std]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { loop {} }
#[inline(always)]
fn scaled(x: u32) -> u32 { x.wrapping_mul(7) }
#[inline(never)]
fn calculate(x: u32) -> u32 {
    scaled(x).wrapping_add(11) // controlled edit: 11 -> 12
}
#[no_mangle]
pub extern "C" fn entry(x: u32) -> u32 { calculate(x) }
