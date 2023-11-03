use crate::{
    io::outb,
    video::vesa::TEXT_BUFFER,
    x86::idt::{GateDescriptor, GateType, SegmentSelector, Table},
};

#[cfg(feature = "alloc")]
#[fzproc_macros::interrupt_descriptor_table(0x8)]
pub mod handlers;

// todo: restore locks afterwards
unsafe fn release_locks() {
    if let Some(text_buffer) = TEXT_BUFFER.get() {
        text_buffer.buffer.force_unlock();
    }
}

#[no_mangle]
pub unsafe fn _int_entry() {
    release_locks();
}

#[no_mangle]
pub fn _pic_eoi() {
    outb(0x20, 0x20);
    outb(0xA0, 0x20);
}
