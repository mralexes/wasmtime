//! System V ABI unwind information.

use alloc::vec::Vec;
use byteorder::{ByteOrder, LittleEndian};
#[cfg(feature = "enable-serde")]
use serde::{Deserialize, Serialize};

/// Maximum (inclusive) size of a "small" stack allocation
const SMALL_ALLOC_MAX_SIZE: u32 = 128;
/// Maximum (inclusive) size of a "large" stack allocation that can represented in 16-bits
const LARGE_ALLOC_16BIT_MAX_SIZE: u32 = 524280;

struct Writer<'a> {
    buf: &'a mut [u8],
    offset: usize,
}

impl<'a> Writer<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, offset: 0 }
    }

    fn write_u8(&mut self, v: u8) {
        self.buf[self.offset] = v;
        self.offset += 1;
    }

    fn write_u16<T: ByteOrder>(&mut self, v: u16) {
        T::write_u16(&mut self.buf[self.offset..(self.offset + 2)], v);
        self.offset += 2;
    }

    fn write_u32<T: ByteOrder>(&mut self, v: u32) {
        T::write_u32(&mut self.buf[self.offset..(self.offset + 4)], v);
        self.offset += 4;
    }
}

/// The supported unwind codes for the x64 Windows ABI.
///
/// See: https://docs.microsoft.com/en-us/cpp/build/exception-handling-x64
/// Only what is needed to describe the prologues generated by the Cranelift x86 ISA are represented here.
/// Note: the Cranelift x86 ISA RU enum matches the Windows unwind GPR encoding values.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "enable-serde", derive(Serialize, Deserialize))]
pub(crate) enum UnwindCode {
    PushRegister {
        offset: u8,
        reg: u8,
    },
    SaveXmm {
        offset: u8,
        reg: u8,
        stack_offset: u32,
    },
    StackAlloc {
        offset: u8,
        size: u32,
    },
}

impl UnwindCode {
    fn emit(&self, writer: &mut Writer) {
        enum UnwindOperation {
            PushNonvolatileRegister = 0,
            LargeStackAlloc = 1,
            SmallStackAlloc = 2,
            SaveXmm128 = 8,
            SaveXmm128Far = 9,
        }

        match self {
            Self::PushRegister { offset, reg } => {
                writer.write_u8(*offset);
                writer.write_u8((*reg << 4) | (UnwindOperation::PushNonvolatileRegister as u8));
            }
            Self::SaveXmm {
                offset,
                reg,
                stack_offset,
            } => {
                writer.write_u8(*offset);
                let stack_offset = stack_offset / 16;
                if stack_offset <= core::u16::MAX as u32 {
                    writer.write_u8((*reg << 4) | (UnwindOperation::SaveXmm128 as u8));
                    writer.write_u16::<LittleEndian>(stack_offset as u16);
                } else {
                    writer.write_u8((*reg << 4) | (UnwindOperation::SaveXmm128Far as u8));
                    writer.write_u16::<LittleEndian>(stack_offset as u16);
                    writer.write_u16::<LittleEndian>((stack_offset >> 16) as u16);
                }
            }
            Self::StackAlloc { offset, size } => {
                // Stack allocations on Windows must be a multiple of 8 and be at least 1 slot
                assert!(*size >= 8);
                assert!((*size % 8) == 0);

                writer.write_u8(*offset);
                if *size <= SMALL_ALLOC_MAX_SIZE {
                    writer.write_u8(
                        ((((*size - 8) / 8) as u8) << 4) | UnwindOperation::SmallStackAlloc as u8,
                    );
                } else if *size <= LARGE_ALLOC_16BIT_MAX_SIZE {
                    writer.write_u8(UnwindOperation::LargeStackAlloc as u8);
                    writer.write_u16::<LittleEndian>((*size / 8) as u16);
                } else {
                    writer.write_u8((1 << 4) | (UnwindOperation::LargeStackAlloc as u8));
                    writer.write_u32::<LittleEndian>(*size);
                }
            }
        };
    }

    fn node_count(&self) -> usize {
        match self {
            Self::StackAlloc { size, .. } => {
                if *size <= SMALL_ALLOC_MAX_SIZE {
                    1
                } else if *size <= LARGE_ALLOC_16BIT_MAX_SIZE {
                    2
                } else {
                    3
                }
            }
            Self::SaveXmm { stack_offset, .. } => {
                if *stack_offset <= core::u16::MAX as u32 {
                    2
                } else {
                    3
                }
            }
            _ => 1,
        }
    }
}

/// Represents Windows x64 unwind information.
///
/// For information about Windows x64 unwind info, see:
/// https://docs.microsoft.com/en-us/cpp/build/exception-handling-x64
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "enable-serde", derive(Serialize, Deserialize))]
pub struct UnwindInfo {
    pub(crate) flags: u8,
    pub(crate) prologue_size: u8,
    pub(crate) frame_register: Option<u8>,
    pub(crate) frame_register_offset: u8,
    pub(crate) unwind_codes: Vec<UnwindCode>,
}

impl UnwindInfo {
    /// Gets the emit size of the unwind information, in bytes.
    pub fn emit_size(&self) -> usize {
        let node_count = self.node_count();

        // Calculation of the size requires no SEH handler or chained info
        assert!(self.flags == 0);

        // Size of fixed part of UNWIND_INFO is 4 bytes
        // Then comes the UNWIND_CODE nodes (2 bytes each)
        // Then comes 2 bytes of padding for the unwind codes if necessary
        // Next would come the SEH data, but we assert above that the function doesn't have SEH data

        4 + (node_count * 2) + if (node_count & 1) == 1 { 2 } else { 0 }
    }

    /// Emits the unwind information into the given mutable byte slice.
    ///
    /// This function will panic if the slice is not at least `emit_size` in length.
    pub fn emit(&self, buf: &mut [u8]) {
        const UNWIND_INFO_VERSION: u8 = 1;

        let node_count = self.node_count();
        assert!(node_count <= 256);

        let mut writer = Writer::new(buf);

        writer.write_u8((self.flags << 3) | UNWIND_INFO_VERSION);
        writer.write_u8(self.prologue_size);
        writer.write_u8(node_count as u8);

        if let Some(reg) = self.frame_register {
            writer.write_u8((self.frame_register_offset << 4) | reg);
        } else {
            writer.write_u8(0);
        }

        // Unwind codes are written in reverse order (prologue offset descending)
        for code in self.unwind_codes.iter().rev() {
            code.emit(&mut writer);
        }

        // To keep a 32-bit alignment, emit 2 bytes of padding if there's an odd number of 16-bit nodes
        if (node_count & 1) == 1 {
            writer.write_u16::<LittleEndian>(0);
        }

        // Ensure the correct number of bytes was emitted
        assert_eq!(writer.offset, self.emit_size());
    }

    fn node_count(&self) -> usize {
        self.unwind_codes
            .iter()
            .fold(0, |nodes, c| nodes + c.node_count())
    }
}
