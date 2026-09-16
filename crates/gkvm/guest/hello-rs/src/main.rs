//! hello-rs: byte-for-byte the same guest behavior as hello.c, from the
//! other toolchain (rustc + rust-lld instead of riscv64 gcc + GNU ld). The
//! runner's fixture test asserts both ELFs produce identical output, which
//! is what collapses the doc's "linker/entry ABI mismatch" risk into a
//! checked property.
#![no_std]
#![no_main]

use core::arch::{asm, global_asm};

const SP1_HALT: u64 = 0x00;
const SP1_WRITE: u64 = 0x02;
const SP1_HINT_LEN: u64 = 0xF0;
const SP1_HINT_READ: u64 = 0xF1;
// SP1 v6 offsets its special fds by LOWEST_ALLOWED_FD = 10 (see
// sp1-primitives consts::fd): public values ride fd 3 + 10.
const FD_PUBLIC_VALUES: u64 = 13;

const INPUT_CAP: usize = 131_072;

global_asm!(
    r#"
    .section .text._start
    .globl _start
_start:
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop
    li sp, 0x78000000
    call __gk_start
"#
);

fn hint_len() -> u64 {
    let len: u64;
    unsafe {
        asm!("ecall", in("t0") SP1_HINT_LEN, lateout("t0") len, options(nostack));
    }
    len
}

/// Read the next hint buffer. `ptr` must be 8-aligned with room for the
/// word-granular spill (len rounded up to 8, plus 8 when already a multiple).
unsafe fn hint_read(ptr: *mut u8, len: u64) {
    unsafe {
        asm!(
            "ecall",
            in("t0") SP1_HINT_READ,
            in("a0") ptr,
            in("a1") len,
            options(nostack),
        );
    }
}

fn write_public(bytes: &[u8]) {
    unsafe {
        asm!(
            "ecall",
            in("t0") SP1_WRITE,
            in("a0") FD_PUBLIC_VALUES,
            in("a1") bytes.as_ptr(),
            in("a2") bytes.len() as u64,
            options(nostack),
        );
    }
}

fn halt(code: u32) -> ! {
    unsafe {
        asm!("ecall", in("t0") SP1_HALT, in("a0") code as u64, options(nostack, noreturn));
    }
}

#[repr(align(8))]
struct Aligned<const N: usize>([u8; N]);

static mut ROOT: Aligned<40> = Aligned([0; 40]);
static mut INPUT: Aligned<{ INPUT_CAP + 8 }> = Aligned([0; INPUT_CAP + 8]);

#[unsafe(no_mangle)]
extern "C" fn __gk_start() -> ! {
    unsafe {
        // GKVM ABI v1 framing: buffer 0 is the 32-byte artifact root.
        if hint_len() != 32 {
            halt(0xFA);
        }
        hint_read(&raw mut ROOT.0 as *mut u8, 32);

        let len = hint_len();
        if len > INPUT_CAP as u64 {
            halt(0xFA);
        }
        let input = &raw mut INPUT.0 as *mut u8;
        hint_read(input, len);

        write_public(b"GKVM-HELLO-V1\n");
        let payload = core::slice::from_raw_parts_mut(input, len as usize);
        payload.reverse();
        write_public(payload);
    }
    halt(0)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    halt(0xFB)
}
